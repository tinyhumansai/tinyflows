//! Resumable owner migration execution and progress records.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tinystoragedrivers_api::{DocumentStore, DocumentStoreExt};
use tinystoragedrivers_types::{
    CollectionSpec, ErrorKind, Filter, ID_FIELD, Precondition, Query, Result, Sort, StorageError,
    WriteOptions,
};

use super::{MIGRATIONS_COLLECTION, Schema, SourceRecord, Step, TargetRecord};
use crate::Key;

const PAGE_SIZE: usize = 100;

impl Schema {
    /// Ensure this owner’s collections, then migrate its data in place.
    ///
    /// The shared `tsd.migrations` collection records one progress row per
    /// owner, item and step. Backends are supplied by the caller; this method
    /// never constructs an engine or an in-memory fallback.
    pub async fn migrate(
        &self,
        docs: &Arc<dyn DocumentStore>,
        options: &MigrateOptions,
    ) -> Result<MigrationReport> {
        if options.batch_size == 0 {
            return Err(StorageError::invalid_input(
                "migration batch size must be at least 1",
            ));
        }
        for spec in self.collections() {
            docs.ensure_collection(&spec).await?;
        }
        let migrations = Migrations {
            docs: Arc::clone(docs),
        };
        let mut report = MigrationReport::default();
        for step in &self.steps {
            let progress_key = Key::of([self.owner.as_str(), step.item, step.id])?
                .as_str()
                .to_owned();
            let progress = migrations.load(&progress_key).await?;
            if progress.as_ref().is_some_and(|p| p.done) {
                report.already_done += 1;
                continue;
            }
            let source = step.source.unwrap_or(step.item);
            if source != step.item {
                docs.ensure_collection(&CollectionSpec::new(source)).await?;
            }
            let mut query = Query::filter(Filter::default())
                .sort(Sort::asc(ID_FIELD))
                .limit(options.batch_size);
            if let Some(cursor) = progress.as_ref().and_then(|p| p.cursor.clone()) {
                query = query.after(tinystoragedrivers_types::Cursor(cursor));
            }
            let mut written = progress.as_ref().map_or(0, |saved| saved.written);
            loop {
                let page = docs.query(source, &query).await?;
                if page.items.is_empty() {
                    break;
                }
                let last_cursor = page.next.clone();
                let inputs = page
                    .items
                    .iter()
                    .filter(|row| source != step.item || row.meta.schema < step.to)
                    .map(|row| SourceRecord {
                        id: row.id.clone(),
                        body: row.doc.clone(),
                        meta: row.meta,
                    })
                    .collect();
                let outputs = (step.run)(inputs)?;
                let output_ids: std::collections::HashSet<(String, String)> = outputs
                    .iter()
                    .map(|r| (r.collection.clone(), r.id.clone()))
                    .collect();
                for target in outputs {
                    if !target.body.is_object() {
                        return Err(StorageError::invalid_input(
                            "migration output must be an object",
                        ));
                    }
                    let output_schema = self.target_schema(step, &target)?;
                    let origin_row = page.items.iter().find(|row| row.id == target.id);
                    let same_row = (source == step.item && target.collection == step.item)
                        .then_some(origin_row)
                        .flatten();
                    let precondition = same_row.map_or(Precondition::Absent, |row| {
                        Precondition::Version(row.version)
                    });
                    let mut options = WriteOptions::new();
                    options = options.schema(output_schema);
                    let expected_expiry = target
                        .expires_ms
                        .or_else(|| origin_row.and_then(|row| row.meta.expires_ms));
                    if let Some(expires_ms) = expected_expiry {
                        options = options.expires_ms(expires_ms);
                    }
                    match docs
                        .put_with(
                            &target.collection,
                            &target.id,
                            target.body.clone(),
                            precondition,
                            options,
                        )
                        .await
                    {
                        Ok(_) => written += 1,
                        Err(error)
                            if matches!(
                                error.kind(),
                                ErrorKind::Conflict | ErrorKind::AlreadyExists
                            ) =>
                        {
                            let existing = docs.get(&target.collection, &target.id).await?;
                            let identical = existing.is_some_and(|row| {
                                row.doc == target.body
                                    && row.meta.schema == output_schema
                                    && same_row.is_none_or(|source| {
                                        row.meta.created_ms == source.meta.created_ms
                                    })
                                    && row.meta.expires_ms == expected_expiry
                            });
                            if !identical {
                                return Err(error);
                            }
                            written += 1;
                        }
                        Err(error) => return Err(error),
                    }
                }
                for row in &page.items {
                    if source != step.item
                        || (row.meta.schema < step.to
                            && !output_ids.contains(&(step.item.to_owned(), row.id.clone())))
                    {
                        match docs
                            .delete(source, &row.id, Precondition::Version(row.version))
                            .await
                        {
                            Err(error)
                                if error.kind() == ErrorKind::Conflict
                                    && docs.get(source, &row.id).await?.is_none() => {}
                            other => {
                                other?;
                            }
                        }
                    }
                }
                let cursor = last_cursor.map(|cursor| cursor.0);
                migrations
                    .save(
                        &progress_key,
                        &Progress {
                            owner: self.owner.as_str().to_owned(),
                            step: step.id.to_owned(),
                            from: step.from,
                            to: step.to,
                            cursor,
                            written,
                            done: false,
                        },
                    )
                    .await?;
                if let Some(next) = page.next {
                    query = query.after(next);
                } else {
                    break;
                }
            }
            migrations
                .save(
                    &progress_key,
                    &Progress {
                        owner: self.owner.as_str().to_owned(),
                        step: step.id.to_owned(),
                        from: step.from,
                        to: step.to,
                        cursor: None,
                        written,
                        done: true,
                    },
                )
                .await?;
            report.ran.push(StepReport {
                owner: self.owner.as_str().to_owned(),
                item: step.item.to_owned(),
                step: step.id.to_owned(),
                written,
            });
        }
        Ok(report)
    }

    fn target_schema(&self, step: &Step, target: &TargetRecord) -> Result<u32> {
        let owned = target
            .collection
            .strip_prefix(self.owner.as_str())
            .is_some_and(|rest| rest.starts_with('.'));
        let version = self
            .items
            .iter()
            .find(|(spec, _, _)| spec.name == target.collection)
            .map(|(_, version, _)| *version)
            .filter(|_| owned)
            .ok_or_else(|| {
                StorageError::invalid_input(
                    "migration output targets an undeclared owner collection",
                )
            })?;
        Ok(if target.collection == step.item {
            step.to
        } else {
            version
        })
    }
}

/// Batch size for owner migration runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrateOptions {
    batch_size: usize,
}
impl MigrateOptions {
    /// Defaults to 100 documents per batch.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            batch_size: PAGE_SIZE,
        }
    }
    /// Set the number of source documents read per batch.
    #[must_use]
    pub const fn batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size;
        self
    }
}
impl Default for MigrateOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary of an owner migration run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Steps completed by this invocation.
    pub ran: Vec<StepReport>,
    /// Steps already marked complete.
    pub already_done: usize,
}

/// A completed migration step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    /// Owner name.
    pub owner: String,
    /// Collection being migrated.
    pub item: String,
    /// Stable step identifier.
    pub step: String,
    /// Number of destination records written.
    pub written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Progress {
    owner: String,
    step: String,
    from: u32,
    to: u32,
    cursor: Option<String>,
    written: u64,
    done: bool,
}

/// Per-owner migration completion markers.
#[derive(Debug, Clone)]
pub struct Migrations {
    docs: Arc<dyn DocumentStore>,
}
impl Migrations {
    /// Bind markers to the caller-provided document store and owner.
    #[must_use]
    pub fn new(docs: &Arc<dyn DocumentStore>, _owner: &str) -> Self {
        Self {
            docs: Arc::clone(docs),
        }
    }

    /// Whether a step has completed.
    pub async fn done(&self, owner: &str, step: &str) -> Result<bool> {
        self.ensure().await?;
        let id = Key::of([owner, step])?.as_str().to_owned();
        Ok(self
            .docs
            .get_as::<Progress>(MIGRATIONS_COLLECTION, &id)
            .await?
            .is_some_and(|row| row.doc.done))
    }

    /// Mark an external import step complete.
    pub async fn mark_done(&self, owner: &str, step: &str) -> Result<()> {
        self.ensure().await?;
        let id = Key::of([owner, step])?.as_str().to_owned();
        self.save(
            &id,
            &Progress {
                owner: owner.to_owned(),
                step: step.to_owned(),
                from: 0,
                to: 0,
                cursor: None,
                written: 0,
                done: true,
            },
        )
        .await
    }

    async fn load(&self, id: &str) -> Result<Option<Progress>> {
        Ok(self
            .docs
            .get_as::<Progress>(MIGRATIONS_COLLECTION, id)
            .await?
            .map(|row| row.doc))
    }

    async fn ensure(&self) -> Result<()> {
        self.docs
            .ensure_collection(&CollectionSpec::new(MIGRATIONS_COLLECTION))
            .await
    }

    async fn save(&self, id: &str, progress: &Progress) -> Result<()> {
        self.docs
            .put_as(MIGRATIONS_COLLECTION, id, progress, Precondition::None)
            .await
            .map(|_| ())
    }
}

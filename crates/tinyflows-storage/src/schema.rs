//! TinyFlows-owned collection declarations and resumable owner migrations.

use std::sync::Arc;

use serde_json::Value;
use tinystoragedrivers_types::{CollectionSpec, Meta, Result, StorageError};

pub(super) const MIGRATIONS_COLLECTION: &str = "tsd.migrations";
mod migrations;
pub use migrations::{MigrateOptions, MigrationReport, Migrations, StepReport};

/// Which product scope owns a declared collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeClass {
    /// Tenant or local user data.
    Tenant,
}

/// A validated TinyFlows storage owner name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Owner(String);

impl Owner {
    /// Validate an owner name.
    pub fn new(name: &str) -> Result<Self> {
        let valid = (2..=24).contains(&name.len())
            && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid || name == "tsd" {
            return Err(StorageError::invalid_input("invalid storage owner name"));
        }
        Ok(Self(name.to_owned()))
    }

    /// Owner name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A source document passed to a migration step.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRecord {
    /// Existing source id.
    pub id: String,
    /// Existing document body.
    pub body: Value,
    /// Existing storage metadata.
    pub meta: Meta,
}

/// A document emitted by a migration step.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetRecord {
    /// Destination collection.
    pub collection: String,
    /// Destination id.
    pub id: String,
    /// Destination body.
    pub body: Value,
    /// Absolute expiry to preserve or assign to the destination record.
    pub expires_ms: Option<i64>,
}

impl TargetRecord {
    /// Create a migrated document.
    #[must_use]
    pub fn new(collection: impl Into<String>, id: impl Into<String>, body: Value) -> Self {
        Self {
            collection: collection.into(),
            id: id.into(),
            body,
            expires_ms: None,
        }
    }

    /// Set the destination's absolute expiry timestamp.
    #[must_use]
    pub fn expires_ms(mut self, value: i64) -> Self {
        self.expires_ms = Some(value);
        self
    }

    /// Preserve the source record's absolute expiry when the migration
    /// changes the destination id or collection.
    #[must_use]
    pub fn preserve_expiry_from(mut self, source: &SourceRecord) -> Self {
        self.expires_ms = source.meta.expires_ms;
        self
    }
}

type StepFn = dyn Fn(Vec<SourceRecord>) -> Result<Vec<TargetRecord>> + Send + Sync;

/// One document-collection migration step.
#[derive(Clone)]
pub struct Step {
    /// Stable step id.
    pub id: &'static str,
    /// Destination collection.
    pub item: &'static str,
    /// Source schema version.
    pub from: u32,
    /// Destination schema version.
    pub to: u32,
    /// Legacy source collection, when different from `item`.
    pub source: Option<&'static str>,
    run: Arc<StepFn>,
}

impl Step {
    /// Define a document migration step.
    #[must_use]
    pub fn new(
        id: &'static str,
        item: &'static str,
        from: u32,
        to: u32,
        run: impl Fn(Vec<SourceRecord>) -> Result<Vec<TargetRecord>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id,
            item,
            from,
            to,
            source: None,
            run: Arc::new(run),
        }
    }

    /// Read from a legacy collection rather than the destination.
    #[must_use]
    pub fn source(mut self, name: &'static str) -> Self {
        self.source = Some(name);
        self
    }
}

/// One owner’s collections and migrations, opened by that owner.
#[derive(Clone)]
pub struct Schema {
    /// Storage owner for these collections.
    pub owner: Owner,
    /// Declared collections and versions.
    pub items: Vec<(CollectionSpec, u32, ScopeClass)>,
    /// Ordered migrations.
    pub steps: Vec<Step>,
}

impl Schema {
    /// Start a declaration for `owner`.
    #[must_use]
    pub fn new(owner: Owner) -> Self {
        Self {
            owner,
            items: Vec::new(),
            steps: Vec::new(),
        }
    }

    /// Declare a collection and schema version.
    #[must_use]
    pub fn collection(self, name: &str, version: u32, scope: ScopeClass) -> Self {
        self.collection_with(version, CollectionSpec::new(name), scope)
    }

    /// Declare a fully specified collection and schema version.
    #[must_use]
    pub fn collection_with(
        mut self,
        version: u32,
        spec: CollectionSpec,
        scope: ScopeClass,
    ) -> Self {
        self.items.push((spec, version, scope));
        self
    }

    /// Add an ordered collection migration.
    #[must_use]
    pub fn step(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    /// Pure collection declarations for host bootstrap and documentation.
    #[must_use]
    pub fn collections(&self) -> Vec<CollectionSpec> {
        let mut specs = self
            .items
            .iter()
            .map(|(spec, _, _)| spec.clone())
            .collect::<Vec<_>>();
        specs.push(CollectionSpec::new(MIGRATIONS_COLLECTION));
        specs
    }
}

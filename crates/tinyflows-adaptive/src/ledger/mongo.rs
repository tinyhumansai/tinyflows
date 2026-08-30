//! A [`Ledger`] on MongoDB, for a hosted deployment.
//!
//! Four collections mirroring the sqlite tables, and the same conformance suite
//! runs against both. Where the two differ is concurrency: this one is a real
//! async driver and several loops may write the same ledger at once, so the two
//! counter updates use `$inc` rather than read-modify-write. A read-modify-write
//! here loses increments under exactly the load a hosted deployment has.

use async_trait::async_trait;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::{IndexOptions, ReturnDocument};
use mongodb::{Client, Collection, Database, IndexModel};

use super::{
    Episode, EpisodeStatus, Ledger, LedgerError, LedgerRow, Lesson, LessonKind, Result, Score,
};

impl From<mongodb::error::Error> for LedgerError {
    fn from(err: mongodb::error::Error) -> Self {
        Self::Backend(err.to_string())
    }
}

impl From<mongodb::bson::ser::Error> for LedgerError {
    fn from(err: mongodb::bson::ser::Error) -> Self {
        Self::Corrupt(err.to_string())
    }
}

impl From<mongodb::bson::de::Error> for LedgerError {
    fn from(err: mongodb::bson::de::Error) -> Self {
        Self::Corrupt(err.to_string())
    }
}

const ROWS: &str = "ledger_rows";
const LESSONS: &str = "lessons";
const EVIDENCE: &str = "lesson_evidence";
const SCORES: &str = "workflow_scores";
const VARIANTS: &str = "variants";
const EPISODES: &str = "episodes";
const STEPS: &str = "attempt_steps";
const COUNTERS: &str = "counters";

/// A ledger backed by a MongoDB database.
pub struct MongoLedger {
    db: Database,
    scope: Option<String>,
}

impl MongoLedger {
    /// Connect to `uri` and use the database named `database`.
    ///
    /// # Errors
    /// When the URI is malformed, the server is unreachable, or an index
    /// cannot be created.
    pub async fn connect(uri: &str, database: &str) -> Result<Self> {
        let client = Client::with_uri_str(uri).await?;
        Self::with_database(client.database(database)).await
    }

    /// Use an already-connected database. For a host that manages its own
    /// client and pool.
    ///
    /// # Errors
    /// When an index cannot be created.
    pub async fn with_database(db: Database) -> Result<Self> {
        let store = Self { db, scope: None };
        store.ensure_indexes().await?;
        Ok(store)
    }

    /// A handle onto the same database, scoped to one tenant.
    ///
    /// Cheap — a `Database` is a handle over a shared pool. Construct one per
    /// request at the edge of the service and hand it to the loop; everything
    /// downstream reads and writes the right bucket without knowing a tenant
    /// exists.
    #[must_use]
    pub fn for_tenant(&self, scope: impl Into<String>) -> Self {
        Self {
            db: self.db.clone(),
            scope: Some(scope.into()),
        }
    }

    /// This handle's bucket, as stored: `""` for global. Stored as a present
    /// empty string rather than an absent field, so the upsert filter on
    /// workflow scores matches one document instead of creating a new one each
    /// time — the same reason sqlite makes the column NOT NULL.
    fn bucket(&self) -> &str {
        self.scope.as_deref().unwrap_or_default()
    }

    /// Match the current episode bucket, including pre-tenancy global rows
    /// whose `scope_key` field is absent.
    fn episode_scope_filter(&self) -> Document {
        match &self.scope {
            Some(scope) => doc! { "scope_key": scope },
            None => doc! { "scope_key": { "$in": ["", Bson::Null] } },
        }
    }

    async fn ensure_indexes(&self) -> Result<()> {
        // Ordered by `seq`, never by timestamp: two attempts finishing in the
        // same second would otherwise read back in an arbitrary order, which
        // silently reorders the exclusion list.
        self.rows()
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "episode": 1, "seq": 1 })
                    .build(),
            )
            .await?;
        self.evidence()
            .create_index(IndexModel::builder().keys(doc! { "lesson_id": 1 }).build())
            .await?;
        // The score key is (scope_key, workflow_id) since tenancy landed. The
        // old single-field unique index would reject the same workflow id in a
        // second tenant's bucket, so it is dropped if present — failure means
        // it never existed, which is the ordinary case.
        let _ = self.scores().drop_index("workflow_id_1").await;
        let unique = IndexOptions::builder().unique(true).build();
        self.scores()
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "scope_key": 1, "workflow_id": 1 })
                    .options(unique)
                    .build(),
            )
            .await?;
        Ok(())
    }

    fn rows(&self) -> Collection<Document> {
        self.db.collection(ROWS)
    }
    fn lessons_c(&self) -> Collection<Document> {
        self.db.collection(LESSONS)
    }
    fn evidence(&self) -> Collection<Document> {
        self.db.collection(EVIDENCE)
    }
    fn scores(&self) -> Collection<Document> {
        self.db.collection(SCORES)
    }
    fn variants(&self) -> Collection<Document> {
        self.db.collection(VARIANTS)
    }
    fn episodes_c(&self) -> Collection<Document> {
        self.db.collection(EPISODES)
    }
    fn steps_c(&self) -> Collection<Document> {
        self.db.collection(STEPS)
    }

    /// The next value in a named sequence.
    ///
    /// A counter document rather than a `count()` of the collection: counting
    /// races with a concurrent insert and hands two writers the same number,
    /// while `findAndModify` with `$inc` is atomic on the server.
    async fn next_seq(&self, name: &str) -> Result<i64> {
        let updated = self
            .db
            .collection::<Document>(COUNTERS)
            .find_one_and_update(doc! { "_id": name }, doc! { "$inc": { "seq": 1 } })
            .upsert(true)
            .return_document(ReturnDocument::After)
            .await?;
        Ok(updated.and_then(|d| d.get_i64("seq").ok()).unwrap_or(1))
    }
}

fn kind_str(kind: LessonKind) -> &'static str {
    match kind {
        LessonKind::Strategy => "strategy",
        LessonKind::Constraint => "constraint",
        LessonKind::FailureMode => "failure_mode",
        LessonKind::Calibration => "calibration",
    }
}

fn as_u32(doc: &Document, key: &str) -> u32 {
    doc.get_i64(key)
        .ok()
        .and_then(|v| u32::try_from(v).ok())
        .or_else(|| doc.get_i32(key).ok().and_then(|v| u32::try_from(v).ok()))
        .unwrap_or(0)
}

fn text(doc: &Document, key: &str) -> String {
    doc.get_str(key).unwrap_or_default().to_string()
}

fn read_row(doc: &Document) -> LedgerRow {
    LedgerRow {
        id: text(doc, "_id"),
        episode: text(doc, "episode"),
        attempt: as_u32(doc, "attempt"),
        approach_sig: text(doc, "approach_sig"),
        approach_desc: text(doc, "approach_desc"),
        // An absent key and a stored null are the same thing to a reader.
        workflow_id: doc.get_str("workflow_id").ok().map(ToString::to_string),
        outcome: text(doc, "outcome"),
        cause: text(doc, "cause"),
        cost_usd: doc.get_f64("cost_usd").unwrap_or(0.0),
        at: text(doc, "at"),
        satisfied: doc.get_bool("satisfied").unwrap_or(false),
        advanced: doc.get_bool("advanced").unwrap_or(false),
    }
}

fn read_episode(doc: &Document) -> Result<Episode> {
    let scope = text(doc, "scope_key");
    Ok(Episode {
        id: doc
            .get_str("id")
            .map(str::to_string)
            .unwrap_or_else(|_| text(doc, "_id")),
        goal: serde_json::from_str(&text(doc, "goal"))
            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
        scope_key: (!scope.is_empty()).then_some(scope),
        status: serde_json::from_str(&text(doc, "status"))
            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
        attempt: as_u32(doc, "attempt"),
        stalled: as_u32(doc, "stalled"),
        started_at: text(doc, "started_at"),
        updated_at: text(doc, "updated_at"),
    })
}

include!("mongo/ledger_impl.rs");
#[cfg(test)]
#[path = "mongo_tests.rs"]
mod tests;

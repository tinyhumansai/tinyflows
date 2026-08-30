//! A [`Ledger`] on MongoDB, for a hosted deployment.
//!
//! Four collections mirroring the sqlite tables, and the same conformance suite
//! runs against both. Where the two differ is concurrency: this one is a real
//! async driver and several loops may write the same ledger at once, so the two
//! counter updates use `$inc` rather than read-modify-write. A read-modify-write
//! here loses increments under exactly the load a hosted deployment has.

use async_trait::async_trait;
use mongodb::bson::{Document, doc};
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
        id: text(doc, "_id"),
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

#[async_trait]
impl Ledger for MongoLedger {
    fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    async fn append(&self, row: &LedgerRow) -> Result<String> {
        let seq = self.next_seq(ROWS).await?;
        let id = format!("ldg_{seq:08}");
        self.rows()
            .insert_one(doc! {
                "_id": &id,
                "episode": &row.episode,
                "attempt": i64::from(row.attempt),
                "approach_sig": &row.approach_sig,
                "approach_desc": &row.approach_desc,
                "workflow_id": row.workflow_id.clone(),
                "outcome": &row.outcome,
                "cause": &row.cause,
                "cost_usd": row.cost_usd,
                "at": &row.at,
                "satisfied": row.satisfied,
                "advanced": row.advanced,
                "scope_key": self.bucket(),
                "seq": seq,
            })
            .await?;
        Ok(id)
    }

    async fn rows(&self, episode: &str) -> Result<Vec<LedgerRow>> {
        let mut cursor = self
            .rows()
            .find(doc! { "episode": episode, "scope_key": self.bucket() })
            .sort(doc! { "seq": 1 })
            .await?;
        let mut out = Vec::new();
        while cursor.advance().await? {
            out.push(read_row(&cursor.deserialize_current()?));
        }
        Ok(out)
    }

    async fn promote(&self, lesson: &Lesson, cites: &[String]) -> Result<String> {
        let seq = self.next_seq(LESSONS).await?;
        let id = format!("les_{seq:08}");
        self.lessons_c()
            .insert_one(doc! {
                "_id": &id,
                "kind": kind_str(lesson.kind),
                "trigger": &lesson.trigger,
                "mechanism": &lesson.mechanism,
                "claim": &lesson.claim,
                "applied": i64::from(lesson.applied),
                "helped": i64::from(lesson.helped),
                // The handle's, never the argument's.
                "scope_key": self.bucket(),
                "seq": seq,
            })
            .await?;
        for row_id in cites {
            // Upsert on the pair so re-promoting the same citation is a no-op
            // rather than a duplicate edge.
            self.evidence()
                .update_one(
                    doc! { "lesson_id": &id, "row_id": row_id },
                    doc! { "$setOnInsert": { "lesson_id": &id, "row_id": row_id } },
                )
                .upsert(true)
                .await?;
        }
        Ok(id)
    }

    async fn lessons(&self, kind: Option<LessonKind>) -> Result<Vec<Lesson>> {
        // This bucket plus global. An unscoped handle's bucket is global, so
        // the two halves coincide and it sees exactly what it wrote. `null` is
        // in the set because `$in` only matches a *missing* field when the
        // array contains null — and a lesson written before scoping existed
        // has no field at all; those read as global, which is what they were.
        let mine = doc! { "$in": [self.bucket(), "", mongodb::bson::Bson::Null] };
        let filter = match kind {
            Some(want) => doc! { "kind": kind_str(want), "scope_key": mine },
            None => doc! { "scope_key": mine },
        };
        let mut cursor = self
            .lessons_c()
            .find(filter)
            .sort(doc! { "seq": 1 })
            .await?;
        let mut out = Vec::new();
        while cursor.advance().await? {
            let d = cursor.deserialize_current()?;
            out.push(Lesson {
                id: text(&d, "_id"),
                kind: LessonKind::parse(&text(&d, "kind")),
                trigger: text(&d, "trigger"),
                mechanism: text(&d, "mechanism"),
                claim: text(&d, "claim"),
                applied: as_u32(&d, "applied"),
                helped: as_u32(&d, "helped"),
                scope_key: Some(text(&d, "scope_key")).filter(|s| !s.is_empty()),
            });
        }
        Ok(out)
    }

    async fn evidence(&self, lesson_id: &str) -> Result<Vec<LedgerRow>> {
        let mut cursor = self
            .evidence()
            .find(doc! { "lesson_id": lesson_id })
            .await?;
        let mut ids = Vec::new();
        while cursor.advance().await? {
            ids.push(text(&cursor.deserialize_current()?, "row_id"));
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut found = self
            .rows()
            .find(doc! { "_id": { "$in": ids }, "scope_key": self.bucket() })
            .sort(doc! { "seq": 1 })
            .await?;
        let mut out = Vec::new();
        while found.advance().await? {
            out.push(read_row(&found.deserialize_current()?));
        }
        Ok(out)
    }

    async fn score_lesson(&self, lesson_id: &str, helped: bool) -> Result<()> {
        // Constrained to what this handle can see — the id arrives from model
        // output, and naming another tenant's lesson must not move its score.
        self.lessons_c()
            .update_one(
                doc! { "_id": lesson_id,
                "scope_key": { "$in": [self.bucket(), "", mongodb::bson::Bson::Null] } },
                doc! { "$inc": { "applied": 1_i64, "helped": i64::from(helped) } },
            )
            .await?;
        Ok(())
    }

    async fn score_workflow(&self, workflow_id: &str, helped: bool) -> Result<()> {
        // `$inc` on an upsert, not read-modify-write: several loops may finish
        // the same workflow at once, and a lost increment is a promotion gate
        // reading the wrong evidence.
        self.scores()
            .update_one(
                doc! { "workflow_id": workflow_id, "scope_key": self.bucket() },
                doc! { "$inc": { "applied": 1_i64, "helped": i64::from(helped) } },
            )
            .upsert(true)
            .await?;
        Ok(())
    }

    async fn workflow_score(&self, workflow_id: &str) -> Result<Score> {
        let found = self
            .scores()
            .find_one(doc! { "workflow_id": workflow_id, "scope_key": self.bucket() })
            .await?;
        Ok(found.map_or_else(Score::default, |d| Score {
            applied: as_u32(&d, "applied"),
            helped: as_u32(&d, "helped"),
        }))
    }

    async fn link_variant(&self, parent: &str, variant: &str) -> Result<()> {
        self.variants()
            .update_one(
                doc! { "scope_key": self.bucket(), "variant": variant },
                doc! { "$setOnInsert": {
                    "scope_key": self.bucket(), "variant": variant, "parent": parent
                } },
            )
            .upsert(true)
            .await?;
        Ok(())
    }

    async fn parent_of(&self, id: &str) -> Result<Option<String>> {
        let found = self
            .variants()
            .find_one(doc! { "scope_key": self.bucket(), "variant": id })
            .await?;
        Ok(found.map(|d| text(&d, "parent")).filter(|p| !p.is_empty()))
    }

    async fn save_episode(&self, episode: &Episode) -> Result<()> {
        let goal = serde_json::to_string(&episode.goal)
            .map_err(|e| LedgerError::Corrupt(e.to_string()))?;
        let status = serde_json::to_string(&episode.status)
            .map_err(|e| LedgerError::Corrupt(e.to_string()))?;
        self.episodes_c()
            .update_one(
                doc! { "_id": &episode.id },
                doc! {
                    "$set": {
                        "goal": goal,
                        "status": status,
                        "attempt": i64::from(episode.attempt),
                        "stalled": i64::from(episode.stalled),
                        "updated_at": &episode.updated_at,
                    },
                    // Set once: the handle's scope and the first timestamp are
                    // facts about the episode's creation, not its progress.
                    "$setOnInsert": {
                        "scope_key": self.bucket(),
                        "started_at": &episode.started_at,
                    },
                },
            )
            .upsert(true)
            .await?;
        Ok(())
    }

    async fn episode(&self, id: &str) -> Result<Option<Episode>> {
        let found = self
            .episodes_c()
            .find_one(doc! { "_id": id, "scope_key": self.bucket() })
            .await?;
        found.as_ref().map(read_episode).transpose()
    }

    async fn save_steps(&self, row_id: &str, steps: &[crate::execute::StepRecord]) -> Result<()> {
        // Replace, not overlay: a shorter re-save must not leave the old tail
        // behind it, or `steps()` returns two attempts stitched together.
        self.steps_c()
            .delete_many(doc! { "scope_key": self.bucket(), "row_id": row_id })
            .await?;
        // A document per step. One per attempt would exceed the 16 MB cap on a
        // looped graph, and would do it only in production.
        for (seq, step) in steps.iter().enumerate() {
            let seq = i64::try_from(seq).unwrap_or(i64::MAX);
            self.steps_c()
                .update_one(
                    doc! { "scope_key": self.bucket(), "row_id": row_id, "seq": seq },
                    doc! { "$set": {
                        "node_id": &step.node_id,
                        "status": serde_json::to_string(&step.status)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?
                            .trim_matches('"'),
                        "output": serde_json::to_string(&step.output)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                        "duration_ms": i64::try_from(step.duration_ms).unwrap_or(i64::MAX),
                        "null_bindings": serde_json::to_string(&step.null_bindings)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                        "transcript": serde_json::to_string(&step.transcript)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                    } },
                )
                .upsert(true)
                .await?;
        }
        Ok(())
    }

    async fn steps(&self, row_id: &str) -> Result<Vec<crate::execute::StepRecord>> {
        let mut cursor = self
            .steps_c()
            .find(doc! { "scope_key": self.bucket(), "row_id": row_id })
            .sort(doc! { "seq": 1 })
            .await?;
        let mut out = Vec::new();
        while cursor.advance().await? {
            let d = cursor.deserialize_current()?;
            out.push(crate::execute::StepRecord {
                node_id: text(&d, "node_id"),
                status: if text(&d, "status") == "error" {
                    crate::execute::StepOutcome::Error
                } else {
                    crate::execute::StepOutcome::Success
                },
                output: serde_json::from_str(&text(&d, "output"))
                    .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                duration_ms: u64::from(as_u32(&d, "duration_ms")),
                null_bindings: serde_json::from_str(&text(&d, "null_bindings"))
                    .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                // A document written before this field existed has no
                // `transcript` key; `text` yields "" for it, which is not valid
                // JSON. Absent means "recorded none", so it reads as empty
                // rather than corrupting the whole attempt's steps.
                transcript: match text(&d, "transcript").as_str() {
                    "" => Vec::new(),
                    raw => serde_json::from_str(raw)
                        .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                },
            });
        }
        Ok(out)
    }

    async fn episodes(&self, running_only: bool, page: super::Page) -> Result<Vec<Episode>> {
        let mut cursor = self
            .episodes_c()
            .find(doc! { "scope_key": self.bucket() })
            .sort(doc! { "updated_at": -1, "_id": 1 })
            .await?;
        let mut out = Vec::new();
        while cursor.advance().await? {
            let episode = read_episode(&cursor.deserialize_current()?)?;
            if !running_only || episode.status == EpisodeStatus::Running {
                out.push(episode);
            }
        }
        Ok(page.apply(out))
    }

    async fn children_of(&self, id: &str) -> Result<Vec<String>> {
        let mut cursor = self
            .variants()
            .find(doc! { "scope_key": self.bucket(), "parent": id })
            .sort(doc! { "variant": 1 })
            .await?;
        let mut out = Vec::new();
        while cursor.advance().await? {
            out.push(text(&cursor.deserialize_current()?, "variant"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::conformance;

    /// Runs the same suite the sqlite backend passes, against a real server.
    ///
    /// Ignored by default: it needs one. Point `ADAPTIVE_MONGO_URI` at a
    /// throwaway database and run with `--ignored`. Skipping silently when the
    /// variable is absent would let this rot unnoticed, so the case is
    /// `#[ignore]` and visible in the run summary instead.
    #[tokio::test]
    #[ignore = "needs a MongoDB server; set ADAPTIVE_MONGO_URI"]
    async fn passes_the_conformance_suite() {
        let uri = std::env::var("ADAPTIVE_MONGO_URI").expect("ADAPTIVE_MONGO_URI");
        let name = format!("adaptive_conformance_{}", std::process::id());
        let store = MongoLedger::connect(&uri, &name).await.expect("connect");
        conformance::run_all(&store).await;
        conformance::run_tenants(
            &store,
            &store.for_tenant("user-a"),
            &store.for_tenant("user-b"),
        )
        .await;
        store.db.drop().await.expect("drop the throwaway database");
    }
}

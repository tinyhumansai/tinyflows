#[async_trait]
impl Ledger for SqliteLedger {
    fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    async fn append(&self, row: &LedgerRow) -> Result<String> {
        let conn = self.guard()?;
        let seq = next_seq(&conn, "ledger_rows")?;
        let id = new_id("ldg", seq);
        conn.execute(
            "INSERT INTO ledger_rows(id, episode, attempt, approach_sig, approach_desc,
                                     workflow_id, outcome, cause, cost_usd, at,
                                     satisfied, advanced, scope_key, seq)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                id,
                row.episode,
                i64::from(row.attempt),
                row.approach_sig,
                row.approach_desc,
                row.workflow_id,
                row.outcome,
                row.cause,
                row.cost_usd,
                row.at,
                i64::from(row.satisfied),
                i64::from(row.advanced),
                self.bucket(),
                seq,
            ],
        )?;
        Ok(id)
    }

    async fn rows(&self, episode: &str) -> Result<Vec<LedgerRow>> {
        let conn = self.guard()?;
        // Scoped as well as keyed by episode. An episode id is opaque and a
        // service may hand one straight through from a request path, so this
        // must not be the one read where guessing an id is enough.
        let mut stmt = conn.prepare(
            "SELECT * FROM ledger_rows WHERE episode = ?1 AND scope_key = ?2 ORDER BY seq",
        )?;
        let found = stmt
            .query_map(params![episode, self.bucket()], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(found)
    }

    async fn promote(&self, lesson: &Lesson, cites: &[String]) -> Result<String> {
        let conn = self.guard()?;
        let seq = next_seq(&conn, "lessons")?;
        let id = new_id("les", seq);
        conn.execute(
            "INSERT INTO lessons(id, kind, trigger, mechanism, claim, applied, helped,
                                 scope_key, seq)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                serde_json::to_string(&lesson.kind)
                    .map_err(|e| LedgerError::Corrupt(e.to_string()))?
                    .trim_matches('"'),
                lesson.trigger,
                lesson.mechanism,
                lesson.claim,
                i64::from(lesson.applied),
                i64::from(lesson.helped),
                // The handle's, never the argument's.
                self.bucket(),
                seq,
            ],
        )?;
        for row_id in cites {
            conn.execute(
                "INSERT OR IGNORE INTO lesson_evidence(lesson_id, row_id) VALUES(?1,?2)",
                params![id, row_id],
            )?;
        }
        Ok(id)
    }

    async fn lessons(&self, kind: Option<LessonKind>) -> Result<Vec<Lesson>> {
        let conn = self.guard()?;
        // This bucket plus global. An unscoped handle's bucket is global, so
        // the two halves coincide and it sees exactly what it wrote.
        let mut stmt = conn
            .prepare("SELECT * FROM lessons WHERE scope_key = ?1 OR scope_key = '' ORDER BY seq")?;
        let all = stmt
            .query_map([self.bucket()], |r| {
                let scope: String = r.get("scope_key")?;
                Ok(Lesson {
                    id: r.get("id")?,
                    kind: LessonKind::parse(&r.get::<_, String>("kind")?),
                    trigger: r.get("trigger")?,
                    mechanism: r.get("mechanism")?,
                    claim: r.get("claim")?,
                    applied: r.get::<_, i64>("applied")?.try_into().unwrap_or(0),
                    helped: r.get::<_, i64>("helped")?.try_into().unwrap_or(0),
                    scope_key: (!scope.is_empty()).then_some(scope),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(match kind {
            Some(want) => all.into_iter().filter(|l| l.kind == want).collect(),
            None => all,
        })
    }

    async fn evidence(&self, lesson_id: &str) -> Result<Vec<LedgerRow>> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT r.* FROM ledger_rows r
             JOIN lesson_evidence e ON e.row_id = r.id
             WHERE e.lesson_id = ?1 AND (r.scope_key = ?2 OR r.scope_key = '')
             ORDER BY r.seq",
        )?;
        let found = stmt
            .query_map(params![lesson_id, self.bucket()], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(found)
    }

    async fn score_lesson(&self, lesson_id: &str, helped: bool) -> Result<()> {
        let conn = self.guard()?;
        conn.execute(
            // Constrained to what this handle can see — the id arrives from
            // model output, and naming another tenant's lesson must not move
            // its score.
            "UPDATE lessons SET applied = applied + 1, helped = helped + ?2
             WHERE id = ?1 AND (scope_key = ?3 OR scope_key = '')",
            params![lesson_id, i64::from(helped), self.bucket()],
        )?;
        Ok(())
    }

    async fn score_workflow(&self, workflow_id: &str, helped: bool) -> Result<()> {
        let conn = self.guard()?;
        // Upsert: the first run of a workflow is the common case and must not
        // need a separate registration step.
        conn.execute(
            "INSERT INTO workflow_scores(scope_key, workflow_id, applied, helped)
             VALUES(?1, ?2, 1, ?3)
             ON CONFLICT(scope_key, workflow_id) DO UPDATE SET
                applied = applied + 1,
                helped  = helped + ?3",
            params![self.bucket(), workflow_id, i64::from(helped)],
        )?;
        Ok(())
    }

    async fn workflow_score(&self, workflow_id: &str) -> Result<Score> {
        let conn = self.guard()?;
        let found = conn
            .query_row(
                "SELECT applied, helped FROM workflow_scores
                 WHERE scope_key = ?1 AND workflow_id = ?2",
                params![self.bucket(), workflow_id],
                |r| {
                    Ok(Score {
                        applied: r.get::<_, i64>(0)?.try_into().unwrap_or(0),
                        helped: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
                    })
                },
            )
            .optional()?;
        Ok(found.unwrap_or_default())
    }

    async fn link_variant(&self, parent: &str, variant: &str) -> Result<()> {
        let conn = self.guard()?;
        conn.execute(
            "INSERT OR IGNORE INTO variants(scope_key, variant, parent) VALUES(?1,?2,?3)",
            params![self.bucket(), variant, parent],
        )?;
        Ok(())
    }

    async fn parent_of(&self, id: &str) -> Result<Option<String>> {
        let conn = self.guard()?;
        let found = conn
            .query_row(
                "SELECT parent FROM variants WHERE scope_key = ?1 AND variant = ?2",
                params![self.bucket(), id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found)
    }

    async fn save_episode(&self, episode: &Episode) -> Result<()> {
        let conn = self.guard()?;
        conn.execute(
            "INSERT INTO episodes(id, scope_key, goal, status, attempt, stalled,
                                  started_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(scope_key, id) DO UPDATE SET
                goal = ?3, status = ?4, attempt = ?5, stalled = ?6, updated_at = ?8",
            params![
                episode.id,
                self.bucket(),
                serde_json::to_string(&episode.goal)
                    .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                serde_json::to_string(&episode.status)
                    .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                i64::from(episode.attempt),
                i64::from(episode.stalled),
                episode.started_at,
                episode.updated_at,
            ],
        )?;
        Ok(())
    }

    async fn episode(&self, id: &str) -> Result<Option<Episode>> {
        let conn = self.guard()?;
        let found = conn
            .query_row(
                "SELECT * FROM episodes WHERE id = ?1 AND scope_key = ?2",
                params![id, self.bucket()],
                read_episode,
            )
            .optional()?;
        found.transpose()
    }

    async fn save_steps(&self, row_id: &str, steps: &[crate::execute::StepRecord]) -> Result<()> {
        let conn = self.guard()?;
        // Replace, not overlay: `INSERT OR REPLACE` only touches the sequence
        // numbers present in `steps`, so a shorter re-save would leave the old
        // tail behind and `steps()` would stitch two attempts together.
        conn.execute(
            "DELETE FROM attempt_steps WHERE scope_key = ?1 AND row_id = ?2",
            params![self.bucket(), row_id],
        )?;
        for (seq, step) in steps.iter().enumerate() {
            conn.execute(
                "INSERT OR REPLACE INTO attempt_steps(scope_key, row_id, seq, node_id, status,
                                                      output, duration_ms, null_bindings,
                                                      transcript)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    self.bucket(),
                    row_id,
                    i64::try_from(seq).unwrap_or(i64::MAX),
                    step.node_id,
                    serde_json::to_string(&step.status)
                        .map_err(|e| LedgerError::Corrupt(e.to_string()))?
                        .trim_matches('"'),
                    serde_json::to_string(&step.output)
                        .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                    i64::try_from(step.duration_ms).unwrap_or(i64::MAX),
                    serde_json::to_string(&step.null_bindings)
                        .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                    serde_json::to_string(&step.transcript)
                        .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                ],
            )?;
        }
        Ok(())
    }

    async fn steps(&self, row_id: &str) -> Result<Vec<crate::execute::StepRecord>> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT node_id, status, output, duration_ms, null_bindings, transcript
             FROM attempt_steps
             WHERE scope_key = ?1 AND row_id = ?2 ORDER BY seq",
        )?;
        let found = stmt
            .query_map(params![self.bucket(), row_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        found
            .into_iter()
            .map(
                |(node_id, status, output, duration_ms, bindings, transcript)| {
                    Ok(crate::execute::StepRecord {
                        node_id,
                        status: if status == "error" {
                            crate::execute::StepOutcome::Error
                        } else {
                            crate::execute::StepOutcome::Success
                        },
                        output: serde_json::from_str(&output)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                        duration_ms: u64::try_from(duration_ms).unwrap_or(0),
                        null_bindings: serde_json::from_str(&bindings)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                        transcript: serde_json::from_str(&transcript)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                    })
                },
            )
            .collect()
    }

    async fn episodes(&self, running_only: bool, page: super::Page) -> Result<Vec<Episode>> {
        let conn = self.guard()?;
        let mut stmt = conn
            .prepare("SELECT * FROM episodes WHERE scope_key = ?1 ORDER BY updated_at DESC, id")?;
        let all = stmt
            .query_map([self.bucket()], read_episode)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let kept: Result<Vec<Episode>> = all
            .into_iter()
            .filter(|e| {
                !running_only || e.as_ref().is_ok_and(|e| e.status == EpisodeStatus::Running)
            })
            .collect();
        Ok(page.apply(kept?))
    }

    async fn children_of(&self, id: &str) -> Result<Vec<String>> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT variant FROM variants WHERE scope_key = ?1 AND parent = ?2 ORDER BY variant",
        )?;
        let found = stmt
            .query_map(params![self.bucket(), id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(found)
    }
}

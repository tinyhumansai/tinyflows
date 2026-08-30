use super::*;

impl WorkflowStore for FileWorkflowStore {
    fn policy(&self) -> &dyn HostPolicy {
        self.policy.as_ref()
    }

    fn proposal_decision_scope(&self) -> String {
        self.decision_scope.clone()
    }

    fn list(&self) -> Result<Vec<WorkflowSummary>, WorkflowError> {
        Ok(self
            .load()
            .workflows
            .iter()
            .map(WorkflowRecord::summary)
            .collect())
    }

    fn get(&self, id: &str) -> Result<Option<WorkflowRecord>, WorkflowError> {
        Ok(self.load().workflows.into_iter().find(|w| w.id == id))
    }

    fn save(&self, record: &WorkflowRecord) -> Result<(), WorkflowError> {
        // Held across the whole read-modify-write below — see `write_lock`'s
        // doc comment for what a concurrent `save`/`delete` on this store
        // would otherwise interleave.
        let _guard = self.write_lock.lock().unwrap_or_else(|poison| {
            // A prior panic mid-write is exactly the case a lock exists to
            // survive: the on-disk state is whatever it was left in, but that
            // is what a torn write already risks and `write_atomic`'s rename
            // makes recoverable — poisoning must not turn one bad write into
            // every future save failing too.
            poison.into_inner()
        });
        self.with_definition_lock(&record.id, || {
            // The id decides a filename, so it is checked before anything else:
            // a document's own `id` overrides what the caller asked for, and a
            // document may have been written by an agent.
            let path = self.definition_path(&record.id)?;
            // Validate before writing so a listing can be trusted to be runnable.
            validate_graph(&record.id, &record.graph)?;
            let document = to_document(record)?;
            // Snapshot what is about to be replaced, before replacing it. Doing it
            // here rather than at each call site is what makes every authoring
            // surface undoable without any of them having to opt in.
            if let Some(superseded) = self.superseded_by(&path, &record.id)? {
                let staged = stage_atomic(&path, &document)?;
                let revision = revisions::capture(&self.revisions_dir, &superseded)?;
                if let Err(error) = staged.commit() {
                    revisions::rollback_capture(&revision);
                    return Err(error);
                }
                revisions::commit_capture(&revision)?;
                return Ok(());
            }
            write_atomic(&path, &document)
        })
    }

    fn save_if_fingerprint(
        &self,
        record: &WorkflowRecord,
        expected_fingerprint: &str,
    ) -> Result<bool, WorkflowError> {
        self.save_if_current_matches(record, expected_fingerprint, |current| {
            crate::store::types::fingerprint(&current.graph)
        })
    }

    fn save_if_record_fingerprint(
        &self,
        record: &WorkflowRecord,
        expected_fingerprint: &str,
    ) -> Result<bool, WorkflowError> {
        self.save_if_current_matches(
            record,
            expected_fingerprint,
            crate::store::types::record_fingerprint,
        )
    }

    fn delete(&self, id: &str) -> Result<(), WorkflowError> {
        // See `save`'s matching guard and `write_lock`'s doc comment: this is
        // the same read (`load`/`get`), snapshot, write shape.
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.with_definition_lock(id, || {
            let existing = self
                .load()
                .workflows
                .into_iter()
                .find(|w| w.id == id)
                .ok_or_else(|| WorkflowError::NotFound(id.to_string()))?;
            let default_path = self.definition_path(id)?;
            let path = existing.source_path.clone().unwrap_or(default_path);
            if path.parent() != Some(self.write_dir()) {
                return Err(WorkflowError::ReadOnlyDefinition {
                    id: id.to_string(),
                    path,
                });
            }
            // Snapshot before removing. A delete is the one edit that leaves
            // nothing to diff against afterwards, so without this it is the one
            // edit that cannot be undone.
            let revision = revisions::capture(&self.revisions_dir, &existing)?;
            if let Err(source) = std::fs::remove_file(&path) {
                revisions::rollback_capture(&revision);
                return Err(WorkflowError::Io { path, source });
            }
            revisions::commit_capture(&revision)
        })
    }

    fn record_run(&self, run: &RunRecord) -> Result<(), WorkflowError> {
        let path = self.run_path(&run.id)?;
        let body = serde_json::to_vec_pretty(run)
            .map_err(|err| WorkflowError::Malformed(err.to_string()))?;
        write_atomic(&path, &body)
    }

    fn get_run(&self, run_id: &str) -> Result<Option<RunRecord>, WorkflowError> {
        let path = self.run_path(run_id)?;
        let body = match std::fs::read(&path) {
            Ok(body) => body,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(WorkflowError::Io { path, source }),
        };
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|err| WorkflowError::Malformed(format!("{}: {err}", path.display())))
    }

    fn list_runs(&self, workflow_id: &str) -> Result<Vec<RunRecord>, WorkflowError> {
        let mut runs: Vec<RunRecord> = self
            .read_run_dir()?
            .into_iter()
            .filter(|run| run.workflow_id == workflow_id)
            .collect();
        runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
        Ok(runs)
    }

    fn unsettled_runs(&self) -> Result<Vec<RunRecord>, WorkflowError> {
        let mut runs: Vec<RunRecord> = self
            .read_run_dir()?
            .into_iter()
            .filter(|run| !run.status.is_settled())
            .collect();
        runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
        Ok(runs)
    }

    fn list_revisions(&self, workflow_id: &str) -> Result<Vec<WorkflowRevision>, WorkflowError> {
        // Releases before the source/state split kept undo snapshots beside
        // definitions. Merge that history with new workspace-scoped snapshots
        // so the first post-upgrade edit does not hide the older entries.
        revisions::list_merged(
            &self.revisions_dir,
            &self.write_dir().join(".revisions"),
            workflow_id,
        )
    }

    fn revision(
        &self,
        workflow_id: &str,
        revision_id: &str,
    ) -> Result<Option<WorkflowRevision>, WorkflowError> {
        match revisions::read(&self.revisions_dir, workflow_id, revision_id)? {
            some @ Some(_) => Ok(some),
            None => revisions::read(
                &self.write_dir().join(".revisions"),
                workflow_id,
                revision_id,
            ),
        }
    }

    fn list_notes(&self, workflow_id: &str) -> Result<Vec<WorkflowNote>, WorkflowError> {
        journal::list(&self.journal_dir, workflow_id)
    }

    fn append_note(&self, note: &WorkflowNote) -> Result<(), WorkflowError> {
        // Under the same lock as `save`/`delete`: appending is a
        // read-modify-write of one file, so two passes writing at once would
        // otherwise lose whichever note lost the race.
        //
        // Poison-tolerant for the same reason `save` is, and it matters more
        // here: this runs on the failure path, where the caller has documented
        // it as best effort. Panicking on a poisoned lock would unwind out of a
        // run that already completed.
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        journal::append(&self.journal_dir, note)
    }

    fn supersede_note(
        &self,
        workflow_id: &str,
        note_id: &str,
        by: &str,
    ) -> Result<bool, WorkflowError> {
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        journal::supersede(&self.journal_dir, workflow_id, note_id, by)
    }

    fn save_proposal(&self, proposal: &WorkflowProposal) -> Result<(), WorkflowError> {
        // Every proposal transition (verification, rejection, acceptance, and
        // supersession) funnels through this method. Serialize those writes on
        // the shared store lock so clones cannot concurrently replace the same
        // proposal document.
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        proposals::save(&self.proposals_dir, proposal)
    }

    fn save_proposal_if_fingerprint(
        &self,
        proposal: &WorkflowProposal,
        expected_fingerprint: &str,
    ) -> Result<bool, WorkflowError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.with_definition_lock(&proposal.workflow_id, || {
            let Some(current) = self.get(&proposal.workflow_id)? else {
                return Ok(false);
            };
            if crate::store::types::fingerprint(&current.graph) != expected_fingerprint {
                return Ok(false);
            }
            proposals::save(&self.proposals_dir, proposal)?;
            Ok(true)
        })
    }

    fn get_proposal(&self, id: &str) -> Result<Option<WorkflowProposal>, WorkflowError> {
        proposals::read(&self.proposals_dir, id)
    }

    fn list_proposals(&self, workflow_id: &str) -> Result<Vec<WorkflowProposal>, WorkflowError> {
        proposals::list_for(&self.proposals_dir, workflow_id)
    }

    fn lock_proposal_decision(
        &self,
        workflow_id: &str,
    ) -> Result<Box<dyn ProposalDecisionGuard>, WorkflowError> {
        std::fs::create_dir_all(&self.proposals_dir).map_err(|source| WorkflowError::Io {
            path: self.proposals_dir.clone(),
            source,
        })?;
        let path = self.proposals_dir.join(format!(
            ".workflow-{}.decision.lock",
            safe_component(workflow_id)?
        ));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| WorkflowError::Io {
                path: path.clone(),
                source,
            })?;
        file.lock_exclusive().map_err(|source| WorkflowError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Box::new(FileProposalDecisionGuard { file, path }))
    }
}

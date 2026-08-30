use super::*;

impl FileWorkflowStore {
    /// A store over explicit directories. Mostly for tests; production callers
    /// want [`FileWorkflowStore::discover`].
    pub fn new(dirs: Vec<PathBuf>, runs_dir: PathBuf) -> Self {
        // The journal is derived from the runs directory rather than taken as a
        // parameter, so every existing caller of this constructor keeps working
        // and still gets a working journal. `with_state` is the explicit form.
        let journal_dir = runs_dir
            .parent()
            .map(|state| state.join("journal"))
            .unwrap_or_else(|| PathBuf::from("journal"));
        let proposals_dir = runs_dir
            .parent()
            .map(|state| state.join("proposals"))
            .unwrap_or_else(|| PathBuf::from("proposals"));
        let definition_root = catalog_identity(&dirs)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("state/workflows");
        let definition_state = definition_state_dir(&definition_root, &dirs);
        let decision_scope = file_store_scope(&proposals_dir);
        Self {
            dirs,
            runs_dir,
            journal_dir,
            proposals_dir,
            revisions_dir: definition_state.join("revisions"),
            definition_locks_dir: definition_state.join("locks"),
            decision_scope,
            write_lock: Arc::new(Mutex::new(())),
            policy: Arc::new(EnginePolicy),
        }
    }

    /// A store whose host state lives under one directory.
    ///
    /// The explicit form of [`FileWorkflowStore::new`], for callers that know
    /// where state belongs rather than only where runs go.
    pub fn with_state(dirs: Vec<PathBuf>, state_dir: &Path) -> Self {
        Self::with_state_roots(dirs, state_dir, state_dir)
    }

    /// Build with independently selected run and shared definition state roots.
    fn with_state_roots(dirs: Vec<PathBuf>, run_state: &Path, definition_state: &Path) -> Self {
        let proposals_dir = run_state.join("proposals");
        let definition_state = definition_state_dir(definition_state, &dirs);
        Self {
            dirs,
            runs_dir: run_state.join("runs"),
            journal_dir: run_state.join("journal"),
            revisions_dir: definition_state.join("revisions"),
            definition_locks_dir: definition_state.join("locks"),
            decision_scope: file_store_scope(&proposals_dir),
            proposals_dir,
            write_lock: Arc::new(Mutex::new(())),
            policy: Arc::new(EnginePolicy),
        }
    }

    /// The same store, judging every document and every edit by `policy`.
    ///
    /// Applied to reads as well as writes, so a document that became invalid
    /// because the host's harness vocabulary changed under it is reported the
    /// next time it is read rather than silently running somewhere else.
    #[must_use]
    pub fn with_policy(mut self, policy: Arc<dyn HostPolicy>) -> Self {
        self.policy = policy;
        self
    }

    /// A store whose host state is isolated to one workspace.
    pub fn with_workspace_state(dirs: Vec<PathBuf>, state_dir: &Path, workspace: &Path) -> Self {
        Self::with_state_roots(dirs, &scoped_state_dir(state_dir, workspace), state_dir)
    }

    /// A store over the conventional locations beneath `home`, for the working
    /// directory `cwd`.
    ///
    /// `project_dir` is the per-checkout directory a host keeps its own data in
    /// (e.g. `.myapp`), whose `workflows/` subdirectory supplies repository-
    /// provided defaults.
    pub fn discover(home: &Path, cwd: &Path, project_dir: &str) -> Self {
        let state_dir = home.join("state").join("workflows");
        Self::with_workspace_state(workflow_dirs(home, cwd, project_dir), &state_dir, cwd)
    }

    /// The definition directories, lowest precedence first.
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The directory new definitions are written to: the highest-precedence one,
    /// which production discovery resolves to `<host data dir>/workflows`.
    ///
    /// Project-local workflows remain a readable lower-precedence layer, but
    /// generated user data belongs beside the host's own config and state
    /// rather than appearing as an untracked repository artifact.
    pub fn write_dir(&self) -> &Path {
        self.dirs
            .last()
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new("."))
    }

    /// Read every `*.json` in every directory, later directories overriding
    /// earlier ones by workflow id.
    ///
    /// Files within one directory are read in sorted order so the catalog is
    /// stable across platforms. Never fails: a missing directory yields nothing
    /// and a bad document yields an entry in [`LoadReport::errors`].
    pub fn load(&self) -> LoadReport {
        let mut report = LoadReport::default();
        for dir in &self.dirs {
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                // Not existing is the normal state, not a failure worth reporting.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    report.errors.push(format!("{}: {err}", dir.display()));
                    continue;
                }
            };
            report.dirs.push(dir.clone());

            let mut paths: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|path| is_json(path))
                .collect();
            paths.sort();

            for path in paths {
                match read_workflow_with(&path, self.policy.as_ref()) {
                    Ok(record) => upsert(&mut report.workflows, record),
                    Err(err) => report.errors.push(err),
                }
            }
        }
        report
    }

    /// The path a workflow with `id` is written to.
    pub(super) fn definition_path(&self, id: &str) -> Result<PathBuf, WorkflowError> {
        Ok(self
            .write_dir()
            .join(format!("{}.json", safe_component(id)?)))
    }

    /// The version a save to `path` is about to supersede, if there is one.
    ///
    /// Two cases, and the cheap one is the common one. When the write directory
    /// already holds this workflow, that file *is* what a reader resolves to —
    /// the write directory is the highest-precedence one — so parsing it alone
    /// is exactly right and costs one read.
    ///
    /// Only when it does not is a full load needed: the workflow is coming from
    /// a lower-precedence directory and this save will shadow it. Snapshotting
    /// the shadowed version is what lets an operator undo a project-local edit
    /// back to what their home directory had.
    pub(super) fn superseded_by(
        &self,
        path: &Path,
        id: &str,
    ) -> Result<Option<WorkflowRecord>, WorkflowError> {
        if path.exists() {
            // A file that no longer parses is not a version worth keeping, and
            // refusing the save over it would strand the operator with a broken
            // definition they cannot overwrite.
            return Ok(read_workflow_with(path, self.policy.as_ref()).ok());
        }
        self.get(id)
    }

    /// The path a run record is written to.
    pub(super) fn run_path(&self, run_id: &str) -> Result<PathBuf, WorkflowError> {
        Ok(self
            .runs_dir
            .join(format!("{}.json", safe_component(run_id)?)))
    }

    /// Every parsable run record in the scope's runs directory, unordered.
    ///
    /// A missing directory is an empty listing, not an error: a scope that has
    /// never run anything has nothing to read. A record this build cannot parse
    /// is skipped rather than failing the whole listing — history is
    /// diagnostic, and one corrupt file should not hide the rest of it.
    pub(super) fn read_run_dir(&self) -> Result<Vec<RunRecord>, WorkflowError> {
        let entries = match std::fs::read_dir(&self.runs_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(WorkflowError::Io {
                    path: self.runs_dir.clone(),
                    source,
                });
            }
        };
        Ok(entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| is_json(path))
            .filter_map(|path| std::fs::read(&path).ok())
            .filter_map(|body| serde_json::from_slice::<RunRecord>(&body).ok())
            .collect())
    }

    /// Run one workflow definition mutation while holding its filesystem lock.
    pub(super) fn with_definition_lock<T>(
        &self,
        workflow_id: &str,
        operation: impl FnOnce() -> Result<T, WorkflowError>,
    ) -> Result<T, WorkflowError> {
        std::fs::create_dir_all(&self.definition_locks_dir).map_err(|source| {
            WorkflowError::Io {
                path: self.definition_locks_dir.clone(),
                source,
            }
        })?;
        let lock_path = self
            .definition_locks_dir
            .join(format!(".{}.lock", safe_component(workflow_id)?));
        let file_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| WorkflowError::Io {
                path: lock_path.clone(),
                source,
            })?;
        file_lock
            .lock_exclusive()
            .map_err(|source| WorkflowError::Io {
                path: lock_path.clone(),
                source,
            })?;
        let result = operation();
        if let Err(source) = FileExt::unlock(&file_lock) {
            tracing::warn!(path = %lock_path.display(), "failed to release workflow lock: {source}");
        }
        result
    }

    /// Atomically save a workflow when the selected part of its current record
    /// still matches the caller's observation.
    pub(super) fn save_if_current_matches(
        &self,
        record: &WorkflowRecord,
        expected_fingerprint: &str,
        fingerprint: impl FnOnce(&WorkflowRecord) -> String,
    ) -> Result<bool, WorkflowError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.with_definition_lock(&record.id, || {
            let Some(current) = self.get(&record.id)? else {
                return Ok(false);
            };
            if fingerprint(&current) != expected_fingerprint {
                return Ok(false);
            }
            let path = self.definition_path(&record.id)?;
            validate_graph(&record.id, &record.graph)?;
            let document = to_document(record)?;
            let staged = stage_atomic(&path, &document)?;
            let revision = revisions::capture(&self.revisions_dir, &current)?;
            if let Err(error) = staged.commit() {
                revisions::rollback_capture(&revision);
                return Err(error);
            }
            revisions::commit_capture(&revision)?;
            Ok(true)
        })
    }
}

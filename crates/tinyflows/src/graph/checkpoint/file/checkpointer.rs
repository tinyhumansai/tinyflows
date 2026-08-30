use super::*;

#[async_trait]
impl<State> Checkpointer<State> for FileCheckpointer<State>
where
    State: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    async fn put(&self, checkpoint: Checkpoint<State>) -> Result<CheckpointId> {
        let id = CheckpointId::new(checkpoint.checkpoint_id.clone());
        // The serialize + filesystem append is synchronous, blocking work; run
        // it on the blocking pool so it never stalls a tokio worker on the
        // step-critical path.
        let base_dir = self.base_dir.clone();
        let path = self.thread_path(&checkpoint.thread_id);
        tokio::task::spawn_blocking(move || -> Result<()> {
            fs::create_dir_all(&base_dir).map_err(|e| io_err("create base dir", e))?;
            let mut line =
                serde_json::to_string(&checkpoint).map_err(|e| io_err("encode record", e))?;
            line.push('\n');
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| io_err("open thread file for append", e))?;
            file.write_all(line.as_bytes())
                .map_err(|e| io_err("append record", e))?;
            // Without an explicit flush to stable storage a "persisted"
            // checkpoint is only in the page cache: a host crash loses
            // boundaries the executor has already reported as durable, and can
            // leave a torn trailing line behind (which `read_records` now
            // tolerates, but should not have to see).
            file.sync_all().map_err(|e| io_err("fsync record", e))
        })
        .await
        .map_err(|e| io_err("join blocking put task", e))??;
        Ok(id)
    }

    async fn get(
        &self,
        thread_id: &str,
        checkpoint_id: Option<&str>,
    ) -> Result<Option<Checkpoint<State>>> {
        // Stream lines and fully decode only the single target line, instead of
        // deserializing every record's `State` just to pick one. Selection
        // matches the previous `rev().find` / `next_back` semantics: the last
        // matching line (or the last line, for `None`) wins.
        let path = self.thread_path(thread_id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("open thread file", e)),
        };
        let reader = BufReader::new(file);
        let mut target: Option<String> = None;
        for line in reader.lines() {
            let line = line.map_err(|e| io_err("read line", e))?;
            if line.trim().is_empty() {
                continue;
            }
            match checkpoint_id {
                Some(id) => {
                    // Decode only the id header to test the match, not `State`.
                    let header: CheckpointIdHeader =
                        serde_json::from_str(&line).map_err(|e| io_err("decode header", e))?;
                    if header.checkpoint_id == id {
                        target = Some(line);
                    }
                }
                None => target = Some(line),
            }
        }
        match target {
            Some(line) => Ok(Some(
                serde_json::from_str(&line).map_err(|e| io_err("decode record", e))?,
            )),
            None => Ok(None),
        }
    }

    async fn list(&self, thread_id: &str) -> Result<Vec<CheckpointMetadata>> {
        Ok(self
            .read_records(thread_id)?
            .iter()
            .map(Checkpoint::to_metadata)
            .collect())
    }

    async fn get_thread(&self, thread_id: &str) -> Result<Vec<Checkpoint<State>>> {
        // Single-pass bulk read: parse the thread file once, instead of the
        // default's one whole-file `get` scan per listed id (O(H²)).
        self.read_records(thread_id)
    }

    async fn state_history(
        &self,
        thread_id: &str,
        namespace: &[String],
        limit: Option<usize>,
    ) -> Result<Vec<CheckpointTuple<State>>> {
        // Read the whole thread once, then walk the parent lineage in memory
        // (O(H)), instead of re-reading and re-parsing the file per hop (O(H²)).
        let records = self.read_records(thread_id)?;
        if records.is_empty() {
            return Ok(Vec::new());
        }

        // id -> checkpoint, last write wins for duplicate ids (matching `get`,
        // which takes the last matching record). Track the latest checkpoint in
        // the target namespace as the walk's starting point.
        let mut by_id: std::collections::HashMap<String, Checkpoint<State>> =
            std::collections::HashMap::with_capacity(records.len());
        let mut cursor: Option<String> = None;
        for record in records {
            if record.namespace.as_slice() == namespace {
                cursor = Some(record.checkpoint_id.clone());
            }
            by_id.insert(record.checkpoint_id.clone(), record);
        }

        let mut out = Vec::new();
        while let Some(id) = cursor {
            if let Some(limit) = limit
                && out.len() >= limit
            {
                break;
            }
            // `remove` doubles as a cycle guard: each id is visited at most once.
            let Some(checkpoint) = by_id.remove(&id) else {
                break;
            };
            // A checkpoint outside the target namespace is not visible under
            // namespace-scoped lookup, so the lineage walk stops (matching the
            // `get_scoped`-based default).
            if checkpoint.namespace.as_slice() != namespace {
                break;
            }
            cursor = checkpoint.parent_checkpoint_id.clone();
            out.push(tuple_from_checkpoint(checkpoint));
        }
        Ok(out)
    }

    async fn list_threads(&self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.base_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err("read base dir", e)),
        };
        let mut threads = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err("read dir entry", e))?;
            let path = entry.path();
            // Match on the filename suffix rather than `Path::extension()`.
            // The empty thread id escapes to the empty string, so its file is
            // literally `.jsonl` — a dotfile whose `extension()` is `None`,
            // which made that thread invisible to listing (and to everything
            // built on listing) while `get`/`put` addressed it perfectly well.
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.ends_with(&format!(".{THREAD_EXT}")) || name.ends_with(WRITES_SUFFIX) {
                continue;
            }
            // Recover the canonical thread id from the first record rather than
            // un-escaping the filename, so the value always matches what was
            // persisted.
            let file = File::open(&path).map_err(|e| io_err("open thread file", e))?;
            let mut reader = BufReader::new(file);
            let mut first = String::new();
            loop {
                first.clear();
                let read = reader
                    .read_line(&mut first)
                    .map_err(|e| io_err("read line", e))?;
                if read == 0 {
                    break; // empty file — skip
                }
                if first.trim().is_empty() {
                    continue;
                }
                // One unreadable file must not take down the whole listing.
                // `list_threads` decodes the first line of *every* file, so an
                // error here made a single poisoned thread break listing —
                // and therefore every operation built on it — globally.
                match serde_json::from_str::<Checkpoint<serde::de::IgnoredAny>>(&first) {
                    Ok(record) => threads.push(record.thread_id),
                    Err(e) => tracing::warn!(
                        "[checkpoint:file] list_threads: skipping unreadable thread file {}: {e}",
                        path.display()
                    ),
                }
                break;
            }
        }
        Ok(threads)
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<()> {
        // The write sidecar goes with the thread: leaving it behind would let a
        // later thread of the same id inherit a dead ledger.
        for path in [self.thread_path(thread_id), self.writes_path(thread_id)] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err("delete thread file", e)),
            }
        }
        Ok(())
    }

    async fn delete_checkpoints(&self, thread_id: &str, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let drop: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut records = self.read_records(thread_id)?;
        let before = records.len();
        records.retain(|c| !drop.contains(c.checkpoint_id.as_str()));
        let removed = before - records.len();
        if removed > 0 {
            self.write_records(thread_id, &records)?;
            // Drop the deleted checkpoints' write ledgers with them.
            let writes_path = self.writes_path(thread_id);
            let write_records = Self::read_write_records(&writes_path, thread_id)?;
            let kept: Vec<&WriteRecord> = write_records
                .iter()
                .filter(|r| !drop.contains(r.checkpoint_id.as_str()))
                .collect();
            if kept.len() != write_records.len() {
                let mut buf = String::new();
                for record in kept {
                    let line =
                        serde_json::to_string(record).map_err(|e| io_err("encode write", e))?;
                    buf.push_str(&line);
                    buf.push('\n');
                }
                if buf.is_empty() {
                    match fs::remove_file(&writes_path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(io_err("remove empty writes file", e)),
                    }
                } else {
                    write_atomic(&writes_path, buf.as_bytes())?;
                }
            }
        }
        Ok(removed)
    }

    async fn put_writes(&self, config: &CheckpointConfig, writes: &[PendingWrite]) -> Result<()> {
        let checkpoint_id = super::super::require_checkpoint_id(config)?;
        if writes.is_empty() {
            return Ok(());
        }
        let path = self.writes_path(&config.thread_id);
        let mut records = Self::read_write_records(&path, &config.thread_id)?;

        // Split out this checkpoint's ledger, merge, then rebuild the file.
        let (mut mine, others): (Vec<WriteRecord>, Vec<WriteRecord>) = records
            .drain(..)
            .partition(|r| r.checkpoint_id == checkpoint_id && r.namespace == config.namespace);
        let mut existing: Vec<PendingWrite> = mine.drain(..).map(|r| r.write).collect();
        let changed = merge_writes(&mut existing, writes);

        let mut buf = String::new();
        for record in others.iter() {
            let line = serde_json::to_string(record).map_err(|e| io_err("encode write", e))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        for write in existing {
            let record = WriteRecord {
                namespace: config.namespace.clone(),
                checkpoint_id: checkpoint_id.clone(),
                write,
            };
            let line = serde_json::to_string(&record).map_err(|e| io_err("encode write", e))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        fs::create_dir_all(&self.base_dir).map_err(|e| io_err("create base dir", e))?;
        write_atomic(&path, buf.as_bytes())?;
        tracing::debug!(
            "[checkpoint:file] put_writes thread={} checkpoint={checkpoint_id} offered={} stored={changed}",
            config.thread_id,
            writes.len()
        );
        Ok(())
    }

    async fn get_writes(&self, config: &CheckpointConfig) -> Result<Vec<PendingWrite>> {
        let Some(checkpoint_id) = self.resolve_write_target(config).await? else {
            return Ok(Vec::new());
        };
        let path = self.writes_path(&config.thread_id);
        Ok(Self::read_write_records(&path, &config.thread_id)?
            .into_iter()
            .filter(|r| r.checkpoint_id == checkpoint_id && r.namespace == config.namespace)
            .map(|r| r.write)
            .collect())
    }
}

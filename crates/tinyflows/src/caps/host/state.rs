//! Durable key/value state for stateful workflows.
//!
//! One JSON file per key, under a per-workflow namespace directory, so two
//! workflows can use the same key name without colliding. Keys are hashed into
//! their filename rather than used verbatim: a key is author-supplied and may
//! contain path separators, and a `StateStore` must never be a way to write
//! outside its own directory.
//!
//! Writes are staged and renamed, never made in place, so a key can never be
//! left holding a half-written document that no later read can recover from.
//!
//! That guarantee is about *process* crashes, not power loss: `store` neither
//! `sync_all`s the staged file before the rename nor syncs the namespace
//! directory afterward, so a kill -9 mid-write always leaves either the whole
//! previous value or the whole new one, but a host that also needs the rename
//! itself to survive an unclean *shutdown* (a crash, not just a killed
//! process) needs to add that fsync discipline on top of this.

use std::path::{Path, PathBuf};

use crate::caps::StateStore;
use crate::error::{EngineError, Result};
use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// A [`StateStore`] over files beneath a namespace directory.
pub struct FileStateStore {
    /// The namespace's directory: `<state dir>/<namespace hash>`.
    dir: PathBuf,
}

impl FileStateStore {
    /// A store for `namespace` (conventionally `workflow:<id>`) under `root`.
    pub fn new(root: &Path, namespace: &str) -> Self {
        Self {
            dir: root.join(digest(namespace)),
        }
    }

    /// The file a key is stored in.
    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.json", digest(key)))
    }
}

/// A hex SHA-256 digest, used to turn an arbitrary author-supplied string into
/// one safe path component.
fn digest(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[async_trait]
impl StateStore for FileStateStore {
    async fn load(&self, key: &str) -> Result<Option<Value>> {
        let path = self.path(key);
        // `tokio::fs` rather than `std::fs`: these run on the runtime's worker
        // threads alongside every other node in the graph, and a blocking read
        // here stalls whatever else is scheduled there.
        match tokio::fs::read(&path).await {
            Ok(body) => serde_json::from_slice(&body)
                .map(Some)
                .map_err(|err| EngineError::Capability(format!("state: {key}: {err}"))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(EngineError::Capability(format!("state: {key}: {err}"))),
        }
    }

    async fn store(&self, key: &str, value: Value) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|err| EngineError::Capability(format!("state: {key}: {err}")))?;
        let body = serde_json::to_vec(&value)
            .map_err(|err| EngineError::Capability(format!("state: {key}: {err}")))?;
        let path = self.path(key);
        // Staged and renamed rather than written in place. A plain write
        // truncates and then fills, so a kill in that window — or a second
        // writer for the same key — leaves a prefix of JSON on disk, and `load`
        // has no way to read a prefix: the key would be wedged for good. A
        // rename publishes either the whole previous value or the whole new
        // one. The temp name carries a unique token so two writers racing on
        // one key cannot scribble over each other's scratch file, and it sits
        // beside the target so the rename stays within one filesystem.
        let tmp = self.dir.join(format!("{}.tmp", crate::ids::token()));
        // A failed write must not leave the scratch file behind either: like a
        // failed rename, it would otherwise accumulate under the namespace
        // directory, and because every attempt names a fresh token, retries
        // under a full disk would pile up partial `.tmp` files.
        if let Err(err) = tokio::fs::write(&tmp, body).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(EngineError::Capability(format!("state: {key}: {err}")));
        }
        if let Err(err) = tokio::fs::rename(&tmp, &path).await {
            // A failed rename must not leave scratch files accumulating in the
            // namespace directory.
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(EngineError::Capability(format!("state: {key}: {err}")));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

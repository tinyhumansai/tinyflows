//! Which directories hold workflows.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The workflow directories for a host laying its catalog out the conventional
/// way, lowest precedence first: project-local `<cwd>/<project_dir>/workflows`,
/// then user-global `<home>/workflows`.
///
/// Project definitions remain readable as repository-provided defaults, while
/// authored and edited definitions are written to the final, user-global layer
/// beside the rest of the host's persistent data.
///
/// The two must be distinct directories. A host whose `home` resolves *inside*
/// `<cwd>/<project_dir>` would read the same directory twice and make every
/// workflow shadow itself; that is the host's constraint to honour when it
/// chooses a home, not something this function can check.
pub fn workflow_dirs(home: &Path, cwd: &Path, project_dir: &str) -> Vec<PathBuf> {
    vec![
        cwd.join(project_dir).join("workflows"),
        home.join("workflows"),
    ]
}

/// State shared by stores writing the same catalog, beneath the caller's root.
pub(crate) fn definition_state_dir(state_root: &Path, dirs: &[PathBuf]) -> PathBuf {
    let write_dir = catalog_identity(dirs);
    let scope = format!(
        "{:x}",
        Sha256::digest(write_dir.as_os_str().as_encoded_bytes())
    );
    state_root.join("definitions").join(scope)
}

/// Canonical identity of the catalog's write destination.
pub(crate) fn catalog_identity(dirs: &[PathBuf]) -> PathBuf {
    let raw = dirs.last().map_or_else(
        || PathBuf::from("."),
        |dir| {
            if dir.is_absolute() {
                dir.clone()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(dir)
            }
        },
    );
    canonical_path_identity(&raw)
}

/// Resolve existing symlinks while retaining lexical semantics for missing parts.
fn canonical_path_identity(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if resolved.exists() {
                    resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
                }
                resolved.pop();
            }
            other => {
                resolved.push(other.as_os_str());
                if resolved.exists() {
                    resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
                }
            }
        }
    }
    resolved
}

#[cfg(test)]
#[path = "dirs_tests.rs"]
mod tests;

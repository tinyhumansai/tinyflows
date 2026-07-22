//! Explicit shared-tab and workflow-run binding registry.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Chrome tab identifier assigned by the browser.
pub type TabId = u64;

/// Stable workflow-run identifier assigned by the native host.
pub type RunId = String;

/// Metadata for a tab the user explicitly shared with TinyFlows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedTab {
    /// Browser-assigned tab id.
    pub id: TabId,
    /// Browser window containing the tab.
    pub window_id: u64,
    /// Last reported regular-site URL.
    pub url: String,
    /// Last reported title.
    pub title: String,
    /// Monotonic generation incremented each time this tab is shared.
    pub generation: u64,
}

/// Immutable binding between a workflow run and its explicitly selected tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBinding {
    /// Native workflow run id.
    pub run_id: RunId,
    /// Shared tab controlled by this run.
    pub tab_id: TabId,
    /// Sharing generation captured when the run started.
    pub tab_generation: u64,
}

/// Errors produced by explicit tab sharing and run binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabRegistryError {
    /// A run attempted to use a tab the user did not share.
    TabNotShared,
    /// A tab was detached and its previous sharing generation is no longer valid.
    TabRevoked,
    /// A run is already bound to a different tab.
    RunAlreadyBound,
    /// A browser command attempted to address another run's tab.
    RunTabMismatch,
    /// The URL is a browser-internal or otherwise unsupported page.
    UnsupportedPage,
}

impl TabRegistryError {
    /// Stable error code returned across the relay protocol.
    pub fn code(&self) -> &'static str {
        match self {
            Self::TabNotShared => "tab_not_shared",
            Self::TabRevoked => "tab_revoked",
            Self::RunAlreadyBound => "run_already_bound",
            Self::RunTabMismatch => "run_tab_mismatch",
            Self::UnsupportedPage => "unsupported_page",
        }
    }
}

impl std::fmt::Display for TabRegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for TabRegistryError {}

/// Registry containing only user-shared tabs and explicit run bindings.
#[derive(Debug, Default)]
pub struct TabRegistry {
    shared: HashMap<TabId, SharedTab>,
    bindings: HashMap<RunId, RunBinding>,
    generations: HashMap<TabId, u64>,
}

impl TabRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Shares or refreshes a regular-site tab.
    ///
    /// Calling this for an already-shared tab updates its metadata without
    /// changing the generation, so active runs remain valid.
    pub fn share(
        &mut self,
        id: TabId,
        window_id: u64,
        url: impl Into<String>,
        title: impl Into<String>,
    ) -> Result<&SharedTab, TabRegistryError> {
        let url = url.into();
        if !is_supported_url(&url) {
            return Err(TabRegistryError::UnsupportedPage);
        }
        let generation = self
            .shared
            .get(&id)
            .map(|tab| tab.generation)
            .unwrap_or_else(|| {
                let next = self.generations.get(&id).copied().unwrap_or(0) + 1;
                self.generations.insert(id, next);
                next
            });
        self.shared.insert(
            id,
            SharedTab {
                id,
                window_id,
                url,
                title: title.into(),
                generation,
            },
        );
        Ok(self.shared.get(&id).expect("shared tab was just inserted"))
    }

    /// Revokes access immediately and returns affected workflow run ids.
    pub fn revoke(&mut self, tab_id: TabId) -> Vec<RunId> {
        self.shared.remove(&tab_id);
        self.bindings
            .values()
            .filter(|binding| binding.tab_id == tab_id)
            .map(|binding| binding.run_id.clone())
            .collect::<Vec<_>>()
    }

    /// Binds a newly started run to one explicitly shared tab.
    pub fn bind_run(
        &mut self,
        run_id: impl Into<RunId>,
        tab_id: TabId,
    ) -> Result<RunBinding, TabRegistryError> {
        let run_id = run_id.into();
        if let Some(existing) = self.bindings.get(&run_id) {
            if existing.tab_id == tab_id {
                return Ok(existing.clone());
            }
            return Err(TabRegistryError::RunAlreadyBound);
        }
        let tab = self
            .shared
            .get(&tab_id)
            .ok_or(TabRegistryError::TabNotShared)?;
        let binding = RunBinding {
            run_id: run_id.clone(),
            tab_id,
            tab_generation: tab.generation,
        };
        self.bindings.insert(run_id, binding.clone());
        Ok(binding)
    }

    /// Removes a completed or cancelled run's tab binding.
    pub fn unbind_run(&mut self, run_id: &str) -> Option<RunBinding> {
        self.bindings.remove(run_id)
    }

    /// Authorizes an action only when it targets its run's still-shared tab.
    pub fn authorize(
        &self,
        run_id: &str,
        requested_tab: TabId,
    ) -> Result<&SharedTab, TabRegistryError> {
        let binding = self
            .bindings
            .get(run_id)
            .ok_or(TabRegistryError::TabNotShared)?;
        if binding.tab_id != requested_tab {
            return Err(TabRegistryError::RunTabMismatch);
        }
        let tab = self
            .shared
            .get(&binding.tab_id)
            .ok_or(TabRegistryError::TabRevoked)?;
        if tab.generation != binding.tab_generation {
            return Err(TabRegistryError::TabRevoked);
        }
        Ok(tab)
    }

    /// Returns the tab bound to a run, if one is active.
    pub fn binding(&self, run_id: &str) -> Option<&RunBinding> {
        self.bindings.get(run_id)
    }

    /// Lists only tabs currently shared by the user in deterministic order.
    pub fn list(&self) -> Vec<&SharedTab> {
        let mut tabs = self.shared.values().collect::<Vec<_>>();
        tabs.sort_by_key(|tab| tab.id);
        tabs
    }
}

fn is_supported_url(url: &str) -> bool {
    let scheme = url.split_once(':').map(|(scheme, _)| scheme);
    matches!(scheme, Some("http" | "https"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_only_explicitly_shared_regular_tabs() {
        let mut registry = TabRegistry::new();
        assert_eq!(
            registry.bind_run("run-1", 7).unwrap_err(),
            TabRegistryError::TabNotShared
        );
        assert_eq!(
            registry
                .share(7, 1, "chrome://settings", "Settings")
                .unwrap_err(),
            TabRegistryError::UnsupportedPage
        );
        registry
            .share(7, 1, "https://example.test", "Example")
            .unwrap();
        registry.bind_run("run-1", 7).unwrap();
        assert_eq!(registry.authorize("run-1", 7).unwrap().id, 7);
    }

    #[test]
    fn never_falls_back_to_another_shared_tab() {
        let mut registry = TabRegistry::new();
        registry.share(7, 1, "https://one.test", "One").unwrap();
        registry.share(8, 1, "https://two.test", "Two").unwrap();
        registry.bind_run("run-1", 7).unwrap();
        assert_eq!(
            registry.authorize("run-1", 8).unwrap_err(),
            TabRegistryError::RunTabMismatch
        );
    }

    #[test]
    fn revocation_invalidates_runs_and_new_share_gets_new_generation() {
        let mut registry = TabRegistry::new();
        let first_generation = registry
            .share(7, 1, "https://example.test", "Example")
            .unwrap()
            .generation;
        registry.bind_run("run-1", 7).unwrap();
        assert_eq!(registry.revoke(7), vec!["run-1"]);
        let second_generation = registry
            .share(7, 1, "https://example.test", "Example")
            .unwrap()
            .generation;
        assert!(second_generation > first_generation);
        assert_eq!(
            registry.authorize("run-1", 7).unwrap_err(),
            TabRegistryError::TabRevoked
        );
    }
}

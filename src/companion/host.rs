//! Host seam for embedding companions.
//!
//! By default the companion lists and runs workflows from its `workflows_dir`
//! (JSON files) via [`CompanionServer::start_workflow`](super::CompanionServer::start_workflow).
//! An embedding host (e.g. OpenHuman's flows domain) can instead own workflow
//! listing + execution by supplying a [`CompanionRunHost`] on
//! [`CompanionServerConfig::run_host`](super::CompanionServerConfig): when set,
//! the WS control channel and the HTTP native endpoints delegate to it, so
//! extension-initiated runs flow through the host's own engine (its run
//! registry, gates, and history) instead of the built-in path.

use async_trait::async_trait;
use serde_json::Value;

use super::{TabId, WorkflowSummary};

/// Lets an embedding host own the workflow-listing and run-execution the
/// companion exposes to the extension. All methods are called only for
/// already-authenticated control/native requests.
#[async_trait]
pub trait CompanionRunHost: Send + Sync {
    /// The workflows the host chooses to expose to the extension side-panel
    /// (e.g. only flows the user opted into browser-triggering).
    async fn list_workflows(&self) -> Result<Vec<WorkflowSummary>, String>;

    /// Start a run of `workflow_id` bound to the explicitly-shared `tab_id`,
    /// returning the host's own run id. The host is responsible for binding the
    /// run to the tab and applying browser routing in its own engine.
    async fn start_run(
        &self,
        workflow_id: &str,
        tab_id: TabId,
        input: Value,
    ) -> Result<String, String>;

    /// Cancel a previously started host run. Returns whether anything was
    /// cancelled (mirrors [`CompanionServer::cancel_workflow`](super::CompanionServer::cancel_workflow)).
    async fn cancel_run(&self, run_id: &str) -> bool;
}

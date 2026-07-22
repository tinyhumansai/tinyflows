//! Strictly versioned native companion control messages.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{RunId, SharedTab, TabId};

/// Metadata for a workflow exposed from the companion's configured directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSummary {
    /// Stable host-local workflow id.
    pub id: String,
    /// Human-readable workflow name.
    pub name: String,
}

/// A native workflow run event sent to side-panel subscribers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunEvent {
    /// The native host accepted a run.
    Started {
        /// Run receiving this event.
        run_id: RunId,
        /// Explicit tab selected when the run started.
        tab_id: TabId,
    },
    /// A workflow node began executing.
    StepStarted {
        /// Run receiving this event.
        run_id: RunId,
        /// Workflow node id.
        node_id: String,
        /// Node kind shown by the side panel.
        node_kind: String,
    },
    /// A workflow node completed.
    StepCompleted {
        /// Run receiving this event.
        run_id: RunId,
        /// Workflow node id.
        node_id: String,
        /// Structured non-secret output.
        output: Value,
    },
    /// A workflow run reached a terminal success state.
    Completed {
        /// Run receiving this event.
        run_id: RunId,
        /// Final structured workflow output.
        output: Value,
    },
    /// A workflow run reached a terminal failure state.
    Failed {
        /// Run receiving this event.
        run_id: RunId,
        /// Stable machine-readable failure code.
        code: String,
        /// Human-readable non-secret diagnostic.
        message: String,
    },
    /// The native host cancelled the run.
    Cancelled {
        /// Run receiving this event.
        run_id: RunId,
    },
}

/// Requests accepted by the authenticated companion control socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompanionControlRequest {
    /// List workflows exposed from the configured host directory.
    #[serde(rename = "workflow.list")]
    WorkflowList {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id chosen by the caller.
        request_id: String,
    },
    /// Start a native workflow bound to an explicitly shared tab.
    #[serde(rename = "workflow.start")]
    WorkflowStart {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id chosen by the caller.
        request_id: String,
        /// Host-local workflow id.
        workflow_id: String,
        /// Explicit shared tab owned by the initiating side panel or CLI.
        tab_id: TabId,
        /// Structured initial workflow input.
        #[serde(default)]
        input: Value,
    },
    /// Cancel a native workflow run.
    #[serde(rename = "workflow.cancel")]
    WorkflowCancel {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id chosen by the caller.
        request_id: String,
        /// Run to cancel.
        run_id: RunId,
    },
    /// Subscribe this authenticated connection to native run events.
    #[serde(rename = "run.subscribe")]
    RunSubscribe {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id chosen by the caller.
        request_id: String,
        /// Run whose events should be delivered.
        run_id: RunId,
    },
    /// List only tabs the user explicitly shared.
    #[serde(rename = "tab.list")]
    TabList {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id chosen by the caller.
        request_id: String,
    },
    /// Query paired relay connection status.
    #[serde(rename = "connection.status")]
    ConnectionStatus {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id chosen by the caller.
        request_id: String,
    },
}

impl CompanionControlRequest {
    /// Returns the declared protocol version.
    pub const fn protocol_version(&self) -> u32 {
        match self {
            Self::WorkflowList {
                protocol_version, ..
            }
            | Self::WorkflowStart {
                protocol_version, ..
            }
            | Self::WorkflowCancel {
                protocol_version, ..
            }
            | Self::RunSubscribe {
                protocol_version, ..
            }
            | Self::TabList {
                protocol_version, ..
            }
            | Self::ConnectionStatus {
                protocol_version, ..
            } => *protocol_version,
        }
    }

    /// Returns the request correlation id.
    pub fn request_id(&self) -> &str {
        match self {
            Self::WorkflowList { request_id, .. }
            | Self::WorkflowStart { request_id, .. }
            | Self::WorkflowCancel { request_id, .. }
            | Self::RunSubscribe { request_id, .. }
            | Self::TabList { request_id, .. }
            | Self::ConnectionStatus { request_id, .. } => request_id,
        }
    }
}

/// Correlated responses returned by the native companion control surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompanionControlResponse {
    /// Request completed successfully.
    Ok {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id copied from the request.
        request_id: String,
        /// Method-specific structured result.
        result: Value,
    },
    /// Request failed without exposing host credentials or internal state.
    Error {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id copied from the request.
        request_id: String,
        /// Stable machine-readable failure code.
        code: String,
        /// Human-readable non-secret diagnostic.
        message: String,
    },
    /// Workflow listing result.
    Workflows {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id copied from the request.
        request_id: String,
        /// Workflows exposed by the native host.
        workflows: Vec<WorkflowSummary>,
    },
    /// Explicit shared-tab listing result.
    Tabs {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id copied from the request.
        request_id: String,
        /// Only currently shared tabs.
        tabs: Vec<SharedTab>,
    },
    /// Paired extension connection status.
    Connection {
        /// Control protocol version.
        protocol_version: u32,
        /// Correlation id copied from the request.
        request_id: String,
        /// Whether an authenticated extension socket is live.
        connected: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn methods_use_dotted_names_and_reject_unknown_fields() {
        let request: CompanionControlRequest = serde_json::from_value(json!({
            "method": "workflow.start",
            "protocol_version": 1,
            "request_id": "control-1",
            "workflow_id": "login",
            "tab_id": 42,
            "input": {"query":"shoes"}
        }))
        .unwrap();
        assert_eq!(request.protocol_version(), 1);
        assert_eq!(request.request_id(), "control-1");

        assert!(
            serde_json::from_value::<CompanionControlRequest>(json!({
                "method": "tab.list", "protocol_version": 1,
                "request_id": "control-2", "include_unshared": true
            }))
            .is_err()
        );
    }
}

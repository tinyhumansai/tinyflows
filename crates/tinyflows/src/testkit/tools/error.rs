use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Why a tool call failed.
///
/// A closed set of stable snake_case codes: a caller — human or model — can
/// branch on the code, and the message is for reading rather than parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolErrorCode {
    /// No tool by that name.
    UnknownTool,
    /// The arguments were missing something required, or malformed.
    InvalidArguments,
    /// The supplied graph did not parse or did not compile.
    InvalidGraph,
    /// No such run — it was never recorded, or the registry was rebuilt.
    UnknownRun,
    /// No such debug session; it may have been reaped after being idle.
    UnknownSession,
    /// No activation is parked under that pause id, or it was already released.
    UnknownPause,
    /// The session could not be started.
    SessionFailed,
}

/// A failed tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolError {
    /// The machine-readable reason.
    pub code: ToolErrorCode,
    /// What went wrong, for a reader.
    pub message: String,
    /// Anything structured worth attaching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolError {
    /// A failure with a code and a message.
    #[must_use]
    pub fn new(code: ToolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Attach structured detail.
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

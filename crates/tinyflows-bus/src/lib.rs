//! Transport-free vocabulary for the loadable workflow authoring module.
//!
//! Graphs cross the boundary as JSON so hosts do not link the engine to read
//! authoring reports. Migration and interpretation belong to the module.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Module bus identity.
pub const BUS_NAME: &str = "ai.tinyhumans.tinyflows.Authoring";
/// Authoring interface name.
pub const INTERFACE: &str = "ai.tinyhumans.tinyflows.Authoring";
/// Served authoring object path.
pub const OBJECT_PATH: &str = "/ai/tinyhumans/tinyflows/Authoring";
/// Version of this wire contract.
pub const CONTRACT_VERSION: u32 = 1;
/// Authoring members provided by the module.
pub const METHODS: &[&str] = &[
    "ValidateGraph",
    "BindingFailures",
    "AuthoringFailures",
    "UnresolvableToolArgs",
    "BuilderFailures",
];

/// Context-bound host callback interface. It serves a saved-workflow lookup
/// and ordered product gates; the module never interprets host credentials.
pub const HOST_INTERFACE: &str = "ai.tinyhumans.tinyflows.AuthoringHost";

/// A host callback object scoped to this authoring request's identity/store.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AuthoringHost {
    /// Destination that owns the context-bound callback object.
    pub bus_name: String,
    /// Object implementing [`HOST_INTERFACE`].
    pub object_path: String,
}

/// Host callback members and gate stage strings.
pub mod host {
    /// `ResolveWorkflow(id: String) -> Option<Value>`; unavailable graphs are skipped.
    pub const RESOLVE_WORKFLOW: &str = "ResolveWorkflow";
    /// `CheckGate(stage: String, graph: Value) -> Vec<String>`; errors fail closed.
    pub const CHECK_GATE: &str = "CheckGate";
    /// Native-tool argument policy.
    pub const TOOL_POLICY: &str = "tool_policy";
    /// Host agent registry references.
    pub const AGENT_REFERENCES: &str = "agent_references";
    /// Connected-account references.
    pub const CONNECTIONS: &str = "connections";
    /// Tool catalog contracts.
    pub const TOOL_CONTRACTS: &str = "tool_contracts";
}

/// Stable authoring member names.
pub mod methods {
    /// Migrate, parse, structurally validate and check engine compatibility.
    pub const VALIDATE_GRAPH: &str = "ValidateGraph";
    /// Check bindings that omit the required node-output envelope.
    pub const BINDING_FAILURES: &str = "BindingFailures";
    /// Check portable binding, agent prompt/schema, and code-language gates.
    pub const AUTHORING_FAILURES: &str = "AuthoringFailures";
    /// Mock-run outbound arguments, skipping host-provided native slug prefixes.
    pub const UNRESOLVABLE_TOOL_ARGS: &str = "UnresolvableToolArgs";
    /// Ordered engine and host gates followed by mock argument preflight.
    pub const BUILDER_FAILURES: &str = "BuilderFailures";
}

/// One structured graph validation failure.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ValidationError {
    /// Stable failure classification.
    pub code: String,
    /// Author-facing explanation supplied by the engine.
    pub message: String,
    /// Offending node, when known.
    pub node_id: Option<String>,
    /// Offending field, when known.
    pub field: Option<String>,
}

/// Result of migration and graph validation without persistence or host I/O.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct GraphValidation {
    /// Migrated graph when parsing succeeded, including invalid graphs so an
    /// editor can keep the author's work while presenting errors.
    pub graph: Option<Value>,
    /// Every failure from the first failing stage (parse, structure, topology).
    pub errors: Vec<ValidationError>,
}

impl GraphValidation {
    /// Whether all engine validation stages succeeded.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.graph.is_some() && self.errors.is_empty()
    }
}

pub mod schedule;

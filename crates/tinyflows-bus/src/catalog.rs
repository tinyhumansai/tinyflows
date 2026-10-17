//! Wire contract for workflow definitions and their revision history.
//!
//! Storage remains host-owned. Each request carries the callback bound by the
//! host to one acting agent, and the module performs catalog operations over
//! that scoped document port.

use serde::{Deserialize, Serialize};

mod extended_types;
mod storage_types;
mod types;

pub use storage_types::{
    CatalogCollectionSpec, CatalogDocumentCursor, CatalogDocumentMeta, CatalogDocumentPage,
    CatalogDocumentPrecondition, CatalogFilter, CatalogIndexSpec, CatalogQuery, CatalogSearchSpec,
    CatalogSort, CatalogSortDirection, CatalogStorageCapabilities, CatalogStorageFailure,
    CatalogStorageReply, CatalogVersionedDocument, DocDeleteRequest, DocDeleteWhereRequest,
    DocEnsureRequest, DocGetRequest, DocPutRequest, DocQueryRequest,
};

pub use extended_types::*;
pub use types::{
    CallbackAddress, CatalogStorage, DeleteWorkflowResult, FlowUpdateError, GetRevisionRequest,
    ListRevisionsRequest, ListWorkflowsResult, PutWorkflowRequest, PutWorkflowResponse,
    PutWorkflowResult, SetEnabledRequest, Workflow, WorkflowRevision, WorkflowSummary,
};

/// Catalog interface on the shared module bus name.
pub const INTERFACE: &str = "ai.tinyhumans.tinyflows.Catalog";
/// Catalog object path.
pub const OBJECT_PATH: &str = "/ai/tinyhumans/tinyflows/Catalog";
/// Version of the served catalog contract. Version 5 adds scoped schema
/// migration and record-metadata support to the host document callback.
pub const CONTRACT_VERSION: u32 = 5;
/// Exact members served by the catalog module.
pub const METHODS: &[&str] = &[
    members::LIST_WORKFLOWS,
    members::GET_WORKFLOW,
    members::PUT_WORKFLOW,
    members::DELETE_WORKFLOW,
    members::SET_ENABLED,
    members::LIST_REVISIONS,
    members::GET_REVISION,
    members::CREATE_DRAFT,
    members::GET_DRAFT,
    members::UPDATE_DRAFT,
    members::LIST_DRAFTS,
    members::DELETE_DRAFT,
    members::PROMOTE_DRAFT,
    members::UPSERT_SUGGESTIONS,
    members::LIST_SUGGESTIONS,
    members::SET_SUGGESTION_STATUS,
    members::GET_RUN,
    members::LIST_RUNS,
    members::LIST_ALL_RUNS,
    members::PRUNE_RUNS,
    members::NODE_KINDS,
    members::APPLY_GRAPH_OPS,
    members::APPROVAL_MANIFEST,
    members::MIGRATE_STORAGE,
];

/// Callback interface implemented by the scope-owning host.
pub const STORAGE_INTERFACE: &str = "ai.tinyhumans.tinyflows.CatalogStorage";
/// Scope-bound host callback used only for draft promotion saves.
pub const DRAFT_PROMOTION_HOST_INTERFACE: &str = "ai.tinyhumans.tinyflows.DraftPromotionHost";
/// Stable members of the draft-promotion host callback.
pub mod draft_promotion_host {
    /// `(SavePromotedDraftRequest) -> SavePromotedDraftResponse`.
    pub const SAVE_PROMOTED_DRAFT: &str = "SavePromotedDraft";
    /// Methods accepted by the module when it calls the promotion host.
    pub const METHODS: &[&str] = &[SAVE_PROMOTED_DRAFT];
}

/// Stable member names, in [`METHODS`] order.
pub mod members {
    /// `(CatalogStorage) -> ListWorkflowsResult`.
    pub const LIST_WORKFLOWS: &str = "ListWorkflows";
    /// `(CatalogStorage, id) -> Option<Workflow>`.
    pub const GET_WORKFLOW: &str = "GetWorkflow";
    /// `(PutWorkflowRequest) -> PutWorkflowResponse`.
    pub const PUT_WORKFLOW: &str = "PutWorkflow";
    /// `(CatalogStorage, id) -> DeleteWorkflowResult`.
    pub const DELETE_WORKFLOW: &str = "DeleteWorkflow";
    /// `(SetEnabledRequest) -> Workflow`.
    pub const SET_ENABLED: &str = "SetEnabled";
    /// `(ListRevisionsRequest) -> WorkflowRevision[]`.
    pub const LIST_REVISIONS: &str = "ListRevisions";
    /// `(GetRevisionRequest) -> Option<WorkflowRevision>`.
    pub const GET_REVISION: &str = "GetRevision";
    /// `(CreateDraftRequest) -> FlowDraft`.
    pub const CREATE_DRAFT: &str = "CreateDraft";
    /// `(DraftIdRequest) -> Option<FlowDraft>`.
    pub const GET_DRAFT: &str = "GetDraft";
    /// `(UpdateDraftRequest) -> FlowDraft`.
    pub const UPDATE_DRAFT: &str = "UpdateDraft";
    /// `(CatalogStorage) -> FlowDraft[]`.
    pub const LIST_DRAFTS: &str = "ListDrafts";
    /// `(DraftIdRequest) -> bool`.
    pub const DELETE_DRAFT: &str = "DeleteDraft";
    /// `(PromoteDraftRequest) -> PromoteDraftResponse`.
    pub const PROMOTE_DRAFT: &str = "PromoteDraft";
    /// `(UpsertSuggestionsRequest) -> usize`.
    pub const UPSERT_SUGGESTIONS: &str = "UpsertSuggestions";
    /// `(ListSuggestionsRequest) -> FlowSuggestion[]`.
    pub const LIST_SUGGESTIONS: &str = "ListSuggestions";
    /// `(SetSuggestionStatusRequest) -> bool`.
    pub const SET_SUGGESTION_STATUS: &str = "SetSuggestionStatus";
    /// `(RunIdRequest) -> Option<FlowRun>`.
    pub const GET_RUN: &str = "GetRun";
    /// `(ListRunsRequest) -> FlowRun[]`.
    pub const LIST_RUNS: &str = "ListRuns";
    /// `(ListAllRunsRequest) -> FlowRun[]`.
    pub const LIST_ALL_RUNS: &str = "ListAllRuns";
    /// `(PruneRunsRequest) -> usize`.
    pub const PRUNE_RUNS: &str = "PruneRuns";
    /// `() -> NodeKindContract[]`.
    pub const NODE_KINDS: &str = "NodeKinds";
    /// `(ApplyGraphOpsRequest) -> ApplyGraphOpsResult`.
    pub const APPLY_GRAPH_OPS: &str = "ApplyGraphOps";
    /// `(ApprovalManifestRequest) -> Vec<Value>`.
    pub const APPROVAL_MANIFEST: &str = "ApprovalManifest";
    /// `(CatalogStorage) -> ()`; migrates this callback scope's v1 catalog data.
    pub const MIGRATE_STORAGE: &str = "MigrateStorage";
}

/// Pure structural graph edit request. The graph and tagged operations use
/// the engine's JSON representation so this contract has no engine dependency.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ApplyGraphOpsRequest {
    /// Base workflow graph.
    pub graph: serde_json::Value,
    /// Ordered `GraphOp` JSON objects.
    pub ops: Vec<serde_json::Value>,
}

/// Result of applying graph operations. On failure, `graph` is absent and the
/// input graph remains unchanged.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ApplyGraphOpsResult {
    /// Edited graph, present only when every operation applied.
    pub graph: Option<serde_json::Value>,
    /// Failure details for the first operation that could not be applied.
    pub error: Option<ApplyGraphOpsError>,
}

/// Stable operation failure fields for `ApplyGraphOps`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ApplyGraphOpsError {
    /// Zero-based operation index.
    pub index: usize,
    /// Stable snake-case operation name.
    pub op: String,
    /// Human-readable engine error details.
    pub details: String,
}

/// A possible permission prompt identified from a graph.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManifestCandidate {
    /// HTTP request node.
    Http {
        /// Graph node id.
        node_id: String,
        /// Configured URL or a generic label.
        url: String,
    },
    /// Sandboxed code node.
    Code {
        /// Graph node id.
        node_id: String,
    },
    /// Tool slug resolved from runtime data.
    DynamicTool {
        /// Graph node id.
        node_id: String,
    },
    /// Host-native tool call.
    NativeTool {
        /// Graph node id.
        node_id: String,
        /// Tool name with the host prefix stripped.
        tool_name: String,
        /// Tool arguments, which may contain unresolved bindings.
        args: serde_json::Value,
    },
    /// Connector action.
    ConnectorTool {
        /// Graph node id.
        node_id: String,
        /// Connector action slug.
        slug: String,
    },
    /// Agent with an inner harness turn.
    Agent {
        /// Graph node id.
        node_id: String,
    },
}

/// Host security-policy classification for a manifest candidate.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ManifestClass {
    /// Product-defined command class name.
    pub name: String,
    /// Whether policy blocks the action.
    pub blocked: bool,
    /// Whether a curated read action can be omitted from the manifest.
    pub skip: bool,
}

/// Scope-bound callback address used to classify manifest candidates.
pub const APPROVAL_MANIFEST_HOST_INTERFACE: &str = "ai.tinyhumans.tinyflows.ApprovalManifestHost";

/// Stable member name on [`APPROVAL_MANIFEST_HOST_INTERFACE`].
pub const CLASSIFY_MANIFEST_CANDIDATES: &str = "ClassifyManifestCandidates";

/// The host callback receives candidates in graph order and returns one
/// classification slot per candidate (`null` for dynamic and agent rows).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ClassifyManifestCandidatesRequest {
    /// Candidates in graph order.
    pub candidates: Vec<ManifestCandidate>,
}

/// Host classification aligned one-for-one with the request candidates.
pub type ClassifyManifestCandidatesResponse = Vec<Option<ManifestClass>>;

/// Request for the module to identify candidates, ask host policy to classify
/// them, and assemble the final manifest.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ApprovalManifestRequest {
    /// Candidate graph.
    pub graph: serde_json::Value,
    /// Prefix identifying host-native tool slugs.
    pub native_prefix: String,
    /// Scope-bound callback implemented by the host.
    pub host: CallbackAddress,
}

/// Callback members accepted by the generic document adapter.
pub mod storage {
    /// `() -> {value: CatalogStorageCapabilities}`.
    pub const CAPABILITIES: &str = "DocCapabilities";
    /// `(DocGetRequest) -> {value: Option<CatalogVersionedDocument>}`.
    pub const GET: &str = "DocGet";
    /// `(DocPutRequest) -> {value: u64}`.
    pub const PUT: &str = "DocPut";
    /// `(DocDeleteRequest) -> {value: bool}`.
    pub const DELETE: &str = "DocDelete";
    /// `(DocQueryRequest) -> {value: CatalogDocumentPage}`.
    pub const QUERY: &str = "DocQuery";
    /// `(DocEnsureRequest) -> {value: null}`.
    pub const ENSURE: &str = "DocEnsure";
    /// `(DocDeleteWhereRequest) -> {value: u64}`.
    pub const DELETE_WHERE: &str = "DocDeleteWhere";
}

/// Exact storage callback member set used by the served catalog.
pub const STORAGE_METHODS: &[&str] = &[
    storage::CAPABILITIES,
    storage::ENSURE,
    storage::GET,
    storage::PUT,
    storage::DELETE,
    storage::QUERY,
    storage::DELETE_WHERE,
];

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "catalog/extended_types_tests.rs"]
mod extended_tests;

#[cfg(test)]
#[path = "catalog_authoring_tests.rs"]
mod authoring_tests;

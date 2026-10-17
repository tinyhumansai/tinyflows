//! Serde-only requests and replies for the host's CatalogStorage callback.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Document write precondition used by the catalog storage callback.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "version", rename_all = "snake_case")]
pub enum CatalogDocumentPrecondition {
    /// Unconditional upsert.
    #[default]
    None,
    /// Insert only when the document is absent.
    Absent,
    /// Compare-and-swap against this observed document version.
    Version(u64),
}

/// A stored document and its storage version.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CatalogVersionedDocument {
    /// Document id.
    pub id: String,
    /// Storage version.
    pub version: u64,
    /// Document JSON object.
    pub doc: Value,
    /// Driver metadata; omitted by legacy callbacks that do not expose it.
    #[serde(default)]
    pub meta: CatalogDocumentMeta,
}

/// Record metadata returned with catalog document reads.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CatalogDocumentMeta {
    /// Declared body schema version, or zero when unknown.
    #[serde(default)]
    pub schema: u32,
    /// Original record creation time in Unix epoch milliseconds.
    #[serde(default)]
    pub created_ms: i64,
    /// Latest record update time in Unix epoch milliseconds.
    #[serde(default)]
    pub updated_ms: i64,
    /// Record expiry in Unix epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<i64>,
}

/// Cursor returned by a paged document query.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct CatalogDocumentCursor(pub String);

/// Stable filter vocabulary accepted by document queries and deletes.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CatalogFilter {
    /// Match every document.
    #[default]
    All,
    /// Match when the dotted path equals this JSON value.
    Eq { field: String, value: Value },
    /// Match when the dotted path is absent or differs from this value.
    Ne { field: String, value: Value },
    /// Match when the dotted path equals any supplied value.
    In { field: String, values: Vec<Value> },
    /// Match when the path lies inside the inclusive/exclusive bounds.
    Range {
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gt: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gte: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lt: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lte: Option<Value>,
    },
    /// Match when the path is present or absent.
    Exists { field: String, exists: bool },
    /// Match when every child filter matches.
    And { filters: Vec<CatalogFilter> },
    /// Match when any child filter matches.
    Or { filters: Vec<CatalogFilter> },
    /// Match when the child filter does not match.
    Not { filter: Box<CatalogFilter> },
}

/// Sort direction for a catalog document query.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSortDirection {
    /// Smallest values first.
    #[default]
    Asc,
    /// Largest values first.
    Desc,
}

/// One stable sort key for a catalog document query.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CatalogSort {
    /// Dotted field path or `_id`.
    pub field: String,
    /// Sort direction (ascending when omitted).
    #[serde(default)]
    pub direction: CatalogSortDirection,
}

/// Query expression accepted by `DocQuery`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct CatalogQuery {
    /// Filter to apply.
    #[serde(default)]
    pub filter: CatalogFilter,
    /// Ordered sort keys.
    #[serde(default)]
    pub sort: Vec<CatalogSort>,
    /// Maximum page size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Opaque continuation cursor returned by the prior page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CatalogDocumentCursor>,
}

/// One page returned by `DocQuery`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CatalogDocumentPage {
    /// Documents in query order.
    pub items: Vec<CatalogVersionedDocument>,
    /// Cursor for the next page, if any.
    pub next: Option<CatalogDocumentCursor>,
}

/// Secondary-index declaration accepted by `DocEnsure`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CatalogIndexSpec {
    /// Index name.
    pub name: String,
    /// Dotted fields in key order.
    pub fields: Vec<String>,
    /// Whether duplicate key values are rejected.
    #[serde(default)]
    pub unique: bool,
}

/// Full-text fields declared by `DocEnsure`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CatalogSearchSpec {
    /// Dotted searchable fields.
    pub fields: Vec<String>,
}

/// Collection shape passed to `DocEnsure`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CatalogCollectionSpec {
    /// Collection name.
    pub name: String,
    /// Secondary indexes.
    #[serde(default)]
    pub indexes: Vec<CatalogIndexSpec>,
    /// Optional expiry timestamp field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_field: Option<String>,
    /// Optional full-text search declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<CatalogSearchSpec>,
}

/// Request body for `DocEnsure`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DocEnsureRequest {
    /// Collection and index declaration.
    pub spec: CatalogCollectionSpec,
}

/// Request body for `DocGet`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DocGetRequest {
    /// Allowlisted catalog collection.
    pub collection: String,
    /// Document id.
    pub id: String,
}

/// Request body for `DocPut`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DocPutRequest {
    /// Allowlisted catalog collection.
    pub collection: String,
    /// Document id.
    pub id: String,
    /// Document content.
    pub doc: Value,
    /// Storage-driver precondition in its stable JSON shape.
    pub precondition: CatalogDocumentPrecondition,
    /// The body version the writer declares (storage schema); `0` means
    /// undeclared and is omitted from the wire.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub schema: u32,
    /// Unix epoch milliseconds from which the document reads as absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<i64>,
    /// Original record creation time, used only by schema migrations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_ms: Option<i64>,
}

/// Optional abilities of the host document callback.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CatalogStorageCapabilities {
    /// Whether document reads and writes preserve driver record metadata,
    /// including explicit creation times used by migrations.
    #[serde(default)]
    pub record_metadata: bool,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Request body for `DocDelete`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DocDeleteRequest {
    /// Allowlisted catalog collection.
    pub collection: String,
    /// Document id.
    pub id: String,
    /// Storage-driver precondition in its stable JSON shape.
    pub precondition: CatalogDocumentPrecondition,
}

/// Request body for `DocQuery`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DocQueryRequest {
    /// Allowlisted catalog collection.
    pub collection: String,
    /// Filter, sort and pagination.
    pub query: CatalogQuery,
}

/// Request body for `DocDeleteWhere`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DocDeleteWhereRequest {
    /// Allowlisted catalog collection.
    pub collection: String,
    /// Filter.
    pub filter: CatalogFilter,
}

/// Error returned by a storage callback; `kind` is a stable storage error tag.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CatalogStorageFailure {
    /// One of conflict, already_exists, not_found, invalid_input,
    /// serialization, unavailable, or backend.
    pub kind: String,
    /// Safe error description without the document value that caused it.
    pub message: String,
}

/// Standard storage reply envelope for a value or a structured failure.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum CatalogStorageReply {
    /// Successful document-driver result in the driver's JSON shape.
    Value { value: Value },
    /// Failure without the value that caused it.
    Error { error: CatalogStorageFailure },
}

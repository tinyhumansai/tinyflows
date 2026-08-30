//! The on-disk workflow document: reading it, writing it, checking it.
//!
//! A document is the engine's `WorkflowGraph` JSON with this host's own fields
//! (`id`, `name`, `description`, `enabled`, `defaults`) merged in beside it,
//! rather than nested under a wrapper. That shape is deliberate: a file an operator opens
//! reads as a graph, and a graph exported from anywhere else loads here without
//! being re-wrapped.

use std::path::Path;

use crate::model::WorkflowGraph;
use serde_json::Value;

use crate::store::types::{RunRecord, RunStatus, WorkflowDefaults, WorkflowError, WorkflowRecord};

/// Read and parse one workflow document, naming errors by path.
pub fn read_workflow(path: &Path) -> Result<WorkflowRecord, String> {
    read_workflow_with(path, &EnginePolicy)
}

/// [`read_workflow`], judging the `defaults` block by a host's policy.
pub fn read_workflow_with(path: &Path, policy: &dyn HostPolicy) -> Result<WorkflowRecord, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let mut record = parse_workflow_with(&text, stem, policy)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    // A document can deserialize cleanly and still be a graph the engine will
    // not compile — no trigger, an edge to a node that is not there. Catching
    // it here means a listing only ever shows workflows that would actually
    // run, and the operator hears about the broken file by name.
    validate_graph(&record.id, &record.graph)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    record.source_path = Some(path.to_path_buf());
    Ok(record)
}

/// Parse one workflow document.
///
/// The document is the engine's `WorkflowGraph` JSON with optional host fields
/// (`description`, `enabled`, `defaults`) alongside it. `id` defaults to
/// `id_fallback` — the filename, for a file — and `name` to the id, so the
/// smallest useful document is a set of nodes and edges.
///
/// The pipeline is the engine's documented one: migrate the persisted JSON to
/// the current schema *before* deserializing, so a definition saved by an older
/// build keeps loading.
pub fn parse_workflow(text: &str, id_fallback: &str) -> Result<WorkflowRecord, String> {
    parse_workflow_with(text, id_fallback, &EnginePolicy)
}

/// The judgements about a workflow that only its host can make.
///
/// Two of them, and they have the same shape of reason. A `defaults` block's
/// `harness` and `model` are opaque strings to this crate — which harnesses
/// exist, and what names them, is the embedding application's vocabulary. So is
/// which tool slugs resolve, or which integrations are installed, which is the
/// kind of thing a host's own authoring gate refuses.
///
/// Both are checked *at the boundary*: a document naming a harness the host
/// does not have must fail loudly at load, not quietly at dispatch, and an edit
/// that a host gate would refuse must never reach the disk. So the rules are the
/// host's to supply, and the store's to run.
///
/// The default implementations are the honest answer for a host with no such
/// vocabulary: accept any `defaults`, and apply the engine's own gates
/// ([`crate::gates`]) and nothing more.
pub trait HostPolicy: std::fmt::Debug + Send + Sync {
    /// Accept `defaults`, or say in one sentence what is wrong with it.
    ///
    /// # Errors
    /// Returns the sentence shown to whoever tried to load or save the
    /// document.
    fn check_defaults(&self, defaults: &WorkflowDefaults) -> Result<(), String> {
        let _ = defaults;
        Ok(())
    }

    /// Accept `graph` as an authoring write, or list everything wrong with it.
    ///
    /// A host overriding this should run [`crate::gates::failures`] as well as
    /// its own — the engine's gates catch the mistakes that are wrong on any
    /// host, and dropping them would be a silent loss.
    ///
    /// # Errors
    /// Returns [`WorkflowError::Invalid`] listing every failure, so one round
    /// trip tells an author everything rather than one thing at a time.
    fn check_graph(&self, id: &str, graph: &WorkflowGraph) -> Result<(), WorkflowError> {
        gate_failures_into_error(id, crate::gates::failures(graph))
    }
}

/// Turn a gate failure list into the error every authoring path reports.
///
/// Public because a host overriding [`HostPolicy::check_graph`] has to build the
/// same error from its own combined list, and rebuilding it by hand is how the
/// two drift.
///
/// # Errors
/// Returns [`WorkflowError::Invalid`] when `failures` is non-empty.
pub fn gate_failures_into_error(id: &str, failures: Vec<String>) -> Result<(), WorkflowError> {
    if failures.is_empty() {
        return Ok(());
    }
    Err(WorkflowError::Invalid {
        id: id.to_string(),
        messages: failures,
    })
}

/// The policy for a host with no vocabulary of its own.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnginePolicy;

impl HostPolicy for EnginePolicy {}

/// [`parse_workflow`], judging the `defaults` block by a host's policy.
pub fn parse_workflow_with(
    text: &str,
    id_fallback: &str,
    policy: &dyn HostPolicy,
) -> Result<WorkflowRecord, String> {
    let raw: Value = serde_json::from_str(text).map_err(|err| format!("invalid JSON: {err}"))?;
    let migrated = crate::migrate::migrate(raw).map_err(|err| err.to_string())?;

    let object = migrated
        .as_object()
        .ok_or_else(|| "workflow document must be a JSON object".to_string())?;

    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or(id_fallback)
        .to_string();
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let enabled = object
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    // Parsed before the graph, because parsing consumes `migrated`. An
    // unreadable `defaults` block is a hard error rather than an ignored one: a
    // workflow that meant to run on Codex and silently ran on the host default
    // is exactly the kind of quiet wrongness this store exists to refuse.
    let defaults: WorkflowDefaults = match object.get("defaults") {
        Some(Value::Null) | None => WorkflowDefaults::default(),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|err| format!("invalid `defaults`: {err}"))?,
    };
    policy
        .check_defaults(&defaults)
        .map_err(|err| format!("invalid `defaults`: {err}"))?;

    let graph: WorkflowGraph =
        serde_json::from_value(migrated).map_err(|err| format!("invalid workflow: {err}"))?;
    let name = if graph.name.is_empty() {
        id.clone()
    } else {
        graph.name.clone()
    };

    Ok(WorkflowRecord {
        id,
        name,
        description,
        enabled,
        defaults,
        graph,
        source_path: None,
    })
}

/// Run the engine's validation, collecting every failure rather than the first.
///
/// One round-trip then tells an author everything wrong with their graph, which
/// matters most when the author is an agent editing over a tool call.
pub fn validate_graph(id: &str, graph: &WorkflowGraph) -> Result<(), WorkflowError> {
    let errors = crate::validate::validate_all(graph);
    if errors.is_empty() {
        return Ok(());
    }
    Err(WorkflowError::Invalid {
        id: id.to_string(),
        messages: errors
            .iter()
            .map(|err| match err.node_id() {
                Some(node) => format!("[{}] {node}: {err}", err.code()),
                None => format!("[{}] {err}", err.code()),
            })
            .collect(),
    })
}

/// Serialize a record into the on-disk document shape: the graph, with the host
/// fields merged in beside it.
pub fn to_document(record: &WorkflowRecord) -> Result<Vec<u8>, WorkflowError> {
    let mut value = serde_json::to_value(&record.graph)
        .map_err(|err| WorkflowError::Malformed(err.to_string()))?;
    if let Some(object) = value.as_object_mut() {
        object.insert("id".into(), Value::String(record.id.clone()));
        object.insert("name".into(), Value::String(record.name.clone()));
        object.insert(
            "description".into(),
            Value::String(record.description.clone()),
        );
        object.insert("enabled".into(), Value::Bool(record.enabled));
        // Omitted entirely when the workflow states no preference, so an
        // untouched document does not grow a block of nulls.
        if !record.defaults.is_empty() {
            let defaults = serde_json::to_value(&record.defaults)
                .map_err(|err| WorkflowError::Malformed(err.to_string()))?;
            object.insert("defaults".into(), defaults);
        } else {
            object.remove("defaults");
        }
    }
    serde_json::to_vec_pretty(&value).map_err(|err| WorkflowError::Malformed(err.to_string()))
}

/// A run record for a run that has just started.
pub fn new_run_record(id: &str, workflow_id: &str, started_at: u64) -> RunRecord {
    RunRecord {
        id: id.to_string(),
        workflow_id: workflow_id.to_string(),
        status: RunStatus::Running,
        started_at,
        finished_at: None,
        steps: Vec::new(),
        pending_approvals: Vec::new(),
        error: None,
        // Supplied by the caller through `RunRecord::with_inputs` and
        // `with_origin`, which every real door does. A record built without
        // them is still honest — it simply says nothing about what it was
        // started with, which is what an older record says too.
        inputs: serde_json::Map::new(),
        trigger: None,
        origin: None,
        // Stamped by the caller through `RunRecord::with_executor`. Left unset
        // here so this factory stays usable in tests and tools that are not
        // actually executing anything; an unstamped record is treated as
        // unowned, which is the honest reading.
        executor: None,
        // Nobody has asked a run to stop before it has begun.
        cancel_requested: false,
        // Both are evidence about a run that has ended, so a run that has only
        // just started has neither. They are filled in when it settles.
        summary: None,
        diagnosis: None,
    }
}

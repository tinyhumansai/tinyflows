//! Machine-readable authoring contracts for the node kinds — the queryable DSL
//! schema.
//!
//! A [`Node`](crate::model::Node)'s `config` is free-form
//! [`serde_json::Value`](serde_json::Value): each executor reads the keys it
//! needs at run time, so the per-kind config *shape* was, until now, documented
//! only in prose in downstream hosts. This module makes that shape a typed,
//! host-agnostic **source of truth** — one [`NodeKindContract`] per
//! [`NodeKind`](crate::model::NodeKind) — so a host (or an agent authoring a
//! graph) can enumerate the kinds and fetch one kind's config fields, ports, an
//! example node, and the authoring gotchas without reading a prompt.
//!
//! **Host-agnostic by construction** (the crate's core rule): these contracts
//! describe only what the tinyflows model and executors define. Anything a
//! specific host layers on top of the opaque fields — what a `tool_call` slug
//! resolves to, how its output is wrapped, which trigger kinds actually
//! dispatch — is deliberately *not* here; a host augments these contracts with
//! its own notes.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[path = "catalog/contracts/group_01.rs"]
mod group_01;
#[path = "catalog/contracts/group_02.rs"]
mod group_02;
#[path = "catalog/contracts/group_03.rs"]
mod group_03;

use group_01::*;
use group_02::*;
use group_03::*;

/// The node kinds, in the canonical order used wherever the DSL is enumerated
/// (matches [`NodeKind`](crate::model::NodeKind)'s serde discriminators).
pub const NODE_KINDS: [&str; 22] = [
    "trigger",
    "agent",
    "tool_call",
    "http_request",
    "code",
    "shell",
    "condition",
    "switch",
    "merge",
    "split_out",
    "transform",
    "output_parser",
    "sub_workflow",
    "memory",
    "dedup",
    "loop",
    "spawn",
    "gate",
    "scatter",
    "gather",
    "approval",
    "void",
];

/// One config field a node of a given kind reads at run time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigField {
    /// The `config.<name>` key.
    pub name: String,
    /// Whether the node is malformed / a no-op without it.
    pub required: bool,
    /// A human-readable value-shape hint (`string`, `object`, `"=expr"`,
    /// `enum`, `WorkflowGraph`, …) — descriptive, not a JSON Schema `type`.
    pub value_type: String,
    /// What the field means and how to fill it.
    pub description: String,
    /// The allowed values, when the field is a closed enum (e.g.
    /// `trigger_kind`, `code.language`); `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
}

impl ConfigField {
    /// A required config field.
    pub fn required(name: &str, value_type: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            required: true,
            value_type: value_type.to_string(),
            description: description.to_string(),
            enum_values: None,
        }
    }

    /// An optional config field.
    pub fn optional(name: &str, value_type: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            required: false,
            value_type: value_type.to_string(),
            description: description.to_string(),
            enum_values: None,
        }
    }

    /// Marks this field a closed enum with the given allowed values.
    #[must_use]
    pub fn with_enum(mut self, values: &[&str]) -> Self {
        self.enum_values = Some(values.iter().map(|s| s.to_string()).collect());
        self
    }
}

/// The input/output ports a node exposes. Routing is keyed exclusively on the
/// source node's `from_port` (see [`crate::validate`]'s condition-routing
/// check), so the output-port list is what an author wires branch edges onto.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PortSpec {
    /// Named input ports. Almost always just `["main"]`.
    pub inputs: Vec<String>,
    /// Named output ports. `["main"]` for a linear node; `["true","false"]`
    /// for `condition`; case ports + `"default"` for `switch`. Every node can
    /// additionally emit on `"error"` when its `on_error` policy is `"route"`.
    pub outputs: Vec<String>,
}

impl PortSpec {
    /// One `main` input and one `main` output — the shape of every linear node.
    #[must_use]
    pub fn linear() -> Self {
        Self {
            inputs: vec!["main".to_string()],
            outputs: vec!["main".to_string()],
        }
    }

    /// Custom input/output port lists.
    #[must_use]
    pub fn new(inputs: &[&str], outputs: &[&str]) -> Self {
        Self {
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// The full machine-readable contract for one node kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeKindContract {
    /// The kind discriminator (`trigger`, `agent`, …) — the `kind` field value.
    pub kind: String,
    /// One-line summary, safe to render in a compact list.
    pub summary: String,
    /// Fuller description of the node's role and how to author it.
    pub description: String,
    /// The `config.*` fields this kind reads.
    pub config_fields: Vec<ConfigField>,
    /// Input/output ports.
    pub ports: PortSpec,
    /// A complete, valid example node (`{id, kind, name, config}`).
    pub example: Value,
    /// Authoring gotchas that bite in practice (envelope semantics, the
    /// `from_port` branch rule, the `sub_workflow` XOR, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl NodeKindContract {
    /// Appends a host-specific caveat to this contract's [`notes`](Self::notes),
    /// returning the modified contract.
    ///
    /// The mechanism a host uses to augment the portable contract with facts it
    /// owns — how a `tool_call` slug resolves, how output is wrapped, which
    /// triggers dispatch — without editing the crate.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

/// All node-kind contracts, in [`NODE_KINDS`] order.
pub fn all_contracts() -> Vec<NodeKindContract> {
    NODE_KINDS
        .iter()
        .map(|k| contract_for(k).expect("every NODE_KINDS entry has a contract"))
        .collect()
}

/// The contract for one node kind, or `None` if `kind` is not catalogued.
pub fn contract_for(kind: &str) -> Option<NodeKindContract> {
    let c = match kind {
        "trigger" => contract_trigger(),
        "agent" => contract_agent(),
        "tool_call" => contract_tool_call(),
        "http_request" => contract_http_request(),
        "code" => contract_code(),
        "shell" => contract_shell(),
        "condition" => contract_condition(),
        "switch" => contract_switch(),
        "merge" => contract_merge(),
        "split_out" => contract_split_out(),
        "loop" => contract_loop(),
        "transform" => contract_transform(),
        "output_parser" => contract_output_parser(),
        "sub_workflow" => contract_sub_workflow(),
        "memory" => contract_memory(),
        "dedup" => contract_dedup(),
        "spawn" => contract_spawn(),
        "gate" => contract_gate(),
        "scatter" => contract_scatter(),
        "gather" => contract_gather(),
        "approval" => contract_approval(),
        "void" => contract_void(),
        _ => return None,
    };
    Some(with_fan_out_fields(c))
}

/// The node kinds that map over their input, and whether they do so by default.
///
/// `true` means the kind is `per_item` unless told otherwise, so its fan-out
/// knobs apply without an explicit `execution`.
const FAN_OUT_KINDS: [(&str, bool); 5] = [
    ("agent", false),
    ("tool_call", true),
    ("http_request", true),
    ("memory", true),
    ("sub_workflow", false),
];

/// Appends the shared per-item fan-out contract (`execution`, `concurrency`,
/// `on_item_error`) to the kinds that support it.
///
/// These three keys behave identically on every mapping kind, so they are
/// described once here rather than copied into five contracts that would then
/// drift. Kinds that cannot map over their input are returned untouched — and
/// [`crate::validate`] rejects the keys there, so the contract and the validator
/// agree on exactly which kinds fan out.
fn with_fan_out_fields(mut c: NodeKindContract) -> NodeKindContract {
    let Some((_, per_item_by_default)) = FAN_OUT_KINDS.iter().find(|(k, _)| *k == c.kind) else {
        return c;
    };
    let default_mode = if *per_item_by_default {
        "per_item"
    } else {
        "once"
    };

    c.config_fields.push(
        ConfigField::optional(
            "execution",
            "enum",
            &format!(
                "Whether this node runs once for the whole input array or once per input item. \
                 Defaults to \"{default_mode}\" for this kind."
            ),
        )
        .with_enum(&["once", "per_item"]),
    );
    c.config_fields.push(ConfigField::optional(
        "concurrency",
        "integer | \"all\"",
        "With execution \"per_item\", how many items run at a time: 1 (the default) is strictly \
         sequential, n runs at most n at once, and 0 or \"all\" runs every item at once. This is \
         the fan-out dial — use it to turn an array of work into parallel work. Ignored (and \
         rejected by validation) unless the node runs per item.",
    ));
    c.config_fields.push(
        ConfigField::optional(
            "on_item_error",
            "enum",
            "What a failing item does to the batch. Defaults to \"collect\" when the node fans \
             out (concurrency other than 1) and \"fail_fast\" when it runs sequentially. \
             \"collect\" emits an error item — {json:{error,failed:true}} — in that item's slot so \
             the node still returns one output per input and a downstream condition can branch on \
             =item.json.failed. \"fail_fast\" fails the node on the first error in input order, \
             handing it to the node's on_error/retry policy. \"skip\" drops failed items, so the \
             output array may be shorter than the input.",
        )
        .with_enum(&["collect", "fail_fast", "skip"]),
    );

    c.notes.push(
        "Output items are always returned in INPUT order with paired_item set, however the \
         concurrency is set — a fan-out never reorders data."
            .to_string(),
    );
    c
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "fan_out_contract_tests.rs"]
mod fan_out_contract_tests;

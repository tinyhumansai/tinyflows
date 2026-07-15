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

/// The node kinds, in the canonical order used wherever the DSL is enumerated
/// (matches [`NodeKind`](crate::model::NodeKind)'s serde discriminators).
pub const NODE_KINDS: [&str; 12] = [
    "trigger",
    "agent",
    "tool_call",
    "http_request",
    "code",
    "condition",
    "switch",
    "merge",
    "split_out",
    "transform",
    "output_parser",
    "sub_workflow",
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

/// The contract for one node kind, or `None` if `kind` is not one of the 12.
pub fn contract_for(kind: &str) -> Option<NodeKindContract> {
    let c = match kind {
        "trigger" => NodeKindContract {
            kind: "trigger".to_string(),
            summary: "The single entry point of the flow (exactly one required).".to_string(),
            description: "Every graph has exactly one trigger. config.trigger_kind selects how it \
                fires; whether a given kind actually dispatches unattended is a host concern."
                .to_string(),
            config_fields: vec![
                ConfigField::required(
                    "trigger_kind",
                    "enum",
                    "How the flow fires. manual = run on demand; schedule = a timer; the rest are \
                     event/host-driven.",
                )
                .with_enum(&[
                    "manual",
                    "schedule",
                    "webhook",
                    "app_event",
                    "form",
                    "chat_message",
                    "evaluation",
                    "system",
                    "execute_by_workflow",
                ]),
                ConfigField::optional(
                    "schedule",
                    "object",
                    "Required when trigger_kind=schedule: {kind:\"cron\",expr,tz?} | \
                     {kind:\"at\",at} | {kind:\"every\",every_ms}.",
                ),
            ],
            ports: PortSpec::new(&[], &["main"]),
            example: json!({
                "id": "t", "kind": "trigger", "name": "Every morning",
                "config": { "trigger_kind": "schedule", "schedule": { "kind": "cron", "expr": "0 9 * * *" } }
            }),
            notes: vec![
                "Exactly ONE trigger node per graph — zero or multiple is a hard reject."
                    .to_string(),
            ],
        },
        "agent" => NodeKindContract {
            kind: "agent".to_string(),
            summary: "An LLM step, run via the host's LlmProvider capability.".to_string(),
            description: "Runs config.prompt through the injected LlmProvider. config.agent_ref \
                may select a host-registered agent persona; config.output_parser.schema requests a \
                structured, field-addressable output item. The exact data-input convention (how \
                the upstream item reaches the prompt) is defined by the host's LlmProvider."
                .to_string(),
            config_fields: vec![
                ConfigField::required("prompt", "string", "The instruction sent to the model."),
                ConfigField::optional(
                    "agent_ref",
                    "string",
                    "A host-registered agent-kind id to run this step as (persona/model).",
                ),
                ConfigField::optional(
                    "output_parser",
                    "object",
                    "Set output_parser.schema (a JSON Schema object) to coerce the output into a \
                     structured item whose fields downstream nodes can address; without it the \
                     agent emits {text:\"...\"} only.",
                ),
                ConfigField::optional(
                    "connection_ref",
                    "string",
                    "An opaque connection reference passed to the LlmProvider, when the host needs \
                     one.",
                ),
            ],
            ports: PortSpec::linear(),
            example: json!({
                "id": "classify", "kind": "agent", "name": "Classify",
                "config": {
                    "prompt": "Classify the message as urgent, normal, or low priority.",
                    "output_parser": { "schema": { "type": "object", "properties": { "priority": { "type": "string" } } } }
                }
            }),
            notes: vec![
                "If the output feeds a condition, declare that field \"type\":\"boolean\" in \
                 output_parser.schema — an untyped field can carry the truthy string \"false\" and \
                 route to the wrong port."
                    .to_string(),
            ],
        },
        "tool_call" => NodeKindContract {
            kind: "tool_call".to_string(),
            summary: "Invoke a tool via the host's ToolInvoker capability.".to_string(),
            description: "config.slug names the tool (opaque to the engine — the host resolves \
                it); config.args are the arguments; config.connection_ref is an opaque account \
                reference. What slugs exist, their arg schemas, and how their output is shaped are \
                host concerns."
                .to_string(),
            config_fields: vec![
                ConfigField::required(
                    "slug",
                    "string",
                    "The tool identifier, resolved by the host's ToolInvoker.",
                ),
                ConfigField::optional(
                    "args",
                    "object",
                    "Arguments passed to the tool. Values may be literals or =bindings.",
                ),
                ConfigField::optional(
                    "connection_ref",
                    "string",
                    "An opaque connected-account reference the host resolves.",
                ),
            ],
            ports: PortSpec::linear(),
            example: json!({
                "id": "act", "kind": "tool_call", "name": "Do the thing",
                "config": { "slug": "SOME_TOOL_ACTION", "args": { "to": "=nodes.pick.item.json.email" } }
            }),
            notes: vec![],
        },
        "http_request" => NodeKindContract {
            kind: "http_request".to_string(),
            summary: "A raw HTTP call via the host's HttpClient capability.".to_string(),
            description: "config.method + config.url, with optional headers/body. \
                config.connection_ref may reference a host credential for auth."
                .to_string(),
            config_fields: vec![
                ConfigField::required("method", "string", "HTTP method, e.g. GET / POST."),
                ConfigField::required(
                    "url",
                    "string",
                    "The request URL (may be a =binding or contain =interpolated parts).",
                ),
                ConfigField::optional("headers", "object", "Request headers."),
                ConfigField::optional("body", "any", "Request body (object or string)."),
                ConfigField::optional(
                    "connection_ref",
                    "string",
                    "An opaque credential reference for authentication.",
                ),
            ],
            ports: PortSpec::linear(),
            example: json!({
                "id": "fetch", "kind": "http_request", "name": "Fetch",
                "config": { "method": "GET", "url": "https://api.example.com/items" }
            }),
            notes: vec![],
        },
        "code" => NodeKindContract {
            kind: "code".to_string(),
            summary: "Run a sandboxed JavaScript or Python snippet.".to_string(),
            description: "config.language + config.source, run via the host's CodeRunner \
                capability."
                .to_string(),
            config_fields: vec![
                ConfigField::required("language", "enum", "The runtime language.")
                    .with_enum(&["javascript", "python"]),
                ConfigField::required("source", "string", "The code to run."),
            ],
            ports: PortSpec::linear(),
            example: json!({
                "id": "shape", "kind": "code", "name": "Shape",
                "config": { "language": "javascript", "source": "return { total: item.a + item.b };" }
            }),
            notes: vec![],
        },
        "condition" => NodeKindContract {
            kind: "condition".to_string(),
            summary: "A boolean gate that routes to the `true` or `false` port.".to_string(),
            description: "Evaluates config.field and routes on from_port \"true\" or \"false\". \
                Wire both branches (or the unwired one dead-ends)."
                .to_string(),
            config_fields: vec![ConfigField::required(
                "field",
                "\"=expr\"",
                "The boolean expression/field to gate on.",
            )],
            ports: PortSpec::new(&["main"], &["true", "false"]),
            example: json!({
                "id": "gate", "kind": "condition", "name": "Urgent?",
                "config": { "field": "=nodes.classify.item.json.priority == \"urgent\"" }
            }),
            notes: vec![
                "HARD RULE: the branch label goes on the edge's from_port, e.g. \
                 {from_node:\"gate\",from_port:\"true\",to_node:\"x\",to_port:\"main\"}. Putting \
                 the label on to_port instead silently turns the branch into an unconditional \
                 fan-out (BOTH branches run) and is a hard reject."
                    .to_string(),
            ],
        },
        "switch" => NodeKindContract {
            kind: "switch".to_string(),
            summary: "Multi-way routing to the matching case port, else `default`.".to_string(),
            description: "Evaluates config.expression (or config.field) and routes on from_port \
                equal to the matching case value, falling back to the \"default\" port."
                .to_string(),
            config_fields: vec![
                ConfigField::optional(
                    "expression",
                    "\"=expr\"",
                    "The expression whose value selects the case port. Provide this OR field.",
                ),
                ConfigField::optional(
                    "field",
                    "\"=expr\"",
                    "A field whose value selects the case port. Provide this OR expression.",
                ),
            ],
            ports: PortSpec::new(&["main"], &["<case>…", "default"]),
            example: json!({
                "id": "route", "kind": "switch", "name": "By type",
                "config": { "field": "=item.type" }
            }),
            notes: vec![
                "Like condition, case labels go on the edge's from_port; to_port stays \"main\"."
                    .to_string(),
            ],
        },
        "merge" => NodeKindContract {
            kind: "merge".to_string(),
            summary: "A fan-in barrier that passes its inputs through.".to_string(),
            description: "Waits for its inbound branches and passes the collected items through. \
                No config."
                .to_string(),
            config_fields: vec![],
            ports: PortSpec::linear(),
            example: json!({ "id": "join", "kind": "merge", "name": "Join" }),
            notes: vec![],
        },
        "split_out" => NodeKindContract {
            kind: "split_out".to_string(),
            summary: "Fan out one item per element of an array field.".to_string(),
            description: "config.path names an array within the current item; the node emits one \
                item per element."
                .to_string(),
            config_fields: vec![ConfigField::required(
                "path",
                "string",
                "Dotted path to the array field to fan out over, e.g. \"json.data.messages\".",
            )],
            ports: PortSpec::linear(),
            example: json!({
                "id": "each", "kind": "split_out", "name": "Each item",
                "config": { "path": "json.items" }
            }),
            notes: vec![],
        },
        "transform" => NodeKindContract {
            kind: "transform".to_string(),
            summary: "Merge computed keys onto each item.".to_string(),
            description: "config.set = { key: \"=expr\" } — each expression is evaluated and \
                merged onto every item flowing through."
                .to_string(),
            config_fields: vec![ConfigField::required(
                "set",
                "object",
                "A map of output key -> \"=expr\" merged onto each item.",
            )],
            ports: PortSpec::linear(),
            example: json!({
                "id": "enrich", "kind": "transform", "name": "Add name",
                "config": { "set": { "full_name": "=item.first + \" \" + item.last" } }
            }),
            notes: vec![],
        },
        "output_parser" => NodeKindContract {
            kind: "output_parser".to_string(),
            summary: "Passthrough today; no config required.".to_string(),
            description: "A passthrough node reserved for structured-output parsing. Requires no \
                config."
                .to_string(),
            config_fields: vec![],
            ports: PortSpec::linear(),
            example: json!({ "id": "parse", "kind": "output_parser", "name": "Parse" }),
            notes: vec![],
        },
        "sub_workflow" => NodeKindContract {
            kind: "sub_workflow".to_string(),
            summary: "Run a child workflow — inline or by reference.".to_string(),
            description: "References its child EXACTLY one way: config.workflow (an inline child \
                WorkflowGraph) OR config.workflow_id (resolved by the host's WorkflowResolver) — \
                never both, never neither."
                .to_string(),
            config_fields: vec![
                ConfigField::optional(
                    "workflow",
                    "WorkflowGraph",
                    "An inline child graph. Provide this OR workflow_id, not both.",
                ),
                ConfigField::optional(
                    "workflow_id",
                    "string",
                    "The id of a saved workflow to run as the child. Provide this OR workflow.",
                ),
            ],
            ports: PortSpec::linear(),
            example: json!({
                "id": "sub", "kind": "sub_workflow", "name": "Enrich",
                "config": { "workflow_id": "flow-123" }
            }),
            notes: vec![
                "Exactly one of workflow / workflow_id — having both or neither is a hard reject."
                    .to_string(),
            ],
        },
        _ => return None,
    };
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeKind;

    #[test]
    fn every_node_kind_has_a_contract() {
        for kind in NODE_KINDS {
            let c = contract_for(kind).unwrap_or_else(|| panic!("no contract for {kind}"));
            assert_eq!(c.kind, kind);
            assert!(!c.summary.is_empty(), "{kind} has empty summary");
            assert!(!c.description.is_empty(), "{kind} has empty description");
            assert_eq!(
                c.example.get("kind").and_then(Value::as_str),
                Some(kind),
                "{kind} example has the wrong kind"
            );
            for f in &c.config_fields {
                if f.value_type == "enum" {
                    assert!(
                        f.enum_values.is_some(),
                        "{kind}.{} is an enum but lists no values",
                        f.name
                    );
                }
            }
        }
        assert_eq!(all_contracts().len(), 12);
    }

    #[test]
    fn node_kinds_match_the_model_enum() {
        // Every catalog entry must deserialize back to a real NodeKind, and the
        // count must match — a new NodeKind without a contract fails here.
        for kind in NODE_KINDS {
            let parsed: NodeKind = serde_json::from_value(Value::String(kind.to_string()))
                .unwrap_or_else(|_| {
                    panic!("catalog kind {kind} is not a real NodeKind discriminator")
                });
            // round-trips back to the same string
            assert_eq!(
                serde_json::to_value(parsed).unwrap(),
                Value::String(kind.to_string())
            );
        }
    }

    #[test]
    fn unknown_kind_has_no_contract() {
        assert!(contract_for("not_a_kind").is_none());
        assert!(contract_for("").is_none());
    }

    #[test]
    fn with_note_appends_a_host_caveat() {
        let c = contract_for("tool_call").unwrap().with_note("host says hi");
        assert_eq!(c.notes.last().map(String::as_str), Some("host says hi"));
    }

    #[test]
    fn contracts_are_serde_round_trippable() {
        for c in all_contracts() {
            let json = serde_json::to_value(&c).unwrap();
            let back: NodeKindContract = serde_json::from_value(json).unwrap();
            assert_eq!(c, back);
        }
    }
}

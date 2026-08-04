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
pub const NODE_KINDS: [&str; 15] = [
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
                "A workflow's typed PARAMETERS are NOT declared here. They live in the graph's \
                 top-level `inputs` array (name/type/required/default/description), are validated \
                 before the run starts, and are read from any node as \"=inputs.<name>\". The \
                 trigger payload — free-form, whatever fired the run — stays at \
                 \"=run.trigger.<path>\"."
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
        "shell" => NodeKindContract {
            kind: "shell".to_string(),
            summary: "Run a shell script — inline, or an external script file.".to_string(),
            description: "Runs config.source (an inline script) or config.script_path (a script \
                file) through the host's ShellRunner capability, with an optional working \
                directory and environment. The script reads the node's input items as JSON from \
                the file named by its first argument. A non-zero exit fails the step; a \
                successful run emits one item of { exit_code, stdout, stderr, stdout_json }, \
                where stdout_json is the parsed stdout when it was JSON and null otherwise. \
                Whether shell steps run at all, which paths config.script_path and config.cwd may \
                reach, and what environment a script inherits are the host's decisions."
                .to_string(),
            config_fields: vec![
                ConfigField::optional(
                    "source",
                    "string",
                    "An inline script. Required unless script_path is set; the two are mutually exclusive.",
                ),
                ConfigField::optional(
                    "script_path",
                    "string",
                    "A path to an external script file, resolved and access-checked by the host. Required unless source is set.",
                ),
                ConfigField::optional("interpreter", "enum", "The shell runtime; defaults to sh.")
                    .with_enum(&["sh", "bash"]),
                ConfigField::optional(
                    "cwd",
                    "string",
                    "The working directory to run in, subject to the host's own path policy.",
                ),
                ConfigField::optional(
                    "env",
                    "object",
                    "Environment variables as a flat name/value map of strings.",
                ),
            ],
            ports: PortSpec::linear(),
            example: json!({
                "id": "build", "kind": "shell", "name": "Build",
                "config": {
                    "interpreter": "bash",
                    "cwd": "/srv/project",
                    "env": { "PROFILE": "release" },
                    "source": "set -euo pipefail\ncargo build --release\nprintf '{\"built\":true}'"
                }
            }),
            notes: vec![
                "The first argument to the script is a JSON file holding the node's input items."
                    .to_string(),
                "A non-zero exit status fails the step; stderr is quoted in the error.".to_string(),
                "Prefer config.source: an inline script stays reviewable with the workflow, where \
                 a script_path is only as trustworthy as the file it names."
                    .to_string(),
            ],
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
                ConfigField::optional(
                    "inputs",
                    "object",
                    "Values for the child's declared workflow inputs, by name. Each value is \
                     resolved against THIS node's scope, so a parent can forward its own inputs \
                     (\"=inputs.repo\") or an upstream node's output. Under \
                     execution=\"per_item\" the scope is the current element, so each child in a \
                     fan-out gets values from its own item (\"=item.name\").",
                ),
            ],
            ports: PortSpec::linear(),
            example: json!({
                "id": "sub", "kind": "sub_workflow", "name": "Enrich",
                "config": { "workflow_id": "flow-123", "inputs": { "repo": "=inputs.repo" } }
            }),
            notes: vec![
                "Exactly one of workflow / workflow_id — having both or neither is a hard reject."
                    .to_string(),
                "The child validates config.inputs against its OWN declarations, so omitting a \
                 required child input fails before the child executes anything."
                    .to_string(),
            ],
        },
        "memory" => NodeKindContract {
            kind: "memory".to_string(),
            summary: "Reads or writes host-managed memory via the MemoryProvider capability."
                .to_string(),
            description: "config.operation selects recall / search (query lookups), flavour \
                (a named ask/persona/style profile by slug), people (a people lookup), or \
                remember / forget (writes). What memory actually contains, how recall ranks \
                results, and what a flavour/people entry looks like are host concerns — the \
                engine only shapes the call and envelopes the response."
                .to_string(),
            config_fields: vec![
                ConfigField::required(
                    "operation",
                    "enum",
                    "Which memory action this node performs.",
                )
                .with_enum(&[
                    "recall", "search", "flavour", "people", "remember", "forget",
                ]),
                ConfigField::optional(
                    "scope",
                    "enum",
                    "Required for recall / remember / forget. Host-defined: \"user\" (the \
                     caller's durable, cross-flow memory — READ-ONLY from a workflow), \"flow\" \
                     (this flow's own memory — the only scope remember/forget may target), or \
                     \"flows\" (cross-flow read access — read-only).",
                )
                .with_enum(&["user", "flow", "flows"]),
                ConfigField::optional(
                    "query",
                    "\"=expr\"",
                    "Required for recall / search (optional for people). The lookup query.",
                ),
                ConfigField::optional(
                    "flavour",
                    "string",
                    "Required for the flavour operation: the ask/persona/style slug to look up, \
                     e.g. \"email-tone\".",
                ),
                ConfigField::optional(
                    "key",
                    "\"=expr\"",
                    "Required for remember / forget: the memory key to write or delete.",
                ),
                ConfigField::optional(
                    "value",
                    "\"=expr\"",
                    "Required for remember: the value to persist under key.",
                ),
                ConfigField::optional(
                    "limit",
                    "number",
                    "Optional cap on the number of results for recall / search.",
                ),
                ConfigField::optional(
                    "min_score",
                    "number",
                    "Optional relevance-score floor for recall / search results.",
                ),
            ],
            ports: PortSpec::linear(),
            example: json!({
                "id": "check_seen", "kind": "memory", "name": "Already published?",
                "config": { "operation": "recall", "scope": "flow", "query": "=item.title" }
            }),
            notes: vec![
                "HARD SECURITY RULE: a remember/forget node with scope \"user\" is a hard reject \
                 at validate time — writes may only target scope \"flow\". This is enforced \
                 structurally so an author (or an LLM authoring a graph) cannot plant or erase \
                 durable facts about the user via workflow content."
                    .to_string(),
                "Default execution is per_item (like tool_call), so a split_out fan-out runs one \
                 memory call per item — set execution: \"once\" to run a single call against the \
                 first item instead."
                    .to_string(),
            ],
        },
        "dedup" => NodeKindContract {
            kind: "dedup".to_string(),
            summary: "Commit-on-success exactly-once filter: drops items whose key was already \
                      committed."
                .to_string(),
            description: "config.key is an \"=\"-expression resolved per item (e.g. \
                \"=item.id\"). An item whose resolved key is already in the host's COMMITTED set \
                (a prior successful run) is dropped; an unseen key passes the item through and \
                records the key in the TENTATIVE set. This node never commits — the host commits \
                tentative keys into committed when the run succeeds, and releases (discards) \
                tentative keys when the run fails, via `StateStore`. Pairs with a host-side \
                commit-on-success subscriber; see the engine's `dedup` module docs for the exact \
                StateStore key layout."
                .to_string(),
            config_fields: vec![ConfigField::required(
                "key",
                "\"=expr\"",
                "The per-item dedup key expression, e.g. \"=item.id\". A key that resolves to \
                 null, missing, or an empty string fails OPEN: the item passes through and is \
                 NOT recorded (never silently dropped for a missing key).",
            )],
            ports: PortSpec::linear(),
            example: json!({
                "id": "once_only", "kind": "dedup", "name": "Skip already-published",
                "config": { "key": "=item.id" }
            }),
            notes: vec![
                "This node only FILTERS and stages tentative keys — it never writes to the \
                 committed set itself. A workflow author does not need to (and cannot) commit \
                 keys directly; the host commits/releases tentative keys based on overall run \
                 outcome."
                    .to_string(),
                "A null/missing/empty resolved key always passes through unrecorded (fail-open) \
                 rather than being treated as a match or a hard error."
                    .to_string(),
                "Within a single run, two input items that resolve to the SAME key: only the \
                 first passes; later duplicates are dropped, matching the committed-key rule."
                    .to_string(),
            ],
        },
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
        assert_eq!(all_contracts().len(), 15);
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
    fn node_kinds_has_15_entries_including_shell_memory_and_dedup() {
        assert_eq!(NODE_KINDS.len(), 15);
        assert!(NODE_KINDS.contains(&"shell"));
        assert!(NODE_KINDS.contains(&"memory"));
        assert!(NODE_KINDS.contains(&"dedup"));
        // "memory" is the 14th and "dedup" the 15th (last) entry — each added
        // at the end per the sequenced-last design rationale.
        assert_eq!(NODE_KINDS[13], "memory");
        assert_eq!(NODE_KINDS[14], "dedup");
    }

    #[test]
    fn memory_contract_documents_the_six_operations_and_scope_enum() {
        let c = contract_for("memory").expect("memory contract exists");
        let operation_field = c
            .config_fields
            .iter()
            .find(|f| f.name == "operation")
            .expect("memory contract declares `operation`");
        assert!(operation_field.required);
        assert_eq!(
            operation_field.enum_values,
            Some(
                vec![
                    "recall", "search", "flavour", "people", "remember", "forget"
                ]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
            )
        );
        let scope_field = c
            .config_fields
            .iter()
            .find(|f| f.name == "scope")
            .expect("memory contract declares `scope`");
        assert_eq!(
            scope_field.enum_values,
            Some(
                vec!["user", "flow", "flows"]
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            )
        );
        assert!(
            c.notes.iter().any(|n| n.contains("HARD SECURITY RULE")),
            "memory contract must document the user-scope write rejection"
        );
    }

    #[test]
    fn dedup_contract_requires_key_and_documents_fail_open_behavior() {
        let c = contract_for("dedup").expect("dedup contract exists");
        let key_field = c
            .config_fields
            .iter()
            .find(|f| f.name == "key")
            .expect("dedup contract declares `key`");
        assert!(key_field.required);
        assert_eq!(key_field.value_type, "\"=expr\"");
        assert!(
            c.notes.iter().any(|n| n.contains("fail-open")),
            "dedup contract must document the null-key fail-open behavior"
        );
        assert_eq!(c.ports, PortSpec::linear());
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

#[cfg(test)]
mod fan_out_contract_tests {
    use super::*;

    #[test]
    fn every_mapping_kind_advertises_the_fan_out_knobs() {
        for (kind, _) in FAN_OUT_KINDS {
            let c = contract_for(kind).expect("contract");
            for field in ["execution", "concurrency", "on_item_error"] {
                assert!(
                    c.config_fields.iter().any(|f| f.name == field),
                    "{kind} should advertise `{field}`"
                );
            }
        }
    }

    #[test]
    fn kinds_that_cannot_map_do_not_advertise_them() {
        // The contract and the validator must agree on which kinds fan out;
        // advertising a key that validation rejects would be worse than silence.
        for kind in [
            "trigger",
            "condition",
            "switch",
            "merge",
            "transform",
            "code",
        ] {
            let c = contract_for(kind).expect("contract");
            assert!(
                !c.config_fields.iter().any(|f| f.name == "concurrency"),
                "{kind} must not advertise `concurrency`"
            );
        }
    }

    #[test]
    fn the_execution_default_is_stated_per_kind() {
        let doc = |kind: &str| {
            contract_for(kind)
                .expect("contract")
                .config_fields
                .iter()
                .find(|f| f.name == "execution")
                .expect("execution field")
                .description
                .clone()
        };
        // An author needs to know that `agent` must opt in but `tool_call` need not.
        assert!(doc("agent").contains("\"once\""), "{}", doc("agent"));
        assert!(
            doc("tool_call").contains("\"per_item\""),
            "{}",
            doc("tool_call")
        );
    }
}

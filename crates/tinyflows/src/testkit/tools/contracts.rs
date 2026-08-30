use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One tool a host can register with its agent runtime.
///
/// Modelled on [`NodeKindContract`](crate::catalog::NodeKindContract): a
/// machine-readable description a host can enumerate, not a registration.
/// tinyflows talks to no model — it says what the tools are and what they do,
/// and the host decides what to expose and to whom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolContract {
    /// The tool name, namespaced. A host may prefix it.
    pub name: String,
    /// One line, for a tool listing.
    pub summary: String,
    /// What it does, when to reach for it, and what it will not do.
    pub description: String,
    /// A real JSON Schema for the arguments.
    ///
    /// Unlike [`catalog::ConfigField::value_type`](crate::catalog::ConfigField),
    /// which is a prose hint, this is meant to be handed to a model as-is.
    pub input_schema: Value,
    /// Whether calling it changes state a later call can observe.
    ///
    /// Every debug tool that releases a paused run is mutating; reads are not.
    /// A host that wants to gate side effects behind confirmation keys on this.
    pub mutating: bool,
}

/// The `session_id` argument every debug tool takes.
fn session_arg() -> Value {
    json!({
        "type": "string",
        "description": "The debug session, from flow_debug.start."
    })
}

/// A JSON Schema object with the given properties, all additional ones refused.
fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// The graph argument shared by the run/start tools.
fn graph_arg() -> Value {
    json!({
        "type": "object",
        "description": "The workflow graph to run, in tinyflows JSON form.",
    })
}

/// Every tool this module offers.
///
/// Stable order, so a host listing them twice gets the same listing.
#[must_use]
pub fn all_tools() -> Vec<ToolContract> {
    vec![
        ToolContract {
            name: "flow_test.run".to_string(),
            summary: "Run a workflow against mocks and report what it did.".to_string(),
            description: "Compiles and runs a graph with every capability mocked, so nothing \
                 reaches the outside world. Returns the run status, a summary of the trace, and \
                 the diagnosis — including bindings that resolved to null, which is the failure a \
                 green run hides: the node ran, the field was empty, and the workflow did nothing. \
                 Start here when a workflow 'works' but has no effect."
                .to_string(),
            input_schema: schema(
                json!({
                    "graph": graph_arg(),
                    "trigger": { "description": "The trigger payload." },
                    "inputs": {
                        "type": "object",
                        "description": "Values for the workflow's declared inputs, by name.",
                    },
                    "mocks": {
                        "type": "array",
                        "description": "Capability responses to program, in priority order.",
                        "items": mock_rule_schema(),
                    },
                    "approvals": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Gate node ids to pre-approve so the run does not park.",
                    },
                }),
                &["graph"],
            ),
            mutating: false,
        },
        ToolContract {
            name: "flow_test.trace".to_string(),
            summary: "Fetch the full trace of a run, or a slice of it.".to_string(),
            description: "Every node activation with the input it received, the output it \
                 produced, every `=`-binding and the value it resolved to, and every capability \
                 call. Filter by node to keep the reply small. Requires a run_id from \
                 flow_test.run."
                .to_string(),
            input_schema: schema(
                json!({
                    "run_id": { "type": "string", "description": "From flow_test.run." },
                    "node_id": {
                        "type": "string",
                        "description": "Only this node's activations.",
                    },
                }),
                &["run_id"],
            ),
            mutating: false,
        },
        ToolContract {
            name: "flow_test.node".to_string(),
            summary: "Inspect one node's activations in detail.".to_string(),
            description: "The input it was handed, what it emitted, every binding with its \
                 resolved value and the upstream node it reads from, and the capability calls it \
                 made. This is what to call once flow_test.run points at a node."
                .to_string(),
            input_schema: schema(
                json!({
                    "run_id": { "type": "string" },
                    "node_id": { "type": "string" },
                }),
                &["run_id", "node_id"],
            ),
            mutating: false,
        },
        ToolContract {
            name: "flow_debug.start".to_string(),
            summary: "Start a debuggable run and return a session id.".to_string(),
            description: "Starts the run paused-capable but not paused: set breakpoints with \
                 flow_debug.breakpoint, then flow_debug.wait for the first one. The run is held \
                 in this process, so a session does not survive a restart. Sessions are reaped \
                 after an idle period; call flow_debug.stop when finished."
                .to_string(),
            input_schema: schema(
                json!({
                    "graph": graph_arg(),
                    "trigger": { "description": "The trigger payload." },
                    "inputs": { "type": "object" },
                    "mocks": { "type": "array", "items": mock_rule_schema() },
                }),
                &["graph"],
            ),
            mutating: true,
        },
        ToolContract {
            name: "flow_debug.breakpoint".to_string(),
            summary: "Set, list, or clear breakpoints on a session.".to_string(),
            description: "A breakpoint fires before a node runs, after it settles, or both. \
                 Conditions narrow it: on_error catches whichever node fails, activation picks \
                 one pass of a loop, and expr takes an `=`-expression over the node's scope. \
                 Breaking *after* a node is the only way to see what it produced, and is not \
                 available in durable mode because resuming re-runs the node."
                .to_string(),
            input_schema: schema(
                json!({
                    "session_id": session_arg(),
                    "action": {
                        "type": "string",
                        "enum": ["set", "list", "clear"],
                        "description": "Defaults to set.",
                    },
                    "node_id": {
                        "type": "string",
                        "description": "The node to break on. Omit with any:true for every node.",
                    },
                    "any": {
                        "type": "boolean",
                        "description": "Break on every node rather than one.",
                    },
                    "before": { "type": "boolean", "description": "Break before it runs." },
                    "after": { "type": "boolean", "description": "Break after it settles." },
                    "on_error": {
                        "type": "boolean",
                        "description": "Only when the activation failed.",
                    },
                    "activation": {
                        "type": "integer",
                        "description": "Only the nth activation, counting from 1.",
                    },
                    "expr": {
                        "type": "string",
                        "description": "Only when this `=`-expression is truthy.",
                    },
                    "once": { "type": "boolean", "description": "Fire once, then disable." },
                    "breakpoint_id": {
                        "type": "integer",
                        "description": "Which breakpoint to clear.",
                    },
                }),
                &["session_id"],
            ),
            mutating: true,
        },
        ToolContract {
            name: "flow_debug.wait".to_string(),
            summary: "Wait for the run to park at a breakpoint.".to_string(),
            description: "Returns the paused activation: the node, its resolved input, the run \
                 state, its config with every binding resolved, and which bindings resolved to \
                 null. Returns paused:false if the run finished or nothing broke within the \
                 timeout — which is a normal outcome, not an error."
                .to_string(),
            input_schema: schema(
                json!({
                    "session_id": session_arg(),
                    "timeout_ms": {
                        "type": "integer",
                        "description": "How long to wait. Defaults to 5000.",
                    },
                }),
                &["session_id"],
            ),
            mutating: false,
        },
        ToolContract {
            name: "flow_debug.status".to_string(),
            summary: "Where a session is: running, paused, or finished.".to_string(),
            description: "Lists any parked activations and the registered breakpoints, without \
                 waiting for anything."
                .to_string(),
            input_schema: schema(json!({ "session_id": session_arg() }), &["session_id"]),
            mutating: false,
        },
        ToolContract {
            name: "flow_debug.release".to_string(),
            summary: "Release a paused activation and say what to do with it.".to_string(),
            description: "continue runs on; step stops at the very next node; override emits \
                 items you supply instead of running the node (or instead of what it produced, \
                 at the after phase); skip emits nothing; fail makes the node fail into its own \
                 on_error policy; patch merges into the run state before the node runs; detach \
                 clears every breakpoint and lets the run finish."
                .to_string(),
            input_schema: schema(
                json!({
                    "session_id": session_arg(),
                    "pause_id": { "type": "integer", "description": "From flow_debug.wait." },
                    "command": {
                        "type": "string",
                        "enum": [
                            "continue", "step", "override", "skip", "fail", "patch", "detach",
                        ],
                    },
                    "items": {
                        "type": "array",
                        "description": "For override: the items to emit. Plain JSON values are \
                                        wrapped as items automatically.",
                    },
                    "port": { "type": "string", "description": "For override: the output port." },
                    "message": { "type": "string", "description": "For fail: the error message." },
                    "patch": { "type": "object", "description": "For patch: the state to merge." },
                }),
                &["session_id", "pause_id", "command"],
            ),
            mutating: true,
        },
        ToolContract {
            name: "flow_debug.trace".to_string(),
            summary: "The trace of a debug session so far.".to_string(),
            description: "The same shape flow_test.trace returns, for a session that is still \
                 running. Safe to call while parked."
                .to_string(),
            input_schema: schema(
                json!({
                    "session_id": session_arg(),
                    "node_id": { "type": "string" },
                }),
                &["session_id"],
            ),
            mutating: false,
        },
        ToolContract {
            name: "flow_debug.stop".to_string(),
            summary: "End a session and return its outcome.".to_string(),
            description: "Releases anything parked, winds the run down, and frees the session. \
                 A session left running is reaped on idle, but stopping it is cheaper and \
                 returns the run's final state."
                .to_string(),
            input_schema: schema(json!({ "session_id": session_arg() }), &["session_id"]),
            mutating: true,
        },
    ]
}

/// The schema of one programmed capability response.
fn mock_rule_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "capability": {
                "type": "string",
                "enum": ["tools", "http", "llm", "agent", "code", "shell"],
            },
            "target": {
                "type": "string",
                "description": "Tool slug, URL, or agent ref. `*` globs. Defaults to `*`.",
            },
            "node_id": {
                "type": "string",
                "description": "Only apply to calls from this node.",
            },
            "value": { "description": "Respond with this value." },
            "error": { "type": "string", "description": "Fail with this message." },
            "sequence": {
                "type": "array",
                "description": "Answer successive calls with successive entries. Each entry is \
                                {value} or {error}. Past the end the last entry repeats.",
                "items": { "type": "object" },
            },
            "schema": {
                "type": "object",
                "description": "Synthesize a value satisfying this JSON Schema.",
            },
            "delay_ms": { "type": "integer", "description": "Wait before answering." },
        },
        "required": ["capability"],
        "additionalProperties": false,
    })
}

/// One tool by name.
#[must_use]
pub fn tool_for(name: &str) -> Option<ToolContract> {
    all_tools().into_iter().find(|tool| tool.name == name)
}

#[cfg(test)]
#[path = "contracts_tests.rs"]
mod tests;

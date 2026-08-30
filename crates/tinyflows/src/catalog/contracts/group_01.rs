use super::*;

pub(super) fn contract_trigger() -> NodeKindContract {
    NodeKindContract {
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
            ConfigField::optional(
                "recursion_limit",
                "number",
                "Total super-steps the run may execute before it fails. The graph-wide \
                     backstop for cycles; a `loop` node's own max_iterations is the per-loop \
                     bound and should usually be preferred because it names the offending loop.",
            ),
            ConfigField::optional(
                "max_node_visits",
                "number",
                "How many times any single node may be activated in one run. Bounds a cycle \
                     that has no `loop` node and, unlike recursion_limit, names the node that ran \
                     away.",
            ),
            ConfigField::optional(
                "max_concurrency",
                "number",
                "How many branches of one super-step may run at once, across the whole graph                      (default: unbounded, clamped to 256). This is ADMISSION CONTROL, not                      backpressure: a super-step engine cannot block a producer mid-step, so the                      only lever is how many activations are allowed to start.",
            ),
            ConfigField::optional(
                "max_item_concurrency",
                "number",
                "A run-level ceiling on every node's per-item `concurrency`, declared once                      here instead of edited into each node. It only ever LOWERS a node's own                      value. Note the dials MULTIPLY: peak in-flight work is roughly                      min(max_concurrency, active branches) x per-node concurrency.",
            ),
            ConfigField::optional(
                "node_timeout_secs",
                "number",
                "Bounds each individual node ATTEMPT (not the whole retry loop), so a node \
                     with retries gets this budget per attempt.",
            ),
            ConfigField::optional(
                "max_sub_workflow_depth",
                "number",
                "How deep `sub_workflow` nodes may nest before the chain is cut (default 8). \
                     Declared on the ROOT graph's trigger; it is forwarded to every child run, so \
                     setting it on a nested workflow has no effect.",
            ),
            ConfigField::optional(
                "workspace",
                "string",
                "The directory this run is pinned to. Every node `cwd` resolves against it \
                     and none may escape it; a `sub_workflow` node may re-pin a child run to a \
                     directory INSIDE it. A host that pins a workspace per run rather than per \
                     graph puts the same key on the trigger PAYLOAD instead. With no workspace \
                     the engine resolves nothing and a `cwd` reaches the harness verbatim.",
            ),
        ],
        ports: PortSpec::new(&[], &["main"]),
        example: json!({
            "id": "t", "kind": "trigger", "name": "Every morning",
            "config": { "trigger_kind": "schedule", "schedule": { "kind": "cron", "expr": "0 9 * * *" } }
        }),
        notes: vec![
            "Exactly ONE trigger node per graph — zero or multiple is a hard reject.".to_string(),
            "A workflow's typed PARAMETERS are NOT declared here. They live in the graph's \
                 top-level `inputs` array (name/type/required/default/description), are validated \
                 before the run starts, and are read from any node as \"=inputs.<name>\". The \
                 trigger payload — free-form, whatever fired the run — stays at \
                 \"=run.trigger.<path>\"."
                .to_string(),
        ],
    }
}

pub(super) fn contract_agent() -> NodeKindContract {
    NodeKindContract {
        kind: "agent".to_string(),
        summary: "An LLM step, run via the host's LlmProvider capability.".to_string(),
        description: "Runs config.prompt through the injected LlmProvider, or — when \
                config.agent_ref names an agent type and the host wired an AgentRunner — through \
                that agent's own loop. An agent type is defined either in the graph's top-level \
                `agents` registry or by the host; the node may then NARROW it (fewer tools, lower \
                limits, extra instructions) but never widen it. config.output_parser.schema \
                requests a structured, field-addressable output item. tinyflows runs no agent \
                loop itself: it assembles the request and the harness executes it."
            .to_string(),
        config_fields: vec![
            ConfigField::required("prompt", "string", "The instruction sent to the model."),
            ConfigField::optional(
                "agent_ref",
                "string",
                "The agent type to run this step as. Resolved against the graph's `agents` \
                     registry first, then the host's. Must be a LITERAL — an =expression is \
                     rejected, since run data must not choose a differently-privileged agent.",
            ),
            ConfigField::optional(
                "instructions",
                "string",
                "Extra standing instructions for this step. APPENDED to the agent \
                     definition's, never replacing them.",
            ),
            ConfigField::optional(
                "model",
                "string",
                "Model id, opaque to the engine. Overrides the agent definition's.",
            ),
            ConfigField::optional(
                "provider",
                "string",
                "Provider id, opaque to the engine (e.g. a gateway or routing key). Carried \
                     separately from `model` so a host routing one model through several \
                     providers need not parse a composite string.",
            ),
            ConfigField::optional(
                "cwd",
                "string",
                "Working directory the agent runs in, when it runs somewhere with a \
                     filesystem. Resolved against the run's workspace (trigger config.workspace): \
                     a relative path is joined to it, an absolute one must resolve inside it, and \
                     a directory that is missing or is not a directory FAILS the step rather than \
                     falling back to the workspace. Bindable, which is the point — \
                     \"=nodes.prepare.item.json.worktree\" points the step at a directory an \
                     earlier node created. On a run with no workspace the string is passed to the \
                     harness verbatim and the host must validate it itself.",
            ),
            ConfigField::optional(
                "working_dir",
                "string",
                "The older spelling of `cwd`, and the name of the field on an agent \
                     definition, so it stays accepted; `cwd` wins when both are set. Same \
                     resolution rules.",
            ),
            ConfigField::optional(
                "context",
                "array",
                "Dynamic context blocks, each {kind, label?, optional?}: kind=text {text} \
                     (literal or =expression); items (the node's input items); memory \
                     {scope,query,limit?} and flavour {slug} (via the host's MemoryProvider); \
                     host {source,params?} (expanded by the harness). A block that cannot be \
                     resolved FAILS the node unless it sets optional:true.",
            ),
            ConfigField::optional(
                "tools",
                "array",
                "Tool grants, each {slug, connection_ref?}. slug is an exact id or a \
                     trailing-.* namespace pattern; both it and connection_ref must be literals. \
                     When the agent definition grants tools, this list may only NARROW them.",
            ),
            ConfigField::optional(
                "limits",
                "object",
                "Advisory ceilings for the harness's loop: {max_steps?, max_tool_calls?, \
                     agent_timeout_secs?, tool_timeout_secs?}. agent_timeout_secs bounds the whole \
                     run; tool_timeout_secs bounds each individual tool call, so one wedged tool \
                     cannot eat the whole budget. May only LOWER the agent definition's. Advisory \
                     because the loop runs host-side — node_timeout_secs is what the engine \
                     itself enforces.",
            ),
            ConfigField::optional(
                "metadata",
                "object",
                "Free-form passthrough to the harness (tier hints, sandbox policy, …), \
                     merged key-wise over the agent definition's. Never interpreted by the engine.",
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
            "Reusable agent types live in the graph's TOP-LEVEL `agents` array (a sibling of \
                 `nodes`/`edges`/`inputs`), each {id, name?, description?, instructions?, model?, \
                 provider?, working_dir?, tools?, context?, limits?, metadata?}. A node's \
                 agent_ref resolves there first, then against the host's registry; a ref neither \
                 knows is passed through to the harness as an id, which is not an error."
                .to_string(),
            "Merge order is definition-then-node, narrowing only: instructions append, \
                 context appends, tools intersect, limits take the lower bound, metadata merges \
                 per key, and model/provider/cwd (working_dir) are overridden by the node."
                .to_string(),
            "agent_ref, tool slug, tool connection_ref, a memory context scope, and a host \
                 context source must all be LITERALS, never =expressions — otherwise run data \
                 (which may include model output) could choose the credential a call acts as, the \
                 tool it reaches, or the agent type it runs as."
                .to_string(),
            "The output item carries a `meta` key alongside json/text/raw: `=item.meta.stop` \
                 is \"finished\" or \"limit_stop\", so a downstream condition can branch on \
                 whether the agent actually reached an answer instead of assuming it did. A \
                 limit_stop output is PARTIAL and skips output_parser."
                .to_string(),
            "A `memory` or `flavour` context block needs the host's MemoryProvider, and a \
                 `host` block needs the harness's context resolver; without them the node fails \
                 unless the block sets optional:true. Silently dropping context would leave the \
                 agent answering confidently from nothing."
                .to_string(),
        ],
    }
}

pub(super) fn contract_tool_call() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_http_request() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_code() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_shell() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_condition() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_switch() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_merge() -> NodeKindContract {
    NodeKindContract {
        kind: "merge".to_string(),
        summary: "A fan-in barrier that passes its inputs through.".to_string(),
        description: "Waits for its inbound branches and passes the collected items through. \
                No config."
            .to_string(),
        config_fields: vec![],
        ports: PortSpec::linear(),
        example: json!({ "id": "join", "kind": "merge", "name": "Join" }),
        notes: vec![],
    }
}

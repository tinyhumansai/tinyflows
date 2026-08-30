use super::*;

pub(super) fn contract_split_out() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_loop() -> NodeKindContract {
    NodeKindContract {
        kind: "loop".to_string(),
        summary: "Repeat a section of the workflow a bounded number of times.".to_string(),
        description: "Repeats a section, optionally CARRYING STATE across the passes. Emits \
                its input on `body` until an exit fires, then on `done` (or `success`). Close the \
                loop by wiring the last node of the body back to this node; that back-edge is \
                what makes the section repeat.\n\n\
                With config.state the loop becomes a fold: `init` seeds an accumulator and \
                `update` folds each pass's output into it, so a refinement loop can remember what \
                it already tried. The accumulator and the pass number are readable anywhere in \
                the graph as \"=nodes.<loop id>.state\" and \"=nodes.<loop id>.iteration\"; \
                inside this node's own `update`/`until` the accumulator is just \"state\"."
            .to_string(),
        config_fields: vec![
            ConfigField::optional(
                "max_iterations",
                "number",
                "How many times the body may run before the loop stops (default 25). Always \
                     finite — a loop with no cap is the runaway case this node exists to prevent.",
            ),
            ConfigField::optional(
                "on_exceeded",
                "enum",
                "What happens at the cap: \"error\" (default) fails the run, naming this \
                     node; \"continue\" stops looping and emits on `done` so downstream still \
                     runs with the last pass's items.",
            )
            .with_enum(&["error", "continue"]),
            ConfigField::optional(
                "condition",
                "\"=expr\"",
                "Optional early exit. While it resolves truthy the loop continues; the first \
                     falsey result routes to `done` without consuming an iteration. Checked \
                     before the cap, so a loop that finishes on its own terms never errors.",
            ),
            ConfigField::optional(
                "state",
                "object",
                "{init, update} — the accumulator. `init` is a literal or \"=expr\" resolved \
                     once when the loop starts; `update` folds each pass into it and is either a \
                     jq program producing the whole next accumulator, or an object of per-key \
                     \"=expr\" merged over it (like transform.set). Inside `update` the previous \
                     accumulator is \"state\" and the body's output is \"item\"/\"items\".",
            ),
            ConfigField::optional(
                "until",
                "\"=expr\"",
                "Stop when this goes truthy — the OPPOSITE polarity to `condition` (which \
                     means keep going while). Evaluated against the accumulator AFTER the pass \
                     is folded in, so \"=.state.score > 0.9\" tests the pass that just ran. \
                     Checked before `condition` and before the cap, so converging beats both \
                     running out of work and running out of tries.",
            ),
            ConfigField::optional(
                "emit",
                "enum",
                "What the exit port carries: \"items\" (default, the last pass's items) | \
                     \"state\" (one item holding the accumulator) | \"both\".",
            )
            .with_enum(&["items", "state", "both"]),
            ConfigField::optional(
                "success_port",
                "boolean",
                "Route an `until` exit to a separate `success` port instead of `done`, so a \
                     loop that CONVERGED can be handled differently from one that ran out of \
                     tries. Requires an edge on `success`, or the graph is refused.",
            ),
        ],
        ports: PortSpec::new(&["main"], &["body", "done", "success"]),
        example: json!({
            "id": "retry_until_clean", "kind": "loop", "name": "Until tests pass",
            "config": {
                "max_iterations": 5,
                "on_exceeded": "continue",
                "condition": "=item.tests_failing"
            }
        }),
        notes: vec![
            "The body must route back to this node or it runs once and stops — the \
                 back-edge is the loop."
                .to_string(),
            "A fan-in `merge` inside the loop body deadlocks it ONLY when one of the inputs \
                 it waits for comes from OUTSIDE the cycle: that arm runs once, on the seeding \
                 pass, and never again, so from the second iteration the barrier can never \
                 complete. A merge whose arms are all on the cycle is fine — they all re-run \
                 every pass."
                .to_string(),
            "`exit_reason` is recorded alongside `iteration` and `state` (\"until\" | \
                 \"condition\" | \"max_iterations\"), which is how downstream tells a loop that \
                 CONVERGED from one that merely ran out of tries under on_exceeded:\"continue\"."
                .to_string(),
            "The fold is at-least-once: if an activation is replayed after a resume the \
                 update applies again. The iteration counter has always behaved this way; the \
                 accumulator just makes it visible (a duplicated append). Prefer an idempotent \
                 `update` — assign the next value rather than appending — where that matters."
                .to_string(),
        ],
    }
}

pub(super) fn contract_transform() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_output_parser() -> NodeKindContract {
    NodeKindContract {
        kind: "output_parser".to_string(),
        summary: "Passthrough today; no config required.".to_string(),
        description: "A passthrough node reserved for structured-output parsing. Requires no \
                config."
            .to_string(),
        config_fields: vec![],
        ports: PortSpec::linear(),
        example: json!({ "id": "parse", "kind": "output_parser", "name": "Parse" }),
        notes: vec![],
    }
}

pub(super) fn contract_sub_workflow() -> NodeKindContract {
    NodeKindContract {
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
                "workspace",
                "string",
                "Runs the child in another directory: it becomes the CHILD run's workspace, \
                     which the child's own nodes resolve their `cwd` against. Resolved against \
                     the parent's workspace under the same rule as an agent node's `cwd` (must \
                     resolve inside it, must exist, must be a directory) and bindable the same \
                     way. Omitted, the child inherits the parent's workspace.",
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
    }
}

pub(super) fn contract_memory() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_dedup() -> NodeKindContract {
    NodeKindContract {
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
    }
}

pub(super) fn contract_spawn() -> NodeKindContract {
    NodeKindContract {
        kind: "spawn".to_string(),
        summary: "Starts work WITHOUT waiting for it and emits a ticket; a downstream `gate` \
                      collects the result."
            .to_string(),
        description: "Every other node blocks its branch until it has an answer. This one \
                          starts the work and immediately emits a ticket, so the branch carries \
                          on while the work runs, and a downstream `gate` turns tickets back \
                          into results. Use it when a slow call has no downstream dependency \
                          until later in the graph."
            .to_string(),
        config_fields: vec![
            ConfigField::required("target", "enum", "What to start: workflow | tool | http.")
                .with_enum(&["workflow", "tool", "http"]),
            ConfigField::optional(
                "workflow",
                "WorkflowGraph",
                "Child graph, when target=workflow.",
            ),
            ConfigField::optional(
                "input",
                "any",
                "Trigger payload for the child, when target=workflow.",
            ),
            ConfigField::optional("slug", "string", "Tool identifier, when target=tool."),
            ConfigField::optional("args", "object", "Tool arguments, when target=tool."),
            ConfigField::optional(
                "request",
                "object",
                "Request description, when target=http.",
            ),
        ],
        ports: PortSpec::new(&["main"], &["main", "error"]),
        example: json!({
            "id": "kick_off", "kind": "spawn", "name": "Start the scan",
            "config": { "target": "tool", "slug": "scanner.run", "args": { "repo": "=item.repo" } }
        }),
        notes: vec![
            "Emits one item per started task shaped {ticket, spawn, started_at_step}. The \
                 ticket is opaque — pass it to a `gate`, do not interpret it."
                .to_string(),
            "Needs the host's TaskRunner capability to actually overlap. With NONE injected \
                 the work runs INLINE and the ticket comes back already settled: the answer is \
                 the same, the concurrency is not. That is a silent performance cliff, so check \
                 the host wires a TaskRunner before relying on overlap."
                .to_string(),
            "Fire-and-forget is legal — a spawn no gate ever collects simply runs. Wire the \
                 spawn into a `void` to say that on purpose; wire it into a `gate` if you \
                 actually wanted the results."
                .to_string(),
        ],
    }
}

pub(super) fn contract_gate() -> NodeKindContract {
    NodeKindContract {
        kind: "gate".to_string(),
        summary: "Waits for spawned work and emits results once its release policy is \
                      satisfied (all / any / first_n / quorum / timeout_partial)."
            .to_string(),
        description: "The collecting half of `spawn`. More than a barrier because of the \
                          release policy: a gate can proceed on the first result, on a quorum, \
                          or on whatever arrived before its deadline, rather than only on all of \
                          them. Waiting is counted in POLLS — each costs a super-step — unless \
                          `wait_mode: \"suspend\"` interrupts the run instead."
            .to_string(),
        config_fields: vec![
            ConfigField::optional(
                "from",
                "array",
                "Ids of upstream `spawn` nodes whose tickets to wait on. The usual spelling; \
                     mutually exclusive with `tickets`.",
            ),
            ConfigField::optional(
                "tickets",
                "\"=expr\"",
                "Expression yielding a ticket id or array of them, for a graph that carries \
                     tickets some other way.",
            ),
            ConfigField::optional(
                "release",
                "enum",
                "When to proceed: all (default) | any | first_n | quorum | timeout_partial.",
            )
            .with_enum(&["all", "any", "first_n", "quorum", "timeout_partial"]),
            ConfigField::optional(
                "n",
                "number",
                "Required (and must be > 0) for first_n and quorum.",
            ),
            ConfigField::optional(
                "poll_interval_ms",
                "number",
                "Gap between polls (default 250).",
            ),
            ConfigField::optional(
                "max_polls",
                "number",
                "Poll budget before the wait is called spent (default 200). EVERY poll costs \
                     a super-step and a node visit, so this interacts with recursion_limit and \
                     max_node_visits.",
            ),
            ConfigField::optional(
                "wait_mode",
                "enum",
                "poll (default) re-activates the node each interval; suspend interrupts the \
                     run so the host resumes it when the work lands — right for long waits.",
            )
            .with_enum(&["poll", "suspend"]),
            ConfigField::optional(
                "on_timeout",
                "enum",
                "error (default) | partial (emit what arrived) | route (use the `timeout` port).",
            )
            .with_enum(&["error", "partial", "route"]),
        ],
        ports: PortSpec::new(&["main"], &["main", "timeout", "error"]),
        example: json!({
            "id": "collect", "kind": "gate", "name": "Best two of three",
            "config": { "from": ["kick_off"], "release": "quorum", "n": 2, "on_timeout": "partial" }
        }),
        notes: vec![
            "Output is ordered by TICKET INDEX, not by which finished first, and each item \
                 keeps its `paired_item`. Two runs therefore emit the same order regardless of \
                 timing."
                .to_string(),
            "A partial release (any / first_n / quorum) leaves the stragglers running. Their \
                 results are simply not collected."
                .to_string(),
            "A failed task is emitted as an item shaped {failed: true, error} rather than \
                 failing the node, so it can be branched on with \"=item.failed\"."
                .to_string(),
        ],
    }
}

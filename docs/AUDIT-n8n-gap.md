# tinyflows vs n8n — Gap Analysis & Audit

Audit date: 2026-07-05. Crate at `vendor/tinyflows` (~0.5.x, `feat/named-node-scope`),
already integrated into OpenHuman host. Scope: what to port from n8n, missing
functionality, bugs, and agent-facing graph-editing tools.

---

## 0. TL;DR

tinyflows is a **small, correct-in-the-common-case, declarative** engine (12 node
kinds, ~10k LoC) that deliberately pushes all vendor logic into host capability
traits. It is **already fully wired into OpenHuman** (six-trait adapter, `flows::`
RPC domain + SQLite, agent propose/revise/dry-run tools, cron-driven triggers,
durable checkpointing). So this is not greenfield — it is a maturity gap vs n8n.

Three things dominate the action list:

1. **One security bug to fix now**: jq expressions can read the host process
   environment (`"=env"` → all env vars, incl. API keys) — see **BUG-1**.
2. **Correctness bugs around branching/merge/sub-workflow** that will bite as soon
   as workflows get non-trivial — **BUG-2..6**.
3. **The n8n gap is mostly breadth, not depth**: n8n has ~470 integration nodes +
   a rich per-item data model + triggers/webhooks/UI. tinyflows covers ~1% of nodes
   by count but its _capability-trait_ design means most of that breadth is the
   host's job (Composio), not the engine's. The real engine-level gaps are
   **per-item semantics, triggers, and the expression language**.

---

## 1. What tinyflows has (node catalog)

12 node kinds. Wire names are snake_case; dispatch in `src/nodes/mod.rs:208`.

| Kind            | Role                                   | Per-item?                          | Notes                                                                                                       |
| --------------- | -------------------------------------- | ---------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `trigger`       | sole entry, passthrough                | batch                              | also holds run knobs `recursion_limit`, `node_timeout_secs`; `TriggerKind` is **dead code at runtime**      |
| `agent`         | 1 LLM turn via `LlmProvider`           | batch→1                            | single tool hop (no multi-turn loop); `output_parser` inline; `chat_model`/`memory` sub-ports **not wired** |
| `tool_call`     | 1 integration action via `ToolInvoker` | batch→1                            | `slug` required                                                                                             |
| `http_request`  | outbound HTTP via `HttpClient`         | batch→1                            | no pagination/retry-on-status/response-format in-crate                                                      |
| `code`          | sandboxed JS/Python via `CodeRunner`   | batch→1                            | **skips `=`-expression resolution**; silent JS default                                                      |
| `condition`     | 2-way IF                               | **first item decides whole batch** | `field` = top-level key only; no operators                                                                  |
| `switch`        | N-way branch                           | **first item decides whole batch** | `=`-expr or `field`; **missing `nodes` scope**                                                              |
| `merge`         | fan-in barrier                         | batch                              | **no `mode`** (append/combine/SQL); barrier is engine-level                                                 |
| `split_out`     | fan-out array→items                    | per-item                           | `path` = top-level key only                                                                                 |
| `transform`     | field mapping                          | per-item                           | `set:{k:v-or-=expr}`; **missing `nodes` scope**; no delete/rename                                           |
| `output_parser` | JSON-schema validate + LLM auto-fix    | per-item                           | schema **subset** ($ref/oneOf/bounds unsupported); skips `=`-resolution                                     |
| `sub_workflow`  | run child graph                        | batch→1                            | depth ≤8; **HITL/observer/cancel dropped in child**; skips `=`-resolution; double-wraps input               |

Cross-cutting (engine-level, all kinds): `on_error` (stop/continue/route),
`retry` (fixed/exponential, cap 60s), `requires_approval` HITL gate, cooperative
cancellation, per-node `ExecutionStep` observability.

---

## 2. n8n gap analysis (what to port)

### 2a. Breadth gaps that are NOT the engine's job (host/Composio owns these)

n8n ships ~470 `nodes-base` integrations (Slack, Notion, Postgres, Google*, AWS*, …)
plus a full LangChain node family (agents, chains, memory, vector_store, embeddings,
retrievers, rerankers, text_splitters, document_loaders, output_parser, tools, MCP).
In tinyflows these collapse into `tool_call` (→ Composio) + `agent`. **Do not port
these as nodes.** The gap is catalog coverage in the host's `ToolInvoker`, not engine
work. Grounding already exists (`search_tool_catalog` builder tool).

### 2b. Engine-level gaps worth porting (ranked)

| Priority | n8n feature                                                                                                                                                                      | tinyflows today                                                                                                                                                       | Port recommendation                                                                                                                                                      |
| -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **P0**   | **Per-item execution** — every node maps over items, IF/Switch **partition** items per-branch, node params evaluate per item                                                     | condition/switch route the **whole batch by first item**; agent/tool_call/http resolve config **once** against first item                                             | Biggest semantic divergence. Add a per-item execution mode (at least to condition/switch, ideally integration nodes). Users porting n8n flows will hit this immediately. |
| **P0**   | **Triggers** — Webhook, Schedule, Manual, Form, Chat, polling triggers                                                                                                           | `TriggerKind` modeled but **inert**; host fires runs. Webhook/chat/form/execute-by-workflow validate but **never auto-fire** (`flows/ops.rs:123`, `flows/bus.rs:232`) | Wire webhook→flow in the host webhooks domain (add a `flow` webhook target kind). Schedule + app_event + manual already work via cron/composio bus.                      |
| **P1**   | **Merge modes** — append, combine-by-key, combine-by-position, multiplex                                                                                                         | `merge` = concat only                                                                                                                                                 | Add `mode` config to `merge` node. Combine-by-key is the common ask.                                                                                                     |
| **P1**   | **Expression language** — `{{ }}` interpolation, `$json/$node/$items/$prevNode/$runIndex/$itemIndex`, pairedItem lineage, luxon `$now/$today`, `$env/$vars/$workflow/$execution` | whole-value `=` jq only; `nodes.<id>` (id, not name; latest-run only); no interpolation, no run/item index, no paired-item traversal, no env/vars/workflow metadata   | Add string interpolation and a small set of built-ins (`$now`, `$workflow`, `$runIndex`). Named scope exists but is buggy (see BUG-2).                                   |
| **P1**   | **Wait / delay node** (`Wait`, resume-after, resume-on-webhook)                                                                                                                  | none (only HITL approval interrupt)                                                                                                                                   | A `wait` node (duration or until-webhook) is a common automation primitive; reuses the existing checkpoint/resume machinery.                                             |
| **P2**   | **Loop/batching** (`SplitInBatches`/Loop Over Items)                                                                                                                             | cycles work but only via edge loops + `recursion_limit`; no batch-size loop node                                                                                      | Add a batched-loop node for large fan-outs.                                                                                                                              |
| **P2**   | **Filter / Set / Sort / Limit / Aggregate / Remove-Duplicates** item ops                                                                                                         | only `transform` (set) + `split_out`                                                                                                                                  | Add `filter`, `sort`, `limit`, `aggregate` — cheap, pure, high-value item ops.                                                                                           |
| **P2**   | **Error Trigger / error workflow**, **Stop-and-Error**, **NoOp**, **Sticky Note**                                                                                                | `on_error` per node; no error-workflow, no explicit no-op/stop nodes                                                                                                  | `no_op` and `stop_and_error` are trivial; error-workflow is a host concern.                                                                                              |
| **P2**   | **Binary data pipeline** (files, MoveBinaryData, Read/Write, Compression)                                                                                                        | `Item.binary` field exists but is **never projected into scope** and no node manipulates it                                                                           | If OpenHuman needs file flows, wire binary through expressions + a couple of file nodes.                                                                                 |
| **P3**   | **Pinning / static test data**, node **disabled** flag, **notes/tags**                                                                                                           | none (position/ports are editor-only metadata)                                                                                                                        | Editor-quality features; add when the builder UI matures.                                                                                                                |
| **P3**   | **Sub-workflow input mapping & typed I/O**                                                                                                                                       | child input is double-wrapped serialized items; output is the whole child run-state                                                                                   | Clean up the sub-workflow I/O contract (see BUG-4).                                                                                                                      |

---

## 3. Bugs (verified where noted)

### Security

- **BUG-1 (verified, fix now) — jq expressions leak host env.** `run_jq` registers
  `jaq_std::funs()` (`src/expr.rs:302`); jaq-std's `env` builtin dumps
  `std::env::vars()` (jaq-std lib.rs:466). Any config value `"=env"` / `"=$ENV"`
  returns every process env var (API keys, tokens) into node output → run state,
  journal, downstream tool calls. **Fix:** drop jaq-std default features or filter
  the `env`/`input*` builtins before compile. `now` is also exposed (nondeterministic,
  benign).

### Correctness — branching / merge

- **BUG-2 (verified) — `switch` and `transform` never receive the `nodes` scope.**
  They hand-build `{item,items,run}` (`switch.rs:26`, `transform.rs:27`), so
  `=nodes.a.item.x` silently returns null exactly in the nodes where cross-node
  refs are most useful. Contradicts the module docs. Fix: use `expr_scope`.
- **BUG-3 — mixed-port fan-out silently drops branches.** `fan_out_targets`
  (`engine.rs:422`) only treats _all-same-port_ multi-edges as parallel fan-out.
  The common "fan out `main→a`,`main→b` + `error→h`" shape falls into conditional-edge
  lowering, whose route map (tinyagents builder) overwrites duplicate labels — one of
  `a`/`b` never runs. No validation rejects it.
- **BUG-4 — merge barrier + input collection ignore ports.** Waiting edges are added
  only for single-out-degree predecessors (`engine.rs:789`), so a merge fed by a
  branching/fan-out predecessor gets an incomplete barrier. Compounding it,
  `collect_input` (`engine.rs:171`) pulls items from **every** predecessor slot
  regardless of the port the run actually took — untaken-branch (e.g. condition
  `false`) data leaks into a fan-in wired to the `true` port.

### Correctness — sub-workflow / lifecycle

- **BUG-5 — HITL gate inside a `sub_workflow` is silently ignored.**
  `SubWorkflowNode::execute` keeps only `outcome.output`, discarding
  `pending_approvals`/`cancelled` (`sub_workflow.rs:123`). A child pausing at an
  approval gate returns partial state and the parent continues as if it completed —
  approval gating is unenforceable across the boundary. Child also gets no observer,
  no journal, in-memory checkpointer, and no parent cancellation token.
- **BUG-6 — `on_run_finish` never fires on failure; `RunStatus::Failed` is dead.**
  A `stop`-policy failure bubbles via `?` (`engine.rs:927`) before the `Run` record
  is built, so hosts get `on_run_start` with no `on_run_finish`, and the only status
  ever emitted is `Completed`. Observer-based run history leaks "running forever"
  rows for failed runs. (OpenHuman's `FlowRunObserver` persists steps — check it
  doesn't strand failed runs.)

### Correctness — cancellation / timeout / retry

- **BUG-7 — cancellation not checked inside the retry loop** (`engine.rs:671`): a
  node with large `max_attempts`+`backoff_ms` keeps retrying/sleeping for minutes
  after `cancel()`.
- **BUG-8 — `node_timeout_secs` covers the whole retry loop, not per attempt**
  (`engine.rs:448`): timeout and retry budgets interact silently (30s timeout kills a
  node mid-3×20s-backoff).

### Consistency

- **BUG-9 — expression binding is applied inconsistently.** `code`, `output_parser`,
  and `sub_workflow` read raw config and **skip `=`-resolution** — a `"=item.x"` in
  `code.source` or `sub_workflow.workflow_id` is treated as a literal. Every other
  integration node resolves. Pick one rule.
- **BUG-10 — validation is thin.** No cycle detection (relies on runtime
  `recursion_limit`), no port validation (edges may name ports nothing emits; no check
  that `on_error:"route"` has an `error` edge, or `condition` has both branches), no
  per-kind config validation (missing `tool_call.slug`, bad `on_error`/`retry` values
  → runtime or silent `stop`), no `type_version` compat check, no orphan/dup-edge
  detection. `migrate` also stamps **future** `schema_version`s down to 1 silently.
- **BUG-11 (diff) — quadratic state cloning.** Every node execution deep-clones the
  full `nodes` run-state slice (`engine.rs:630`) and re-projects per retry attempt —
  O(N²·M) copying; large LLM/HTTP payloads make every downstream node pay for the whole
  run history. Clone happens even when config has no `=`-expressions.
- **BUG-12 (diff) — diagnostics dropped on the error path.** The step is recorded with
  empty `diagnostics` on error (`engine.rs:730`), losing exactly the null-resolution
  info the feature targets (null arg → tool errors → stop). Capture misses from the
  last failed attempt.
- **BUG-13 — non-identifier node ids are second-class in expressions.**
  `=nodes.my-node.item.x` fails the dotted fast-path (hyphen) and jq parses `my-node`
  as subtraction → null. Only `=.nodes["my-node"]…` works; nothing validates ids are
  addressable.
- **BUG-14 — `EngineError::Capability` is a catch-all** (config/depth/cycle/serde/real
  capability failures share one variant) — hosts discriminate by string.
- Minor: `collect_input` silently drops malformed items (`engine.rs:181`); `switch`
  routes a non-string discriminant via `to_string()` (an object becomes its JSON dump
  as a port name); `resolve_traced` duplicates `resolve`'s traversal (drift risk).

---

## 4. Agent-facing graph-editing tools (OpenHuman)

**Today (host `src/openhuman/flows/`):** the agent can `propose_workflow`
(validate-only), `revise_workflow` (validate-only, whole-graph resubmit),
`dry_run_workflow` (mock caps, zero side effects), `run_workflow` (confirmed test-run
of a **saved** flow), and read tools (`list_flows`/`get_flow`/`get_flow_run`/
`list_flow_connections`/`search_tool_catalog`). A dedicated `workflow_builder`
sub-agent wraps these. **The agent cannot persist/enable/update/delete** a flow —
that is human-only via the UI `WorkflowProposalCard` → `flows_create`.

**Recommended additions (for the migrate/port pass):**

1. **Node-level patch tool.** Today revision = resubmit the whole graph. Add
   `patch_workflow(ops: [add_node|update_node|remove_node|add_edge|remove_edge])`
   with per-op validation. Far more token-efficient and less error-prone for the agent
   iterating on a large graph.
2. **Structured validation feedback.** `validate` should return typed, node-addressed
   diagnostics (port errors, missing required config, unreachable nodes) — depends on
   fixing **BUG-10**. Feed these back into the agent loop.
3. **Explain/lint tool.** A read tool that surfaces the null-resolution diagnostics and
   the branching/merge footguns (BUG-3/4) so the agent doesn't author them.
4. **n8n import as an agent tool.** `flows_import(format:"n8n")` exists as RPC
   (`n8n_import.rs`) but isn't in the agent tool belt — expose it so the agent can
   "port this n8n workflow" directly (with a coverage report of unmapped nodes).
5. **Guarded persist path (optional).** If autonomy tier allows, a
   `save_workflow(draft, enable:false)` that creates but never auto-enables, keeping
   the human in the loop only for enablement.
6. **Catalog-grounded node authoring.** Extend `search_tool_catalog` grounding to also
   validate `agent` model names and `connection_ref`s at author time.

---

## 4b. Node I/O alignment (the agent ↔ tool_call handoff problem)

**Root cause: there is no item envelope / output contract.** Every capability node
wraps whatever its host capability returned _verbatim_ into `Item.json`
(`agent.rs:115` `Item::new(value)`, `tool_call.rs:31` `Item::new(result)`,
`http_request.rs:24` `Item::new(response)`). The next node reads
`item = input.first().json` (`nodes/mod.rs:62`) — a provider-native blob whose shape
is undocumented in the graph. Nothing normalizes, nothing validates that the shape a
producer emits matches what a consumer's expressions read.

### Exact emitted shapes (OpenHuman host)

| Producer                | `item.json` shape it emits                                                                                              | Stable?                                    |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------- | ------------------------------------------ |
| `agent` (plain)         | if the model text parses as JSON → **that parsed object/array verbatim**; else `{ "text": "…", … }` (`caps.rs:359,369`) | **No — runtime-dependent on model output** |
| `agent` + tool sub-port | above **+ `tool_result: <tool output>`** merged in (`agent.rs:82`)                                                      | No                                         |
| `agent` + output_parser | **replaced entirely** by the schema-coerced value (`agent.rs:104`)                                                      | Shape = the schema, not the completion     |
| `tool_call`             | `serde_json::to_value(<Composio execution result>)` — provider envelope (`caps.rs:968`)                                 | Provider-native                            |
| `http_request`          | raw `HttpClient` response Value                                                                                         | Host-defined                               |
| `code`                  | the runner's return Value                                                                                               | User-defined                               |
| `split_out`             | one item per array element of `path`                                                                                    | —                                          |

So an `agent` node emits **three structurally different shapes** depending only on
which sub-ports are configured, and the plain shape flips between "parsed model JSON"
and `{text}` at _runtime_ based on what the model said. Any downstream
`=item.<field>` is therefore guessing.

### The five concrete misalignments

- **M1 — no normalized field.** To read an agent's text a downstream node needs
  `=item.text` — but only on the `{text}` fallback path; if the model emitted JSON,
  `text` doesn't exist. There is no guaranteed accessor (n8n nodes have declared
  output schemas; here the shape is emergent). This is the primary "they don't line
  up" symptom.
- **M2 — inline tool vs `tool_call` node emit different shapes for the same tool.**
  An agent's inline tool result lands under `item.tool_result`
  (`agent.rs:82`); the standalone `tool_call` node makes the result _be_ `item.json`
  (`tool_call.rs:31`). Moving a tool from inline to a node (or back) silently breaks
  every downstream expression.
- **M3 — fan-out doesn't fan integration nodes (the big one).** `agent`/`tool_call`/
  `http_request` **always emit exactly one item** and resolve config against
  `input.first()` only (`nodes/mod.rs:62`; agent test `emits_exactly_one_item…`
  agent.rs:245). Wire `split_out` (N items) → `tool_call` and the tool fires **once
  against item[0]**; the other N−1 items are silently dropped. In n8n that tool runs
  N times. "Send an email per row" quietly sends one email.
- **M4 — fan-in ignores ports and leaks untaken branches.** `collect_input`
  (`engine.rs:171`) concatenates items from **every** predecessor slot in state,
  regardless of `Edge.to_port` (stored but never read) or which port the predecessor
  actually emitted on. A node fed by a `condition` reads that condition's items even
  on the branch the run didn't take (the `false`-branch slot still exists in state),
  and `item` = whichever predecessor happens to be first in edge order. There are no
  named inputs (merge input A vs B is impossible).
- **M5 — merge produces a heterogeneous pile.** `merge` of `agent`(1 item) +
  `tool_call`(1 item) = 2 items of different shapes concatenated in predecessor
  order; downstream `item` sees only the first. No combine-by-key/position.

### Recommended fixes (in dependency order)

1. **Normalize `agent` output to a stable envelope** — e.g. always
   `{ "text": <string>, "json": <parsed-or-null>, "tool_result"?: … }` instead of
   passing the raw completion through. One place (`agent.rs:115`), immediately makes
   `=item.text` / `=item.json` reliable. Do the same conceptually for `tool_call`
   (wrap the Composio envelope as `{ "data": …, "ok": bool }`) so the accessor is
   predictable. This is the highest-leverage fix for the reported symptom.
2. **Unify inline-tool and node-tool result shape** (M2) — put the inline
   `tool_result` under the same envelope key the `tool_call` node uses.
3. **Add per-item execution to integration nodes** (M3) — map the node over
   `ctx.input` (resolving config per item), emitting one output item per input,
   carrying `paired_item`. This is the n8n mental model and eliminates the
   silent-drop trap. Gate behind a node config flag (`run_once` vs `run_per_item`) if
   batch-once must stay available.
4. **Honor `Edge.to_port` in `collect_input`** (M4) and only collect from the port
   the predecessor emitted on — fixes the untaken-branch leak and enables named
   merge inputs. Pairs with BUG-3/BUG-4 in §3.
5. **Add `merge.mode`** (append / combine-by-key / combine-by-position) (M5).
6. **Author-time alignment check for the agent** — a validate/lint pass that, given a
   producer's declared/known output shape, flags downstream `=item.<field>`
   references to fields the producer can't emit. Feeds the agent-facing tooling in §4.

## 5. Suggested execution order

1. **BUG-1** (env leak) — hotfix, one line of feature-gating.
2. **BUG-2** (switch/transform scope) — one-line fix each, plus a test.
3. **BUG-6 / BUG-5** (failed-run observer, sub-workflow HITL) — these corrupt run
   history / break approval guarantees; verify against OpenHuman's `FlowRunObserver`.
4. **BUG-3 / BUG-4** (branching/merge routing) — needs validation + lowering work; add
   the missing-branch and skipped-branch tests first.
5. **P0 ports**: webhook trigger wiring + per-item condition/switch mode.
6. **Agent tooling**: `patch_workflow` + structured validation + expose n8n import.
7. **P1 ports**: merge modes, wait node, expression interpolation + built-ins.

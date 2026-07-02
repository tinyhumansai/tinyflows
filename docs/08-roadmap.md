# 08 — Roadmap

Two phases: **Phase A** builds and perfects the standalone `tinyflows` crate;
**Phase B** integrates it into OpenHuman. Each stage lists what it delivers and
its exit criteria.

## Current status

- ✅ Crate scaffold + this documentation.
- ✅ **A0–A3 landed**: workflow model + validation; the compiler/engine
  (`engine::run` lowers onto tinyagents, per-run build, merge reducer, item-based
  `{run, nodes:{id:{items}}}` state); native control-flow nodes (condition,
  switch, merge, split_out, transform) with conditional routing — including real
  **parallel fan-out** (multiple same-port successors run concurrently) and a
  **merge fan-in barrier** (a waiting-edge join); and all capability-backed nodes
  (agent, tool_call, http_request, code, output_parser, sub_workflow) running
  against mock capabilities, with opaque `connection_ref` threaded through the
  LLM/tool/HTTP traits. A minimal `=`-expression module (`crate::expr`,
  dotted-path) covers the interim; full `jq`/`jaq`-style expressions are still
  deferred (O1).
- ✅ **A4 essentially done**: per-node error handling (`on_error`
  stop/continue/route + `retry` + `error` port); observability (`tracing`
  events plus the `Run`/`ExecutionStep` record model and the `RunObserver` hook,
  `src/observability.rs`, driven via `engine::run_with_observer`); and HITL —
  nodes with `requires_approval` pause (`RunOutcome::pending_approvals`) via a
  native tinyagents interrupt + `InMemoryCheckpointer`, and `engine::resume`
  completes the approve-and-continue loop. Load-time versioning/migration
  (`schema_version` + per-node `type_version`, `crate::migrate`) also landed.
  Remaining polish: retry **backoff timing** (retries are bounded but don't
  sleep — runtime-agnostic), per-node **timeouts**, and durable
  **checkpointed super-step replay** for resume.
- 🟡 **A5**: publish-ready (`cargo publish --dry-run` clean) but not yet
  published.
- ⬜ B0–B5 (OpenHuman host integration) not started.

The public runtime works end-to-end against mock capabilities, guarded by the
reference-workflow e2e suite (`tests/reference_workflows.rs`, feature `mock`).

## Phase A — the `tinyflows` crate

### A0 — Workflow model ✅ (landed)
- `WorkflowGraph` / `Node` / `Edge` / `Port` / `NodeKind` / `TriggerKind` with
  serde round-trip; `validate()` core checks; capability traits + mocks;
  `NodeExecutor` trait; compiler/engine entry points.
- **Exit:** model round-trips; validation covers ids/trigger/edges; CI green.
- **Landed:** model + validation ship; reference-workflow coverage moved into
  the `tests/reference_workflows.rs` e2e suite.

### A1 — Compiler + engine ✅ (landed)
- Add `tinyagents` dependency. Implement `compile()` → `CompiledGraph<Value>`
  (fresh per definition) and `engine::run()` for the minimal trigger→node path
  with a merge-tolerant reducer.
- **Exit:** a linear workflow runs end-to-end against mock capabilities.
- **Landed:** `engine::run` lowers a validated graph onto tinyagents with a
  per-run build, merge reducer, and item-based `{run, nodes:{id:{items}}}` state
  (item-based data flow, D13).

### A2 — Native control-flow nodes ✅ (landed)
- Implement `condition`, `switch`, `merge`, `split_out`, `transform`; choose and
  wire the expression library (`jaq` or `minijinja`).
- **Exit:** branch/switch/merge/parallel/split/loop covered by unit tests.
- **Landed:** all five control-flow nodes plus conditional routing in the engine
  (branch nodes route by chosen port), including real **parallel fan-out** (a node
  with multiple same-port successors runs them concurrently via `Command::goto` +
  `with_parallel`) and a **merge fan-in barrier** (a real waiting-edge join,
  `add_waiting_edge`). The expression side ships as a **minimal interim**
  `=`-module (`crate::expr`, dotted-path); full `jq`/`jaq`-style expressions
  remain deferred (O1).

### A3 — Capability-backed nodes ✅ (landed)
- Implement `agent` (with chat_model/memory/tool/output_parser sub-ports),
  `tool_call`, `http_request`, `code`, `output_parser`, `sub_workflow` against
  the capability traits.
- **Exit:** all five reference workflows run green against mock capabilities.
- **Landed:** every capability-backed node calls its `caps` trait and is
  exercised against the mocks by the reference-workflow e2e suite.

### A4 — Durability, HITL, observability ✅ (essentially done)
- Wire tinyagents checkpointing; mid-run `Interrupt`/`resume` approval steps;
  per-node retry/backoff + timeout; tracing spans.
- **Exit:** a workflow pauses for approval and resumes; a failing node retries.
- **Landed:** per-node error handling (`on_error` stop/continue/route + `retry`
  + `error` port); observability — `tracing` events **plus** the
  `Run`/`ExecutionStep` record model and the `RunObserver` hook
  (`src/observability.rs`), surfaced through `engine::run_with_observer`; and
  HITL — nodes with `requires_approval` pause (`RunOutcome::pending_approvals`)
  via a native tinyagents interrupt + `InMemoryCheckpointer`, with
  `engine::resume` completing the approve-and-continue loop.
- **Remaining polish:** retry **backoff timing** (retries are bounded but don't
  sleep — runtime-agnostic), per-node **timeouts**, and durable **checkpointed
  super-step replay** for resume (the checkpointer is wired, but resume currently
  re-runs with the approval; replay-resume is a future optimization).

### A5 — Docs, e2e, publish 🟡 (publish-ready)
- Finalize docs + `e2e/` reference scenarios + examples; **`cargo publish` to
  crates.io** (semver + `release.yml` publish-on-tag).
- **Exit:** `tinyflows = "x.y"` is installable; CI green on a tagged release.
- **Status:** publish-ready — `cargo publish --dry-run` is clean — but not yet
  published to crates.io.

## Phase B — OpenHuman integration

### B0 — Wire the crate
- Add `tinyflows = "x.y"` to root `Cargo.toml`; create the adapter seam
  `src/openhuman/tinyflows/` (mirror `src/openhuman/tinyagents/`) implementing the
  capability traits with OpenHuman services + a `SqlRunLedgerCheckpointer`.
- **Exit:** OpenHuman can compile and run a hard-coded workflow via the seam.

### B1 — `automations::` domain
- Canonical domain module: `automation_definitions` SQLite table (WorkflowGraph
  JSON) + CRUD RPCs (reuse crate `validate`); run via the `workflow_runs` ledger;
  enable/disable; register controllers in `src/core/all.rs`.
- **Exit:** create → get → start → run → history over JSON-RPC E2E.

### B2 — Triggers + safety
- Bridge `subconscious_triggers` (schedule/webhook/app-event/form/manual/chat/
  sub-workflow/eval/system) → start run; add `TrustedAutomationSource::Workflow`
  + per-automation outbound-approval toggle; boot-time re-resume; sandbox-only
  `code`.
- **Exit:** an app-event fires a workflow that performs a pre-declared external
  action under the trusted origin.

### B3 — Visual canvas
- React Flow (`@xyflow/react`): node palette w/ search, sub-node ports, wiring,
  per-node config panels (from node-kind schema + dynamic Composio schemas),
  run/inspect overlay with live status + mid-run HITL approval.
- **Exit:** author-and-run a multi-node workflow on the canvas.

### B4 — Agent-first authoring + templates
- Chat propose tool → proposal card → Save compiles to the same WorkflowGraph;
  starter-template catalog with connection chips + Add / Add & Enable.
- **Exit:** describe an automation in chat → save → it runs.

### B5 — Top-level page + polish
- Promote `/workflows` to a real page + bottom tab; "Your workflows" cards
  (status/trigger/toggle/⋮) + filters + search; relabel legacy Skills panel;
  run-history UI.
- **Exit:** the full Workflows surface ships.

## Open ordering choices
- Canvas (B3) vs agent-first (B4) order.
- `code` node default (sandboxed-only vs host-exec under Full autonomy).
- Expression lib (`jaq` vs `minijinja`).

See [decisions](11-decisions.md).

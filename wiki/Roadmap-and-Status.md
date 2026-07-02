# Roadmap and Status

tinyflows has a **working runtime**. The public API runs end-to-end against the
mock capabilities, and `cargo publish --dry-run` is clean.

Rust 2024 · MSRV 1.85 · `#![forbid(unsafe_code)]` · GPL-3.0-or-later.

## Implemented

- **Pipeline** — `WorkflowGraph → validate → compile → engine::run`, lowered
  per-run onto the `tinyagents` state-graph engine.
- **All 11 node kinds + trigger** — control flow (`condition`, `switch`, `merge`,
  `split_out`, `transform`) and capability-backed (`agent`, `tool_call`,
  `http_request`, `code`, `output_parser`, `sub_workflow`).
- **Routing** — linear paths, conditional branching, parallel fan-out, and a
  fan-in merge barrier.
- **Item-based data flow** — state as `{ run, nodes: { id: { items } } }` with
  `Item { json, binary?, paired_item? }` and `=`-prefixed expressions.
- **Per-node error handling** — `on_error` stop/continue/route, `retry`, and an
  `error` port.
- **Observability** — `tracing` plus a `RunObserver` and `Run` / `ExecutionStep`
  records.
- **Human-in-the-loop** — approval gating (`requires_approval` →
  `RunOutcome::pending_approvals`) with `engine::resume`.
- **Credentials** — opaque, host-managed `connection_ref`s (the crate never sees
  secrets).
- **Versioning** — `schema_version` / `type_version` axes plus load-time
  `migrate`.

## Not yet

Being honest about what's ahead:

- **Full expression engine** — a full jq/jaq (or templating) expression library.
  A minimal `=`-dotted-path interim ships today.
- **Retry timing** — backoff and per-node timeouts (retries currently re-attempt
  without a delay, keeping the crate runtime-agnostic).
- **Durable replay** — checkpointed super-step replay for resumable runs (resume
  is currently deterministic re-execution).
- **Authoring surfaces** — visual canvas and agent-first chat authoring
  (host-side).
- **OpenHuman host integration** — the first downstream host (a separate repo).
- **crates.io publish** — the crate is not yet published.

## Deeper reading

- [Architecture](Architecture) — how the pieces fit together.
- [Node Catalog](Node-Catalog) — the node kinds delivered so far.

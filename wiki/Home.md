# tinyflows

**tinyflows** is a Rust-native, host-agnostic workflow automation engine, shipped
as a library crate. A workflow is a directed graph of typed nodes
(`WorkflowGraph`) that is validated, compiled, and lowered per-run onto the
[`tinyagents`](https://crates.io/crates/tinyagents) state-graph engine, then
driven to completion by `engine::run`.

Rust 2024 · MSRV 1.85 · `#![forbid(unsafe_code)]` · GPL-3.0-or-later.

## Highlights

- **Typed graph model.** Workflows are `WorkflowGraph`s of typed nodes and edges;
  JSON is the wire format, with `schema_version` / `type_version` axes and
  load-time migration.
- **Host-agnostic capabilities.** Everything that touches the outside world —
  LLMs, integration tools, HTTP, code execution, durable state — is expressed as
  a capability trait the embedding host implements. Deterministic mocks ship
  behind the `mock` feature for tests and examples.
- **11 node kinds + a trigger.** Native control flow (`condition`, `switch`,
  `merge`, `split_out`, `transform`) and capability-backed effects (`agent`,
  `tool_call`, `http_request`, `code`, `output_parser`, `sub_workflow`).
- **Real routing.** Linear paths, conditional branching, parallel fan-out, and a
  fan-in merge barrier.
- **Item-based data flow.** State is a `serde_json::Value` laid out as
  `{ run, nodes: { id: { items } } }`; data on a connection is an array of
  `Item { json, binary?, paired_item? }`, with `=`-prefixed expressions.
- **Resilience & observability.** Per-node error handling (`on_error`
  stop/continue/route, `retry`, `error` port), `tracing` + `RunObserver`
  records, and human-in-the-loop approval gating (`requires_approval` →
  `RunOutcome::pending_approvals` + `engine::resume`).

## Pages

- [Getting Started](Getting-Started) — install and a runnable quickstart.
- [Architecture](Architecture) — the compile-and-run pipeline and state layout.
- [Node Catalog](Node-Catalog) — every node kind at a glance.
- [Capability Traits](Capability-Traits) — the host-injected seam.
- [Roadmap and Status](Roadmap-and-Status) — what's done and what's ahead.

## Going deeper

The full design specs live in the repository's
[`docs/`](../blob/main/docs/README.md) directory (start with the index). For a
quickstart and project layout, see the repo
[`README.md`](../blob/main/README.md).

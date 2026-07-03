# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

_No unreleased changes yet._

## [0.3.0] - YYYY-MM-DD

_Next (unreleased) minor._

### Added

- Integration nodes (`agent`, `tool_call`, `http_request`) now resolve `=`
  expressions in their config against the node's input, enabling inline
  data-binding from upstream output; new `expr::resolve` recursively evaluates
  `=`-expressions anywhere in a config value, and the binding scope is
  `{ item, items, run }` (the first input item, all input items, and the run
  payload). A minor bump is warranted because a config string starting with `=`
  now evaluates where it was previously carried through as a literal.

## [0.2.0] - YYYY-MM-DD

First functional release: the crate graduates from a skeleton to a working,
host-agnostic workflow engine.

### Added

- **Execution engine** (`engine::run`) that lowers a validated `WorkflowGraph`
  onto the [`tinyagents`](https://crates.io/crates/tinyagents) state-graph engine
  and drives it to completion, with an item-based data-flow contract passing
  lists of items between nodes.
- **Node catalog** with per-node executors:
  - Control-flow nodes: `condition`, `switch`, `merge`, `split_out`,
    `transform`.
  - Capability-backed nodes: `agent`, `tool_call`, `http_request`, `code`,
    `output_parser`, `sub_workflow` (nested graph execution).
- **Conditional routing** driven by node outputs, **parallel fan-out** to run
  branches concurrently, and a **merge fan-in barrier** that joins branches back
  together.
- **Per-node error handling**: configurable `on_error` behaviour, retry with
  backoff, and a dedicated error port for routing failures.
- **Run-level configuration**: overall run timeout and recursion-limit guards.
- **Observability**: `tracing` spans/events plus a `RunObserver` hook and
  structured `Run` / `ExecutionStep` records.
- **Human-in-the-loop approval gating**: workflows can pause with
  `pending_approvals` and continue via `engine::resume`.
- **Opaque `connection_ref` credentials** threaded through capability calls, so
  hosts resolve secrets without the crate ever seeing them.
- **Versioning and migration**: `schema_version` / `type_version` fields and a
  migration framework for evolving workflow definitions.
- **jq expression engine** backed by [`jaq`](https://crates.io/crates/jaq-core),
  with a dotted-path shorthand for simple field access.
- **Injectable checkpointer for durable, cross-process HITL resume**:
  `engine::run_with_checkpointer` / `resume_with_checkpointer` accept a
  host-implemented `Checkpointer<serde_json::Value>` keyed by a `thread_id`, so a
  run can pause at an approval gate, persist to the host's durable store, and
  resume later — even across a process restart. `Checkpointer`, `FileCheckpointer`,
  `InMemoryCheckpointer`, and `DurabilityMode` are re-exported from `tinyagents`.
  (The in-process `run_resumable` remains the simple path.)
- **`StateStore` wired into the `Capabilities` bundle**: the bundle now carries
  all five host capabilities (`llm`, `tools`, `http`, `code`, `state`), and nodes
  reach durable key/value state via `ctx.caps.state`.
- **Reference-workflow end-to-end test suite** and a runnable
  `hello_workflow` example.

## [0.1.1]

- Initial crate scaffold / skeleton release.

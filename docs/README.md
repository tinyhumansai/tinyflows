# tinyflows documentation

**tinyflows** is a Rust-native workflow automation engine. It models an
automation as a directed graph of typed nodes, compiles that graph onto the
[`tinyagents`](https://crates.io/crates/tinyagents) state-graph engine, and runs
it — while staying **host-agnostic** so any application (OpenHuman being the first)
can plug in its own LLM, integration, HTTP, and code-execution capabilities.

## Read in this order

| # | Doc | What it covers |
|---|-----|----------------|
| 00 | [Overview](00-overview.md) | Vision, goals, non-goals, how tinyflows relates to OpenHuman |
| 01 | [Architecture](01-architecture.md) | The three-layer stack, the host-agnostic rule, crate module map |
| 02 | [Workflow model](02-workflow-model.md) | `WorkflowGraph` / `Node` / `Edge` / `Port` / `NodeKind`, JSON format |
| 03 | [Node catalog](03-node-catalog.md) | Every node kind + trigger kind: config, ports, semantics, status |
| 04 | [Execution engine](04-execution-engine.md) | Compiler → tinyagents, `serde_json::Value` state, control-flow, HITL |
| 05 | [Capability traits](05-capability-traits.md) | `LlmProvider` / `ToolInvoker` / `HttpClient` / `CodeRunner` / `StateStore` |
| 06 | [Triggers](06-triggers.md) | Dynamic (app-event) vs built-in trigger kinds; host bridging |
| 07 | [Security](07-security.md) | Code sandboxing, network gating, curated tools, trusted-workflow origin |
| 08 | [Roadmap](08-roadmap.md) | Stages A0–A5 (crate) and B0–B5 (OpenHuman), with exit criteria |
| 09 | [OpenHuman integration](09-openhuman-integration.md) | When/what/how to integrate; trait → service mapping |
| 10 | [Reference workflows](10-reference-workflows.md) | The five reference automation flows as tinyflows JSON + coverage proof |
| 11 | [Decisions](11-decisions.md) | ADR log of every design decision and its rationale |
| 12 | [Testing](12-testing.md) | Test strategy: unit, compiler, e2e-vs-mocks, CI |
| 13 | [Data & expressions](13-data-and-expressions.md) | Item-based data flow, pairing, the expression/reference language |
| 14 | [Error handling](14-error-handling.md) | Per-node error policy, error port, retries, error-trigger workflows |
| 15 | [Credentials & connections](15-credentials-and-connections.md) | Opaque `connection_ref`s; secrets stay host-side |
| 16 | [Observability & runs](16-observability-and-runs.md) | Run / execution-step model, tracing, the inspect hook |
| 17 | [Node authoring](17-node-authoring.md) | How to add a new node kind (extension guide) |
| 18 | [Versioning & migration](18-versioning-and-migration.md) | Schema + per-node `type_version`, load-time migrations |
| 19 | [Feature matrix](19-feature-matrix.md) | Capability checklist with the stage each feature lands |
| 20 | [Glossary](20-glossary.md) | Terms used across the docs |

## Status

Early development. The crate ships a compiling **skeleton** (workflow model,
validation, capability traits, node-executor stubs, compiler/engine entry points)
plus this documentation. Node logic, the tinyagents compiler, and the OpenHuman
integration are staged — see the [roadmap](08-roadmap.md).

## Quick facts

- **Language / edition:** Rust 2024, MSRV 1.85, `#![forbid(unsafe_code)]`.
- **License:** GPL-3.0-or-later (compatible with OpenHuman, which is GPL v3).
- **Execution substrate:** `tinyagents` (added in stage A1).
- **Integration model:** published to crates.io; OpenHuman depends on it and adds
  an adapter seam at `src/openhuman/tinyflows/` — exactly like `tinyagents`.

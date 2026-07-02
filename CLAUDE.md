# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What tinyflows is

A **Rust-native workflow automation engine**, shipped as a library crate. A
workflow is a directed graph of typed nodes (`WorkflowGraph`) that is validated,
compiled onto the [`tinyagents`](https://crates.io/crates/tinyagents) state-graph
engine, and run. It is **host-agnostic**: the crate never hard-codes a vendor —
everything that touches the outside world (LLMs, tools, HTTP, code execution,
persistence) goes through **capability traits** the embedding app implements.
OpenHuman is the first downstream host; tinyflows is published to crates.io and
consumed there via a thin adapter seam.

## Architecture (pipeline)

```
model::WorkflowGraph  →  validate  →  compiler::compile  →  engine::run
   (typed graph)        (structural)   (lowers to tinyagents)  (drives to completion)
                                              ▲
                          caps traits (LlmProvider / ToolInvoker /
                          HttpClient / CodeRunner / StateStore) — host-injected
```

## Module map (`src/`)

- `model/` — workflow definition: `WorkflowGraph`, `Node`, `Edge`, `Port`, and
  `node_kind.rs` (the node-kind discriminators). JSON is the wire format (serde).
- `validate.rs` — structural validation, run before compile.
- `caps/` — host-injected capability traits (`caps/mod.rs`); `caps/mock.rs` has
  in-memory mock impls, gated behind the `mock` feature (always on inside tests).
- `nodes/` — `NodeExecutor` trait + dispatch; `control_flow.rs` (if/switch/merge/
  split_out/…) and `integration.rs` (agent/tool_call/http_request/code/…).
- `compiler.rs` — compiles a validated graph into runnable form.
- `engine.rs` — `engine::run`, drives a compiled workflow to completion.
- `error.rs` — shared error types across validate/compile/execute (thiserror).
- `lib.rs` — crate surface + module declarations; `main.rs` — thin binary stub.

## Conventions & invariants (respect these)

- **Rust 2024, MSRV 1.85.** `#![forbid(unsafe_code)]` and `#![warn(missing_docs)]`
  — every public item needs a doc comment; keep it that way.
- **Host-agnostic rule:** never hard-code an LLM/tool/HTTP/persistence vendor in
  the crate. New outside-world effects go through a `caps` trait, not a direct
  dependency. This is the core design constraint — do not violate it.
- **Declarative model:** no arbitrary embedded scripting in the workflow model;
  code execution is a sandboxed capability, not model logic.
- **License:** GPL-3.0-or-later. Keep new files compatible.

## Commands

```bash
cargo check                        # fast type/borrow check
cargo test                         # unit + compiler tests (mocks auto-available)
cargo test --features mock         # exercise the mock capabilities explicitly
cargo clippy --all-targets         # lint
cargo fmt                          # format (run before committing)
cargo build --release
```

## Docs

Design docs live in `local/docs/` (gitignored — moved out of the public repo,
symlinked into every worktree). **`local/docs/README.md` is the index, read it
first.** Notable: `local/docs/01-architecture.md`, `local/docs/02-workflow-model.md`,
`local/docs/03-node-catalog.md`, `local/docs/05-capability-traits.md`,
`local/docs/08-roadmap.md` (stages A0–A5 / B0–B5),
`local/docs/09-openhuman-integration.md`, `local/docs/11-decisions.md` (ADR log).
When you make a design decision, record it in `local/docs/11-decisions.md`.

## Status

Early development: a compiling **skeleton** (model, validation, capability traits,
node-executor stubs, compiler/engine entry points) plus docs. Node logic, the
tinyagents compiler, and OpenHuman integration are staged — see the roadmap.

# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What tinyflows is

A **Rust-native workflow automation engine**, shipped as a library crate. A
workflow is a directed graph of typed nodes (`WorkflowGraph`) that is validated,
compiled onto the in-crate [`graph`](src/graph) state-graph runtime, and run. It is **host-agnostic**: the crate never hard-codes a vendor —
everything that touches the outside world (LLMs, tools, HTTP, code execution,
persistence) goes through **capability traits** the embedding app implements.
OpenHuman is the first downstream host; tinyflows is published to crates.io and
consumed there via a thin adapter seam.

## Workspace layout

The repository is a **virtual workspace** — there is no root package. Every
crate lives in `crates/`, one directory per package, each directory named for
the package it holds. This is the same shape every other `tiny*` repository
uses, so a reader who knows one knows them all.

| Crate | What it owns |
| --- | --- |
| `crates/tinyflows` | The engine. One graph: validate → compile → run. Published to crates.io. |
| `crates/tinyflows-catalog` | The saved-flow model *around* a graph — flows, revisions, runs, drafts, suggestions — the in-process run/build cancellation registries, the format importers (n8n), and the save/run safety predicates. Storage-free. |
| `crates/tinyflows-sqlite` | SQLite implementations of the catalog and of the engine's checkpoint store, plus the JSON draft store. Every entry point takes a directory, never a host config type. |
| `crates/tinyflows-copilot` | The authoring copilot's *words*: the `workflow_builder` / `flow_discovery` standing archetypes, and the turn brief that opens a builder turn. Names no tool trait, no agent registry, no model client. |
| `crates/tinyflows-adaptive` | The adaptive loop over the engine: select or author a workflow, run it, judge it, learn. |

**Where a new thing goes.** Ask what it depends on, not what it is about. If it
needs storage, it is not `tinyflows-catalog`. If it needs a tool trait or a model
client, it is not `tinyflows-copilot` — that constraint is the whole reason the
copilot is reusable. If it needs to know which trigger kinds a particular host
dispatches, or what a `tool_call` slug resolves to, it is not in this repository
at all: it is a host overlay.

## Architecture (pipeline)

```
model::WorkflowGraph  →  validate  →  compiler::compile  →  engine::run
   (typed graph)        (structural)   (lowers to crate::graph)  (drives to completion)
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
  `caps/host/` has *real* implementations a host may opt into — out-of-process
  script/shell running, a file-backed `StateStore`, an allowlisted HTTP client —
  behind the `host-caps` feature. These are offered, never assumed: nothing in
  the engine reaches into them, and a host with a sandbox implements its own.
- `store/` — the durable model *around* a graph (versioned documents, run
  records, notes, proposals), a JSON file-backed store for it, and `authoring`
  (patch-based editing: apply → validate → gate → save), behind the `store`
  feature. Not part of the engine: `engine::run` neither reads nor writes any of
  it. `store::HostPolicy` is where a host injects the judgements only it can
  make — which harnesses exist, which slugs resolve.
- `bindings.rs` — reading the `=expr` bindings a graph declares: which node
  an expression reads from, and whether it reads as prose rather than jq.
- `gates/` — authoring gates: what is *guaranteed* wrong with a graph, refused
  before a write rather than surfacing as a silent null at run time. Only the
  host-agnostic ones; a host adds its own via `store::HostPolicy::check_graph`.
- `compat.rs` — topologies this engine's fan-in lowering cannot execute safely,
  refused before a run. A third question the other two do not ask: not "is this
  a well-formed graph" (`validate`) or "are its bindings resolvable" (`gates`),
  but "can this implementation actually run it". Fails closed.
- `preflight.rs` (behind `mock`) — the same class of failure as `gates`, caught
  by *running* the graph against the schema-aware mocks and reading the engine's
  own per-step null-resolution diagnostics. Three kinds of null are deliberately
  not reported — trigger-scoped, opaque upstream tool output, and a run that
  never settled — because each one is a correct graph.
- `caps/mock_schema_aware.rs` (behind `mock`) — mocks that answer the shape a
  node *declared* rather than echoing the request. Kept apart from `caps/mock`
  because they answer different questions: an echo is what an engine test wants
  and what a graph dry run must not have, since it fails a good graph's own
  `output_parser` sub-port.
- `nodes/` — `NodeExecutor` trait + dispatch; `control_flow.rs` (if/switch/merge/
  split_out/…) and `integration.rs` (agent/tool_call/http_request/code/…).
- `compiler.rs` — compiles a validated graph into runnable form.
- `engine.rs` — `engine::run`, drives a compiled workflow to completion.
- `graph/` — the in-crate state-graph runtime `engine.rs` lowers onto (builder,
  superstep executor, channels, checkpointing, interrupts, event journal).
  Vendored out of `tinyagents` and trimmed: agents themselves are a host concern
  reached through `caps`, so the crate carries no agent-harness dependency.
- `interception.rs` — the engine's one execution-*gating* hook. A `RunObserver`
  watches a run; a `StepInterceptor` can change one (substitute a node's output,
  inject a failure, patch the state it reads, park the activation). Always
  compiled, inert unless a run is started with `engine::run_intercepted`.
- `testkit/` — testing, mocking, and live debugging, behind the default-off
  `testkit` feature: programmable capability doubles with a call log (`mocks`),
  a structured run trace that names the upstream node behind every null binding
  (`trace`), `TestHarness` and its assertions (`harness`), breakpoints and debug
  sessions (`debug`), and all of it as agent-callable tools with JSON Schemas
  (`tools`). Built entirely on `interception` — nothing in it is special-cased
  inside the engine.
- `diagnostics.rs` / `evidence.rs` — reading a run's steps for what a green
  outcome hides, and bounding what gets handed back. Pure functions of engine
  records, so neither sits behind a feature; both are re-exported from
  `store::types` for callers that always reached them there.
- `error.rs` — shared error types across validate/compile/execute (thiserror).

**Where an authoring check belongs.** Three homes, and the distinction is what
it costs, not what it is about: `gates` is a pure function of the graph and runs
on every write; `compat` is likewise pure but answers for the *engine* rather
than the graph; `preflight` runs the thing. A check that needs a host's
vocabulary — which agent ids resolve, which tool slugs exist, which integrations
are connected — belongs in none of them and stays in the host.
- `lib.rs` — crate surface + module declarations; `main.rs` — thin binary stub.

## Conventions & invariants (respect these)

- **Rust 2024, MSRV 1.85.** `#![forbid(unsafe_code)]` and `#![warn(missing_docs)]`
  — every public item needs a doc comment; keep it that way.
- **Host-agnostic rule:** never hard-code an LLM/tool/HTTP/persistence vendor in
  the crate. New outside-world effects go through a `caps` trait, not a direct
  dependency. This is the core design constraint — do not violate it.
  `caps/host/` and `store/` do not weaken it: they are *optional* implementations
  behind default-off features, and the engine never depends on them. A host name
  (`medulla`, `openhuman`) must not appear in either.
- **Declarative model:** no arbitrary embedded scripting in the workflow model;
  code execution is a sandboxed capability, not model logic.
- **License:** GPL-3.0-or-later. Keep new files compatible.

## Commands

```bash
cargo check                        # fast type/borrow check
cargo test                         # unit + compiler tests (all optional modules compile in tests)
cargo test --features mock         # exercise the mock capabilities explicitly
cargo check --features host-caps   # the opt-in host capability implementations
cargo check --features store       # the file-backed workflow/run store
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

Working runtime. The engine (`engine::run`, lowering onto `crate::graph` with item-based
data flow), the full node catalog (control-flow + capability-backed), conditional +
parallel routing with a merge barrier, per-node error handling (`on_error`/retry/error
port), `tracing`/`RunObserver` observability, HITL approval gating + `engine::resume`,
opaque `connection_ref`, and schema/`type_version` versioning are all implemented and
tested (unit + reference-workflow e2e; `cargo publish --dry-run` clean). Also done:
full jq/jaq `=`-expressions (`src/expr.rs`, routed to `jaq`), retry backoff
(`fixed`/`exponential`) + per-node timeouts (`node_timeout_secs`), and
sub-workflows by inline graph **or** host `workflow_id` (resolved via the injected
`WorkflowResolver`, depth-bounded). Ahead: durable checkpointed super-step replay
and deeper OpenHuman host integration (Phase B). See `local/docs/08-roadmap.md`.

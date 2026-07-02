# tinyflows

**A Rust-native, host-agnostic workflow automation engine, shipped as a library
crate.**

tinyflows models an automation as a `WorkflowGraph` — a directed graph of typed
nodes — that is validated, compiled, and lowered per run onto the
[`tinyagents`](https://crates.io/crates/tinyagents) state-graph engine, then
driven to completion by `engine::run`. It is deliberately host-agnostic:
everything that touches the outside world — LLMs, integration tools, HTTP, code
execution, persistence — goes through capability traits the embedding
application implements, so the crate never hard-codes a vendor.

Rust 2024 · MSRV 1.85 · `#![forbid(unsafe_code)]` · GPL-3.0-or-later.

## Features

**Engine**

- Typed workflow model (`WorkflowGraph` of `Node`s and `Edge`s) with JSON as the
  wire format, structural validation, and per-run compilation onto `tinyagents`.
- Item-based data flow: a connection carries an array of items
  (`{ json, binary?, paired_item? }`); nodes map their logic over input items.
- `=`-prefixed config expressions (e.g. `"=item.name"`) resolved against the run
  scope.
- Linear execution, conditional routing on output ports, **parallel fan-out**
  (concurrent successors sharing a port), and a **merge fan-in barrier** (a node
  runs only once all its predecessors finish).

**Nodes**

- Full node catalog implemented and tested — control-flow (`condition`,
  `switch`, `merge`, `split_out`, `transform`) and capability-backed (`agent`,
  `tool_call`, `http_request`, `code`, `output_parser`, `sub_workflow`), plus the
  `trigger` entry node.

**Reliability**

- Per-node error handling: `on_error` policy (`stop` / `continue` / `route`),
  bounded `retry`, and an `error` output port for routing failures to a recovery
  sub-graph.
- Human-in-the-loop approval gating: a node with `requires_approval` pauses the
  run and is surfaced via `RunOutcome::pending_approvals`; `engine::resume`
  approves and continues.
- Observability via `tracing` plus a `RunObserver` hook and `Run` /
  `ExecutionStep` records.

**Extensibility**

- Host-injected capability traits: `LlmProvider`, `ToolInvoker`, `HttpClient`,
  `CodeRunner`, and `StateStore`. Deterministic in-memory mocks ship behind the
  `mock` cargo feature (`caps::mock::mock_capabilities()`).
- Opaque `connection_ref` credential references — the host resolves them to real
  secrets; the crate never sees them.
- Versioned wire format: graph `schema_version` and per-node `type_version`, with
  a `migrate` framework for load-time upgrades.

## How it works

```text
model::WorkflowGraph  ->  validate  ->  compiler::compile  ->  engine::run
   (typed graph)        (structural)     (validated handle)     (lowers onto
                                                                 tinyagents,
                                                                 drives to done)
```

`compile` validates the graph and returns an opaque `CompiledWorkflow`; the graph
is lowered onto a fresh `tinyagents` state graph once **per run**, inside
`engine::run`, which captures that run's capabilities in each node handler. Run
state is a single JSON value shaped as
`{ "run": { "trigger": … }, "nodes": { "<id>": { "items": [ … ] } } }`: a merge
reducer folds each node's item output under its own id, so independent nodes
never collide (which keeps parallel fan-out deterministic). Every outside-world
effect is reached through the `Capabilities` traits the host supplies for the
run.

## Quickstart

Add the crate:

```toml
[dependencies]
tinyflows = "0.1"
```

Build a `trigger -> transform` graph, compile it, and run it against the mock
capabilities. The `mock` feature provides the in-memory capability impls used by
tests and examples:

```rust
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let graph = WorkflowGraph {
        nodes: vec![
            Node {
                id: "t".into(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "start".into(),
                config: Value::Null,
                ports: vec![],
                position: None,
            },
            Node {
                id: "greet".into(),
                kind: NodeKind::Transform,
                type_version: 1,
                name: "greet".into(),
                config: json!({ "set": { "greeting": "=item.name" } }),
                ports: vec![],
                position: None,
            },
        ],
        edges: vec![Edge {
            from_node: "t".into(),
            from_port: "main".into(),
            to_node: "greet".into(),
            to_port: "main".into(),
        }],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "name": "Ada" }), &mock_capabilities())
        .await
        .expect("run");
    println!("{}", serde_json::to_string_pretty(&outcome.output).unwrap());
}
```

This is the [`hello_workflow`](examples/hello_workflow.rs) example — run it with:

```sh
cargo run --example hello_workflow --features mock
```

## Node catalog

| Kind | What it does |
|------|--------------|
| `trigger` | Entry node that starts the workflow (exactly one per graph); its firing mode is host-driven. |
| `agent` | Runs an LLM agent turn, with optional chat-model / memory / tool / output-parser sub-ports. |
| `tool_call` | Invokes one specific integration action deterministically (no LLM). |
| `http_request` | Performs an outbound HTTP request. |
| `code` | Runs sandboxed user code (JavaScript or Python). |
| `output_parser` | Parses / validates an upstream agent's output into a structured shape. |
| `sub_workflow` | Runs another workflow as a nested sub-graph and returns its output. |
| `condition` | Two-way IF; emits on the `true` or `false` port. |
| `switch` | Multi-way branch keyed by an expression result. |
| `merge` | Fan-in barrier that combines multiple inputs; waits for all wired predecessors. |
| `split_out` | Fan-out that emits one item per element of a list. |
| `transform` | Pure, expression-based data transform / field mapping over the run state. |

See the [Node Catalog](../../wiki/Node-Catalog) wiki page for config keys and
ports.

## Status

The Phase-A engine is complete: model, validation, per-run compilation and
lowering onto `tinyagents`, the full node catalog, item-based data flow with
`=`-expressions, linear / conditional / parallel-fan-out / merge-barrier routing,
per-node error handling (`on_error` / retry / error port), human-in-the-loop
approval gating (`pending_approvals` + `resume`), `tracing` + `RunObserver`
observability, opaque `connection_ref` credentials, and `schema_version` /
`type_version` migration. The runtime runs end-to-end against the mock
capabilities, guarded by a reference-workflow e2e suite, and
`cargo publish --dry-run` is clean.

Not yet:

- A full jq/jaq expression engine — a minimal `=`-dotted-path evaluator ships as
  an interim.
- Retry backoff timing and per-node timeouts.
- Durable, checkpointed super-step replay (`resume` currently re-executes
  deterministically).
- Visual and agent-first authoring (host-side).
- The OpenHuman host integration (Phase B, a separate repo).
- Publishing to crates.io.

## Building & testing

Install Rust 1.85 or newer with [rustup](https://rustup.rs/), then:

```sh
cargo build
cargo test                 # unit + compiler + engine tests (mocks auto-available)
cargo test --all-features  # also exercises the `mock` capability impls explicitly
```

The crate is `#![forbid(unsafe_code)]` and fully documented
(`#![warn(missing_docs)]`). The CI gate is:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## Documentation

The design and implementation guides live in the project
[wiki](../../wiki) — start with
[Getting Started](../../wiki/Getting-Started), then
[Architecture](../../wiki/Architecture) and the
[Node Catalog](../../wiki/Node-Catalog).

## Contributing

Contributions are welcome. Start with [`CONTRIBUTING.md`](CONTRIBUTING.md). In short:

1. Keep changes focused and easy to review.
2. Run the CI checks locally: `cargo fmt --all -- --check`,
   `cargo clippy --all-targets --all-features -- -D warnings`, and
   `cargo test --all-features`.
3. Include tests or documentation when behavior changes.
4. Follow the host-agnostic, no-`unsafe`, fully-documented conventions.

## License

tinyflows is licensed under the GNU General Public License, version 3 or later.
See [LICENSE](LICENSE) for the full license text.

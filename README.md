<h1 align="center">TinyFlows</h1>

<p align="center">
 <img src="https://github.com/tinyhumansai/tinyjuice/raw/main/docs/juice.png" />
</p>

**A Rust-native, host-agnostic workflow automation engine, shipped as a library
crate.**

tinyflows models an automation as a `WorkflowGraph` — a directed graph of typed
nodes — that is validated, compiled, and lowered per run onto the
in-crate `graph` state-graph runtime, then
driven to completion by `engine::run`. It is deliberately host-agnostic:
everything that touches the outside world — LLMs, integration tools, HTTP, code
execution, persistence — goes through capability traits the embedding
application implements, so the crate never hard-codes a vendor.

Rust 2024 · MSRV 1.85 · `#![forbid(unsafe_code)]` · GPL-3.0-or-later.

## Features

**Engine**

- Typed workflow model (`WorkflowGraph` of `Node`s and `Edge`s) with JSON as the
  wire format, structural validation, and per-run compilation onto the in-crate `graph` runtime.
- Item-based data flow: a connection carries an array of items
  (`{ json, binary?, paired_item? }`); nodes map their logic over input items.
- `=`-prefixed config expressions (e.g. `"=item.name"`) resolved against the run
  scope.
- Linear execution, conditional routing on output ports, **parallel fan-out**
  (concurrent successors sharing a port), and a **merge fan-in barrier** (a node
  runs only once all its predecessors finish).
- **Per-item fan-out** — a single node multiplying an array of input into N
  concurrent units of work, array in and array out. Where graph fan-out fixes
  the width when the graph is authored, this width is data-driven:

  ```jsonc
  // one agent turn per topic, at most 8 at a time
  { "kind": "agent", "config": {
      "execution": "per_item",   // map over the input array
      "concurrency": 8,          // 1 = sequential (default), n = bounded, 0/"all" = unbounded
      "prompt": "Research =item.name"
  } }

  // ...or one whole child workflow per item — the multiplier
  { "kind": "sub_workflow", "config": {
      "execution": "per_item", "concurrency": 4, "workflow_id": "deep_dive"
  } }
  ```

  Results always come back in **input order** with `paired_item` set, so a
  fan-out never reorders data. `on_item_error` decides what a failing item does
  to the batch — `collect` (the default when fanning out) marks that item
  `{ error, failed: true }` and keeps the rest, `fail_fast` (the default when
  sequential) hands the error to the node's `on_error` / retry policy, and
  `skip` drops it. Supported on `agent`, `tool_call`, `http_request`, `memory`,
  and `sub_workflow`.

**Nodes**

- Full node catalog implemented and tested — control-flow (`condition`,
  `switch`, `merge`, `split_out`, `transform`) and capability-backed (`agent`,
  `tool_call`, `http_request`, `code`, `shell`, `output_parser`, `sub_workflow`,
  `memory`), plus the `trigger` entry node.

**Reliability**

- Per-node error handling: `on_error` policy (`stop` / `continue` / `route`),
  bounded `retry`, and an `error` output port for routing failures to a recovery
  sub-graph.
- Node postconditions: `config.postcondition = { "require": "non_empty" }` (or
  `field_present` / `non_empty_list` with a dotted `field`) is checked against
  every emitted item, so an empty or misshapen output is a failed attempt that
  `retry` and `on_error` handle instead of flowing downstream.
- Human-in-the-loop approval gating: a node with `requires_approval` pauses the
  run and is surfaced via `RunOutcome::pending_approvals`; `engine::resume`
  approves and continues. A host can also drive durable, cross-process resume by
  injecting a `Checkpointer` via `engine::run_with_checkpointer` /
  `resume_with_checkpointer`.
- Human review as a graph step: an `approval` node carries what is being
  reviewed (a URL, a draft, any payload), reaches the human through the
  host-implemented `caps::ApprovalProvider`, and routes the verdict — reviewer,
  comment, any edit they made — on its `approved` / `rejected` ports. With no
  provider injected it degrades to the pause-and-resume gate above.
- Observability via `tracing` plus a `RunObserver` hook and `Run` /
  `ExecutionStep` records.

**Extensibility**

- Host-injected capability traits: `LlmProvider`, `ToolInvoker`, `HttpClient`,
  `CodeRunner`, `ShellRunner`, and `StateStore`. Deterministic in-memory mocks ship behind the
  `mock` cargo feature (`caps::mock::mock_capabilities()`).
- Opaque `connection_ref` credential references — the host resolves them to real
  secrets; the crate never sees them.
- Versioned wire format: graph `schema_version` and per-node `type_version`, with
  a `migrate` framework for load-time upgrades.
## How it works

```text
model::WorkflowGraph  ->  validate  ->  compiler::compile  ->  engine::run
   (typed graph)        (structural)     (validated handle)     (lowers onto
                                                                 crate::graph,
                                                                 drives to done)
```

`compile` validates the graph and returns an opaque `CompiledWorkflow`; the graph
is lowered onto a fresh `graph` state graph once **per run**, inside
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

## Examples

The crate ships eight runnable examples under [`examples/`](examples/). Each is
gated on the `mock` cargo feature, so run them with:

```sh
cargo run --example <name> --features mock
```

| Example               | What it shows                                                                                                                         |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------- | ------ |
| `hello_workflow`      | Build → compile → run a `trigger → transform` workflow against the mock capabilities.                                                 |
| `conditional_branch`  | IF routing: a `condition` node takes exactly one of its `true` / `false` branches.                                                    |
| `parallel_and_merge`  | Parallel fan-out (a node's same-port successors run concurrently) joined by a `merge` fan-in barrier.                                 |
| `capability_pipeline` | A linear `http_request → code → agent → tool_call` pipeline through the host capability traits (mocked).                              |
| `error_handling`      | Per-node `retry` plus `on_error: "route"` recovering a failing node via its `error` port.                                             |
| `hitl_approval`       | A `requires_approval` gate pauses the run (`pending_approvals`), then `run_resumable(...).resume(...)` continues from the checkpoint. |
| `hitl_review`         | An `approval` node against a host-implemented `ApprovalProvider`: the run suspends, a "human" approves with an edit, and the resume takes the `approved` branch. |
| `jq_expressions`      | The jaq-backed jq engine in a `transform` node (e.g. `=.item.prices                                                                   | add`). |

Omitting `--features mock` is harmless: the demo body is
`#[cfg(feature = "mock")]`-gated, so a default build stays green and the example
just prints a hint to re-run with the feature enabled.

### Testing and debugging workflows

Enable `testkit` for programmable mocks, a structured run trace, and real
breakpoints:

```toml
tinyflows = { version = "0.8", features = ["testkit"] }
```

The problem it exists for: a workflow that runs *green* and does nothing. Every
node ran, nothing errored, the output is an object — and a binding read from a
field no node produces, resolved to `null`, and sent an empty value onward. Null
is a legal value, so the engine has no complaint.

```rust,no_run
# async fn example(graph: tinyflows::model::WorkflowGraph) -> tinyflows::error::Result<()> {
use tinyflows::testkit::{Respond, TestHarness};
use serde_json::json;

let run = TestHarness::new(&graph)
    .trigger(json!({ "repo": "acme/api" }))
    .mock_tool("slack.send", Respond::value(json!({ "ok": true })))
    // First call rate-limits, the retry succeeds — a flaky dependency
    // without a flaky test.
    .mock_tool("gh.issues.*", Respond::sequence([
        Respond::error("429 rate limited"),
        Respond::value(json!({ "number": 7 })),
    ]))
    .run()
    .await?;

run.assert_completed();
run.assert_node_ran("send_email");
run.assert_no_null_bindings();   // the check a green run hides
run.assert_call_count("tools", Some("slack.send"), 1);
# Ok(())
# }
```

A failing `assert_no_null_bindings` names the binding *and* the upstream node it
was reading from, so it points at the node that should have produced the value
rather than only at the one that went without it.

Breakpoints pause a live run so another task can look at it and change it:

```rust,no_run
# async fn example(compiled: tinyflows::compiler::CompiledWorkflow) -> tinyflows::error::Result<()> {
use std::time::Duration;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::testkit::debug::{BreakpointSpec, DebugCommand, DebugSession};
use serde_json::json;

let mut session = DebugSession::start_quiet(compiled, json!({}), mock_capabilities())?;
session.controller().set_breakpoint(BreakpointSpec::before("send_email"))?;

if let Some(pause) = session.next_pause(Duration::from_secs(5)).await {
    println!("about to run with: {:?}", pause.input);
    println!("empty bindings: {:?}", pause.null_bindings);
    session.controller().release(pause.pause_id, DebugCommand::Continue)?;
}
session.finish().await?;
# Ok(())
# }
```

A paused run cannot wedge: a pause times out, detaching releases it, and
dropping the session winds it down.

#### For agents

Workflows here are written by agents as often as by people, and an agent that
cannot debug what it wrote can only guess at why it failed. `testkit::tools`
exposes all of the above as named tools with real JSON Schemas and a
JSON-in/JSON-out dispatcher, so a host can hand the whole module to an agent
without writing an adapter:

```rust,no_run
# async fn example() -> Result<(), tinyflows::testkit::tools::ToolError> {
use tinyflows::testkit::tools::{TestkitRegistry, all_tools};
use serde_json::json;

for tool in all_tools() {
    println!("{} — {}", tool.name, tool.summary);
}

let registry = TestkitRegistry::new();
let result = registry.dispatch("flow_test.run", json!({ "graph": { /* … */ } })).await?;
println!("{}", result["nullBindings"]);
# Ok(())
# }
```

tinyflows registers nothing and talks to no model — the host owns registration,
the same division `catalog` already draws for the node-kind contracts.

### Visual graph debugging

Enable `graph-debug` to render a workflow's nodes, port-labelled edges, branch
flows, loops, and even dangling references to a standalone PNG or JPEG. The
renderer is part of the library and does not require Graphviz:

```toml
tinyflows = { version = "0.6", features = ["graph-debug"] }
```

```rust,no_run
use tinyflows::model::WorkflowGraph;
use tinyflows::visualization::render_graph;

let graph = WorkflowGraph::default();
render_graph(&graph, "workflow.png")?; // .jpg and .jpeg are supported too
# Ok::<(), tinyflows::visualization::GraphRenderError>(())
```

Run all of them in one go:

```sh
for ex in hello_workflow conditional_branch parallel_and_merge \
          capability_pipeline error_handling hitl_approval hitl_review \
          jq_expressions; do
  cargo run --example "$ex" --features mock
done
```

## Node catalog

| Kind            | What it does                                                                                 |
| --------------- | -------------------------------------------------------------------------------------------- |
| `trigger`       | Entry node that starts the workflow (exactly one per graph); its firing mode is host-driven. |
| `agent`         | Runs an LLM agent turn, with optional chat-model / memory / tool / output-parser sub-ports.  |
| `tool_call`     | Invokes one specific integration action deterministically (no LLM).                          |
| `http_request`  | Performs an outbound HTTP request.                                                           |
| `code`          | Runs sandboxed user code (JavaScript or Python).                                             |
| `shell`         | Runs a shell script, inline or from a script file, with a working directory and environment. |
| `output_parser` | Parses / validates an upstream agent's output into a structured shape.                       |
| `sub_workflow`  | Runs another workflow as a nested sub-graph and returns its output.                          |
| `memory`        | Reads/writes host-managed memory (recall/search/flavour/people/remember/forget).             |
| `condition`     | Two-way IF; emits on the `true` or `false` port.                                             |
| `switch`        | Multi-way branch keyed by an expression result.                                              |
| `merge`         | Fan-in barrier that combines multiple inputs; waits for all wired predecessors.              |
| `split_out`     | Fan-out that emits one item per element of a list.                                           |
| `transform`     | Pure, expression-based data transform / field mapping over the run state.                    |
| `void`          | Terminal sink: discards its input and runs nothing downstream — the explicit dead end.       |

See the [Node Catalog](../../wiki/Node-Catalog) wiki page for config keys and
ports.

## Status

The Phase-A engine is complete: model, validation, per-run compilation and
lowering onto the in-crate `graph` runtime, the full node catalog, item-based data flow with
`=`-expressions, linear / conditional / parallel-fan-out / merge-barrier routing,
per-node error handling (`on_error` / retry / error port), human-in-the-loop
approval gating (`pending_approvals` + `resume`), `tracing` + `RunObserver`
observability, opaque `connection_ref` credentials, and `schema_version` /
`type_version` migration. The runtime runs end-to-end against the mock
capabilities, guarded by a reference-workflow e2e suite, and
`cargo publish --dry-run` is clean.

Also implemented:

- **A full jq/jaq expression engine.** Every `=`-prefixed string is a jq program
  compiled and executed by [`jaq`](https://crates.io/crates/jaq) with the
  evaluation scope as its input (`src/expr.rs`); a bare `=.item.name` dotted path
  is served by a fast structural walk, while anything richer (filters, pipes,
  `add`, …) routes to jaq.
- **Retry backoff timing and per-node timeouts.** A node's `retry` config takes
  `backoff_ms` plus `backoff: "fixed" | "exponential"` (capped at 60 s between
  attempts), and a run-level `node_timeout_secs` on the trigger applies a
  per-node timeout across the whole run.
- **Sub-workflows by reference.** A `sub_workflow` node runs a child either from
  an inline `workflow` graph or from a host-managed `workflow_id`, resolved
  through the injected `WorkflowResolver` capability. Nesting is depth-bounded
  and direct self-references are rejected.

Not yet:

- Automatic checkpointed super-step replay. Durable, cross-process resume is
  already supported by injecting a `Checkpointer`
  (`engine::run_with_checkpointer` / `resume_with_checkpointer`); only the
  super-step replay optimization that skips re-executing completed nodes on the
  in-process `resume` path remains.
- Visual and agent-first authoring (host-side).
- The OpenHuman host integration (Phase B, a separate repo).
- Publishing to crates.io.

## Building & testing

Install Rust 1.85 or newer with [rustup](https://rustup.rs/), then:

```sh
cargo build
cargo test                 # unit + compiler + engine tests (mocks auto-available)
cargo test --all-features  # also exercises optional host support
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

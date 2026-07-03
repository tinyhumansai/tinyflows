# Getting Started

## Install

tinyflows is a library crate. You need **Rust 1.85 or newer** (edition 2024);
install it with [rustup](https://rustup.rs/).

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
tinyflows = "0.1"
```

The crate is **host-agnostic**: outside-world effects go through capability
traits you implement (see [Capability Traits](Capability-Traits)). For tests and
examples, enable the `mock` feature to get deterministic, in-memory
implementations via `caps::mock::mock_capabilities()`:

```toml
[dev-dependencies]
tinyflows = { version = "0.1", features = ["mock"] }
```

## Quickstart

Build a two-node graph (`trigger → transform`), compile it, and run it against
the mock capabilities. This mirrors the crate's `examples/hello_workflow.rs`:

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

Notes:

- The `config` value on a `transform` node uses `=`-prefixed expressions (an
  interim `=`-dotted-path form ships today).
- `compile` runs structural validation and lowers the graph into a
  `CompiledWorkflow`; `run` drives it and returns a `RunOutcome` whose `output`
  is the final run state (`{ run, nodes: { id: { items } } }`).

## Examples

The crate ships the same program as a runnable example, alongside six more that
demonstrate the rest of the engine. All seven live under `examples/` and are
gated on the `mock` cargo feature, so run any of them with:

```bash
cargo run --example <name> --features mock
```

| Example | What it shows |
|---------|---------------|
| `hello_workflow` | Build → compile → run a `trigger → transform` workflow against the mock capabilities. |
| `conditional_branch` | IF routing: a `condition` node takes exactly one of its `true` / `false` branches. |
| `parallel_and_merge` | Parallel fan-out (a node's same-port successors run concurrently) joined by a `merge` fan-in barrier. |
| `capability_pipeline` | A linear `http_request → code → agent → tool_call` pipeline through the host capability traits (mocked). |
| `error_handling` | Per-node `retry` plus `on_error: "route"` recovering a failing node via its `error` port. |
| `hitl_approval` | A `requires_approval` gate pauses the run (`pending_approvals`), then `run_resumable(...).resume(...)` continues from the checkpoint. |
| `jq_expressions` | The jaq-backed jq engine in a `transform` node (e.g. `=.item.prices | add`). |

Omitting `--features mock` is harmless: each demo body is
`#[cfg(feature = "mock")]`-gated, so a default build stays green and the example
just prints a hint to re-run with the feature enabled.

To run all of them in one go:

```bash
for ex in hello_workflow conditional_branch parallel_and_merge \
          capability_pipeline error_handling hitl_approval jq_expressions; do
  cargo run --example "$ex" --features mock
done
```

Next: read [Architecture](Architecture) for how the pipeline works, or the
[Node Catalog](Node-Catalog) for the full node vocabulary.

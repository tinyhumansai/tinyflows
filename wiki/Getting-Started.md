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

## Run the example

The crate ships the same program as a runnable example:

```bash
cargo run --example hello_workflow --features mock
```

Next: read [Architecture](Architecture) for how the pipeline works, or the
[Node Catalog](Node-Catalog) for the full node vocabulary.

# tinyflows

tinyflows is a Rust-based workflow management project. The goal is to provide a
small, reliable foundation for building,
running, and managing automation workflows with Rust-native performance and
operational ergonomics.

## Status

tinyflows has a working runtime. The workflow engine (`engine::run`, which
compiles a validated graph onto the tinyagents state-graph engine with item-based
data flow), the full node catalog — control flow (condition, switch, merge,
split-out, transform) plus capability-backed nodes (agent, tool_call,
http_request, code, output_parser, sub_workflow) — per-node error handling
(`on_error` policy, retry, error port), and `tracing`-based observability are all
implemented and tested. The public runtime runs end-to-end against mock
capabilities, guarded by a reference-workflow e2e suite. Still ahead: durable
checkpointing and human-in-the-loop approval, visual and agent-first authoring
(host-side), and publishing to crates.io.

## Documentation

The design and implementation docs live in [`docs/`](docs/README.md) — start with
the [index](docs/README.md), then [Overview](docs/00-overview.md) and
[Architecture](docs/01-architecture.md). The [roadmap](docs/08-roadmap.md) tracks
staged delivery, and [OpenHuman integration](docs/09-openhuman-integration.md)
covers how tinyflows is embedded downstream.

## Project Layout

- `src/` - Rust crate source (`model`, `validate`, `caps`, `nodes`, `compiler`,
  `engine`, `error`).
- `docs/` - Design documentation (see [`docs/README.md`](docs/README.md)).
- `e2e/` - End-to-end testing assets and scenarios.
- `wiki/` - GitHub Wiki source material.

## Getting Started

Install Rust 1.85 or newer with [rustup](https://rustup.rs/), then run:

```sh
cargo build
cargo test
```

To run the command-line entry point:

```sh
cargo run
```

To use the crate from Rust:

```rust
assert_eq!(tinyflows::product_name(), "tinyflows");
```

## Contributing

Contributions are welcome. Start with [`CONTRIBUTING.md`](CONTRIBUTING.md) and the
[coding guidelines](docs/21-coding-guidelines.md). In short:

1. Keep changes focused and easy to review.
2. Run the CI checks locally: `cargo fmt --all -- --check`,
   `cargo clippy --all-targets --all-features -- -D warnings`, and
   `cargo test --all-features`.
3. Include tests or documentation when behavior changes.
4. Follow the host-agnostic, no-`unsafe`, fully-documented conventions.

## License

tinyflows is licensed under the GNU General Public License, version 3 or later.
See [LICENSE](LICENSE) for the full license text.

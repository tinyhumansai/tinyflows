# tinyflows

tinyflows is a Rust-based workflow management project inspired by tools like
Zapier and n8n. The goal is to provide a small, reliable foundation for building,
running, and managing automation workflows with Rust-native performance and
operational ergonomics.

## Status

tinyflows is in early development. The repository currently contains the initial
Rust crate and project scaffolding. The public library surface is intentionally
small while the workflow runtime takes shape.

## Project Layout

- `src/` - Rust application source.
- `docs/` - Project documentation.
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

Contributions are welcome. Before opening a pull request, please:

1. Keep changes focused and easy to review.
2. Run `cargo fmt` and `cargo test`.
3. Include tests or documentation when behavior changes.
4. Explain the workflow use case your change supports.

## License

tinyflows is licensed under the GNU General Public License, version 3 or later.
See [LICENSE](LICENSE) for the full license text.

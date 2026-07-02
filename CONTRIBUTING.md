# Contributing to tinyflows

Thanks for your interest in contributing! **tinyflows** is a Rust-native,
host-agnostic workflow engine, licensed **GPL-3.0-or-later**.

## Before you start

- Skim the design docs — [`docs/README.md`](docs/README.md) is the index.
- Read the **[coding guidelines](docs/21-coding-guidelines.md)** — they're the
  standard every change is held to.
- For anything non-trivial, **open an issue first** (use the
  [issue templates](.github/ISSUE_TEMPLATE)) so we can agree on the approach.

## Development setup

Install Rust **1.85 or newer** via [rustup](https://rustup.rs/), then:

```bash
cargo build                 # build
cargo test                  # unit + doc tests (mocks auto-available)
cargo test --all-features   # exercise the `mock` feature too
```

## Run the CI checks locally

CI runs the following on every push and PR — run them yourself first, and make
sure they're **all green**:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all-features
```

## Coding standards (the short version)

Full detail in [`docs/21-coding-guidelines.md`](docs/21-coding-guidelines.md). The
non-negotiables:

- **Host-agnostic:** never hard-code an LLM/tool/HTTP/persistence vendor — outside-
  world effects go through a capability trait, not a direct dependency.
- **No `unsafe`** (`#![forbid(unsafe_code)]`).
- **Document every public item** (`#![warn(missing_docs)]`), with runnable
  examples where practical.
- **No panics** (`unwrap`/`expect`/`panic!`) on paths reachable from user input;
  return a typed `Result` error instead.
- **Declarative model** — no embedded scripting; code runs only via the sandboxed
  `code` capability.
- Keep dependencies minimal, justified, additive, and GPL-compatible.

## Commits & pull requests

- Small, focused commits with an imperative subject ("Add X", "Fix Y"); the body
  explains **why**. Reference issues (`Fixes #123`) where relevant.
- Branch from `main`, open a PR against `main`, and fill in the
  [PR template](.github/PULL_REQUEST_TEMPLATE.md).
- Update docs alongside behavior/API changes; record design decisions as an ADR in
  [`docs/11-decisions.md`](docs/11-decisions.md).
- Keep PRs reviewable — separate large refactors from behavior changes.

## Licensing

By contributing, you agree that your contributions are licensed under
**GPL-3.0-or-later**, consistent with the rest of the project. Don't add code or
dependencies under an incompatible license.

## Security

Please **do not** open a public issue for security-sensitive reports. Instead,
contact the maintainers privately so a fix can be prepared before disclosure.

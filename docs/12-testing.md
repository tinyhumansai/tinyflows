# 12 — Testing

Testing tracks the [roadmap](08-roadmap.md): the crate is tested in isolation
against mocks; OpenHuman is tested at the seam and end-to-end.

## Crate (this repo)

### Layers
- **Model** — serde round-trip; helper methods (`trigger`, `node`, `successors`).
- **Validation** — each `ValidationError` path (missing/multiple triggers,
  duplicate ids, dangling edges) + accepting valid graphs.
- **Compiler** — valid graph compiles; invalid graph rejected; (A1+) lowering
  produces a runnable graph for each control-flow shape.
- **Nodes** — per-kind unit tests (A2 native nodes fully; A3 capability nodes via
  mocks).
- **Engine** — branch / switch / merge / parallel / split / loop behaviors; HITL
  interrupt→resume; retry/timeout (A4).
- **Reference workflows** — the five [reference workflows](10-reference-workflows.md)
  run green against `mock_capabilities()` in `e2e/` (A3/A5).

### Running
```sh
cargo test                 # default features
cargo test --all-features  # includes the `mock` feature surface
```

### CI gates (`.github/workflows/ci.yml`)
Every push/PR runs, and must pass:
```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets [--all-features]
cargo test [--all-features]
```
Because `-D warnings` + `#![warn(missing_docs)]` are in force, **every public item
needs a doc comment** and the crate must be warning-free. The skeleton already
satisfies all of these.

### Conventions
- No `unsafe` (`#![forbid(unsafe_code)]`).
- Async tests use `#[tokio::test]` (tokio is a dev-dependency).
- Mocks are deterministic echoes; no network, no time flakiness.

## OpenHuman (host, Phase B)

- **Adapter-seam tests** — each capability trait implementation against real
  services with the shared mock backend (`scripts/test-rust-with-mock.sh`).
- **`automations::` domain** — CRUD + validation + enable/disable + trigger→run
  bridge + trusted-origin gating + sandboxed code exec.
- **RPC E2E** (`tests/json_rpc_e2e.rs`) — create → start → assert outcome for a
  workflow exercising IF + Merge + HTTP(mock) + sandboxed Code.
- **Frontend** — Vitest (canvas add/wire/config, proposal card, cards/filters);
  WDIO (`/workflows` nav + ROUTES; author-and-run on the canvas).
- **Manual (`/verify`)** — reproduce a reference workflow on the canvas *and* via
  chat; confirm identical WorkflowGraph, run, and history.
- **Coverage** — OpenHuman enforces ≥80% coverage on changed lines.

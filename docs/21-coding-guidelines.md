# 21 — Coding guidelines

Coding standards for the **tinyflows** crate. All new code and reviews follow
these. They combine general Rust best practice with this crate's specific
invariants. Canonical references are linked at the [bottom](#references).

> New to the codebase? Read [`docs/README.md`](README.md) for the design first,
> then this file, then [`CONTRIBUTING.md`](../CONTRIBUTING.md) for the workflow.

## 0. Golden rules (non-negotiable)

1. **Host-agnostic.** Never hard-code an LLM/tool/HTTP/persistence vendor in the
   crate. Anything touching the outside world goes through a [`caps`](05-capability-traits.md)
   trait the host implements — not a direct dependency. This is the core design
   constraint; see [architecture](01-architecture.md).
2. **No `unsafe`.** The crate is `#![forbid(unsafe_code)]`. There are no exceptions.
3. **Everything public is documented.** The crate is `#![warn(missing_docs)]`;
   treat the warning as an error.
4. **Declarative model.** No arbitrary embedded scripting in the workflow model;
   code execution is a sandboxed capability, not model logic (see
   [D6](11-decisions.md), [security](07-security.md)).
5. **CI is the gate.** `fmt`, `clippy -D warnings`, `build`, and `test` — in both
   the default and `--all-features` configurations — must pass. See §1.
6. **License.** GPL-3.0-or-later. New files inherit it; keep dependencies compatible.

## 1. Toolchain & the local check loop

- **Rust 2024 edition, MSRV 1.85** (`rust-version` in `Cargo.toml`). Don't use APIs
  newer than the MSRV. CI builds on `stable`.
- Run the **exact CI checks** locally before pushing — they must all be clean:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --all-targets -- -D warnings
  cargo clippy --all-targets --all-features -- -D warnings
  cargo build --all-targets --all-features
  cargo test --all-features
  ```

- Never merge with warnings. Don't silence Clippy with `#[allow(...)]` unless you
  add a one-line comment justifying it and scope it as narrowly as possible.

## 2. Formatting & naming

- **rustfmt is authoritative** (defaults). Run `cargo fmt --all`; never hand-format
  in a way fmt would undo. ~100-column width, 4-space indent.
- **Naming** (per the API Guidelines): `snake_case` for modules/functions/vars,
  `UpperCamelCase` for types/traits/enum variants, `SCREAMING_SNAKE_CASE` for
  consts/statics. Treat acronyms as words: `HttpClient`, not `HTTPClient`.
- **Imports:** group `std` / external / crate; prefer explicit imports; avoid glob
  imports except a crate prelude and `use super::*;` inside `#[cfg(test)]`.
- Prefer early returns and `?` over deep nesting; prefer `let … else` and `if let`.

## 3. Error handling

- Library errors are `thiserror`-derived enums in [`error.rs`](../src/error.rs).
  No `anyhow` in the library surface (fine in tests/examples).
- Return `Result<T, Error>`. **Do not** `unwrap()` / `expect()` / `panic!` /
  `unreachable!` on any path reachable from user input or the public API. If a
  panic is genuinely impossible, prove it: use `expect("invariant: …")` that states
  the invariant, or document a `# Panics` section.
- No `todo!()` / `unimplemented!()` in shipped paths — unfinished stubs return a
  typed `Error::Unimplemented`, as the current skeleton does.
- Make the public error enum `#[non_exhaustive]` so adding variants isn't a
  breaking change. Use `#[from]` to wrap sources; keep messages actionable and
  carry context (e.g. offending node id / port) for validation errors.

## 4. Public API design

Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
Key expectations here:

- **Derive the usual traits** where sensible: `Debug` on **all** public types;
  `Clone`, `PartialEq`/`Eq`, `Hash`, `Default` when meaningful; `Serialize` +
  `Deserialize` on model types.
- **`#[non_exhaustive]`** on public enums/structs that will grow — node kinds,
  trigger kinds, error, config — so additions stay backward-compatible.
- **Borrow in, own out:** take `&str` / `&[T]`; return owned values. Use
  `impl Into<String>` / generics judiciously — don't over-genericize.
- **Don't leak external crate types** through the public API. Capability traits are
  the only intended extension points; keep the seam thin.
- Prefer **newtypes** over primitive-obsession for identifiers; **builders** for
  many-optional-field construction; **sealed traits** for anything not meant to be
  implemented downstream.
- The **JSON wire format is public API.** Field renames/removals are semver-breaking
  — coordinate with versioning ([D17](11-decisions.md),
  [18-versioning](18-versioning-and-migration.md)).

## 5. Documentation

- Every `pub` item — module, type, field, variant, function, trait, method — needs
  a doc comment. First line is a one-sentence summary.
- Use the standard sections: `# Examples`, `# Errors` (for `Result`-returning fns),
  `# Panics` (if it can panic).
- Prefer **runnable examples**; doctests run under `cargo test`, so keep them
  compiling. The crate-level `//!` doc carries the mental model + a minimal example.
- Use intra-doc links (`[Type]`, `[`module::item`]`). Link module docs to the
  relevant design doc. Comments explain **why**, not what; keep them current.

## 6. Types, ownership & performance

- **Borrow over clone.** Clone explicitly and only when needed — never `.clone()`
  just to appease the borrow checker without thought. Avoid needless allocations
  and intermediate `collect()`; prefer iterators.
- **Exhaustive `match`** on our own enums — avoid a catch-all `_` so a new variant
  surfaces as a compile error instead of silently falling through.
- Run-state is `serde_json::Value` by design ([D5](11-decisions.md)); keep
  everything else strongly typed. Don't let dynamic JSON leak into places that
  should be typed.
- No premature optimization — measure first — but don't allocate in hot loops
  needlessly.

## 7. Async

- Capability traits are async via `async-trait` (object-safe). **Never block the
  executor** (`std::thread::sleep`, sync IO) inside async code — the host owns the
  runtime.
- Keep the crate **runtime-agnostic**: no hard dependency on a specific async
  runtime in the library (`tokio` is a dev-dependency only, for tests).
- Ensure futures are `Send` where the host's executor requires it.

## 8. Serde & the JSON model

- Model types derive `Serialize`/`Deserialize`. Apply `#[serde(rename_all =
  "snake_case")]` consistently for the wire format. Use tagged enums with clear
  discriminators for node/trigger kinds.
- Weigh `#[serde(deny_unknown_fields)]` (catches typos) against forward-compat and
  the migration story ([18](18-versioning-and-migration.md)). Keep the schema
  documented in [02-workflow-model](02-workflow-model.md).

## 9. Dependencies & features

- **Minimal and justified.** A new dependency must be necessary, well-maintained,
  GPL-compatible, and must **not** pull a vendor into the crate (§0.1). Prefer std,
  then a small, widely-used crate.
- Keep `Cargo.lock` committed. **Features are additive**; default features stay
  minimal (`default = []`). The `mock` feature gates in-memory test helpers only —
  no production behavior behind features.

## 10. Testing

- Unit tests in `#[cfg(test)] mod tests` beside the code; cross-module/integration
  tests in `tests/`.
- Use the [`mock`](05-capability-traits.md) capabilities for anything touching a
  `caps` trait — **tests never hit the network or a real LLM.** Tests are
  deterministic (no wall-clock / ordering / network reliance).
- Cover validation failures, compiler lowering, and node execution. Keep the
  reference workflows ([10](10-reference-workflows.md)) as golden JSON fixtures.
- Everything passes `cargo test` **and** `cargo test --all-features`; doctests count.

## 11. Safety & security

- No `unsafe` (forbidden). No secrets in code, tests, or fixtures — credentials are
  **opaque references** resolved host-side ([D15](11-decisions.md),
  [15-credentials](15-credentials-and-connections.md)).
- User code runs only through a sandboxed capability, never in-process eval
  ([D6](11-decisions.md)). Treat trigger payloads as **untrusted** data
  ([D10](11-decisions.md), [07-security](07-security.md)).
- Don't log secrets; be deliberate about `Debug` for any type that may carry
  sensitive data.

## 12. Commits, branches & PRs

- Small, focused commits. Imperative subject ("Add X", "Fix Y"), ~50 chars; the
  body explains **why**. Separate refactors from behavior changes.
- Branch from `main`, PR against `main`, and fill the
  [PR template](../.github/PULL_REQUEST_TEMPLATE.md). CI must be green.
- Update docs with any behavior/API change; record design decisions as an ADR in
  [11-decisions](11-decisions.md). See [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## References

- **Rust API Guidelines** — <https://rust-lang.github.io/api-guidelines/>
- **The Rust Programming Language** — <https://doc.rust-lang.org/book/>
- **Rust By Example** — <https://doc.rust-lang.org/rust-by-example/>
- **Clippy lints** — <https://rust-lang.github.io/rust-clippy/>
- **rustfmt** — <https://github.com/rust-lang/rustfmt>
- **The Cargo Book** (features, SemVer) — <https://doc.rust-lang.org/cargo/>
- **Rust API SemVer reference** — <https://doc.rust-lang.org/cargo/reference/semver.html>

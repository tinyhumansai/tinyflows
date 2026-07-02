# 05 — Capability traits

Capability traits are how tinyflows stays [host-agnostic](01-architecture.md).
The crate defines the *interfaces*; the host provides the *implementations* and
passes them in via a [`Capabilities`](../src/caps/mod.rs) bundle for each run.
Node executors call these traits — they never call a vendor directly.

Defined in [`src/caps/mod.rs`](../src/caps/mod.rs). All are `async` (via
`async-trait`) and `Send + Sync` so they work behind `Arc<dyn _>`.

## The traits

### `LlmProvider`
```rust
async fn complete(&self, request: Value) -> Result<Value>;
```
Runs one LLM completion. Used by `agent` and `output_parser` nodes. The request
and response shapes are JSON so the crate needn't model any provider's schema.

### `ToolInvoker`
```rust
async fn invoke(&self, slug: &str, args: Value) -> Result<Value>;
```
Executes a named integration action for `tool_call` nodes (and agent tool
sub-ports). The host decides which slugs exist and enforces curation/scoping.

### `HttpClient`
```rust
async fn request(&self, request: Value) -> Result<Value>;
```
Performs an outbound HTTP request for `http_request` nodes. The host applies
allowlisting, DNS-rebind protection, and autonomy gating.

### `CodeRunner`
```rust
async fn run(&self, language: CodeLanguage, source: &str, input: Value) -> Result<Value>;
```
Executes sandboxed user code for `code` nodes. `CodeLanguage` is `JavaScript` or
`Python`. **The host is responsible for sandboxing** (see [security](07-security.md)).

### `StateStore`
```rust
async fn load(&self, key: &str) -> Result<Option<Value>>;
async fn store(&self, key: &str, value: Value) -> Result<()>;
```
Optional durable key/value state for stateful/resumable workflows. (Graph
checkpointing is a separate concern handled by tinyagents' `Checkpointer`, wired
by the host.)

> **Status:** the trait (and `MockStateStore`) are defined, but `StateStore` is
> **not yet part of the `Capabilities` bundle** (below) or `mock_capabilities()` —
> it's wired in when stateful workflows land. Today the per-run bundle is the four
> traits above.

## The `Capabilities` bundle

```rust
pub struct Capabilities {
    pub llm:   Arc<dyn LlmProvider>,
    pub tools: Arc<dyn ToolInvoker>,
    pub http:  Arc<dyn HttpClient>,
    pub code:  Arc<dyn CodeRunner>,
}
```
Constructed once per run and passed to [`engine::run`](04-execution-engine.md).

## Mocks

[`src/caps/mock.rs`](../src/caps/mock.rs) provides deterministic echo
implementations (`MockLlm`, `MockTools`, `MockHttp`, `MockCode`, `MockStateStore`)
and `mock_capabilities()`. They are available inside this crate's tests
automatically and to downstream crates via the `mock` cargo feature — enough to
run the [reference workflows](10-reference-workflows.md) with no external
services.

## How OpenHuman implements them

| Trait | OpenHuman implementation |
|-------|--------------------------|
| `LlmProvider` | the inference stack / provider layer |
| `ToolInvoker` | curated Composio tools (`is_action_visible_with_pref`) + agent `spawn_agent` |
| `HttpClient` | `HttpRequestTool` (allowlist + DNS guard, Network-gated) |
| `CodeRunner` | `node_exec` (JS) / `runtime_python` (Py) via `sandbox::execute_in_sandbox` |
| checkpointing | `SqlRunLedgerCheckpointer` (SQLite) — passed to tinyagents |

Details and file references in [OpenHuman integration](09-openhuman-integration.md).

# Capability Traits

tinyflows is **host-agnostic**: the crate never hard-codes an LLM, tool, HTTP,
code, or persistence vendor. Everything that touches the outside world is
expressed as a trait the **embedding host implements**. This is the core design
constraint — new outside-world effects go through a capability trait, not a direct
dependency.

The host constructs these implementations and hands them to the engine per run.
Deterministic, in-memory mocks ship behind the `mock` feature
(`caps::mock::mock_capabilities()`) so workflows run end-to-end in tests and
examples without any real backend.

## The traits

| Trait | Used by | Purpose |
|-------|---------|---------|
| `LlmProvider` | `agent`, `output_parser` | Runs a single LLM completion from a JSON request, returning JSON. |
| `ToolInvoker` | `tool_call` | Invokes a named integration action (`slug` + `args`), returning its output. |
| `HttpClient` | `http_request` | Issues an outbound HTTP request described by JSON, returning the response. |
| `CodeRunner` | `code` | Executes sandboxed user code (`CodeLanguage::JavaScript` / `Python`) with a JSON input. |
| `StateStore` | resumable / stateful workflows | Durable key/value state (`load` / `store`) for a run. |

## Connection references

`LlmProvider`, `ToolInvoker`, and `HttpClient` each take an optional `conn: Option<&str>`
— an **opaque, host-managed connection reference** (e.g. a credential or Composio
connection id) that names the account a call acts as. The crate never sees real
secrets; the host resolves the reference to credentials inside its implementation.

## Data-binding in config

The capability-backed integration nodes (`agent`, `tool_call`, `http_request`)
resolve `=` expressions embedded anywhere in their config before the config
reaches the host implementation. Each expression is evaluated against the
`{ item, items, run }` scope — the node's first input item, all of its input
items, and the run payload — so a node can bind upstream output straight into its
parameters (e.g. `args: { "text": "=item.name" }`). Values that do not start with
`=` pass through as literals, so existing config is unaffected.

## The `Capabilities` bundle

The engine receives a `Capabilities` struct — the per-run bundle of host
implementations. It bundles all five host capabilities: `llm`, `tools`, `http`,
`code`, and `state` (each an `Arc<dyn Trait>`). Nodes reach each one through
`ctx.caps` during execution — for example, durable key/value state via
`ctx.caps.state`.

Durable, cross-process human-in-the-loop resume is available by implementing
`Checkpointer<serde_json::Value>` and driving the run via
`engine::run_with_checkpointer` / `resume_with_checkpointer` under a stable
`thread_id`: a run can persist its paused approval boundary to the host's store
and resume later, even after a process restart. The in-process `run_resumable`
remains the simple, non-durable path.

## Deeper reading

- [Architecture](Architecture) — how the engine drives a compiled workflow.
- [Node Catalog](Node-Catalog) — the nodes that consume these capabilities.

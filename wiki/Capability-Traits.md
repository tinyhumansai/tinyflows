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

## The `Capabilities` bundle

The engine receives a `Capabilities` struct — the per-run bundle of host
implementations. It currently wires four: `llm`, `tools`, `http`, and `code`
(each an `Arc<dyn Trait>`). `StateStore` is defined for durable, resumable state
and is implemented by hosts that need it.

## Deeper reading

- [Architecture](Architecture) — how the engine drives a compiled workflow.
- [Node Catalog](Node-Catalog) — the nodes that consume these capabilities.

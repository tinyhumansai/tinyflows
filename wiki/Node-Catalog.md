# Node Catalog

Every node has a `NodeKind`. Kind-specific settings live in the node's `config`
(free-form JSON, validated per kind). Ports carry the item arrays between nodes;
the default port is `main`.

## Trigger

Exactly one per workflow — the graph's entry node. Its firing mode is a
`TriggerKind` in config (`manual`, `schedule`, `webhook`, `app_event`, `form`,
`execute_by_workflow`, `chat_message`, `evaluation`, `system`). The host actually
fires it; tinyflows injects the trigger payload as the initial run state.

| Node | Purpose | Ports / config gist |
|------|---------|---------------------|
| `trigger` | Entry node that starts the run | Out `main`; config `trigger_kind` |

## Control-flow nodes (native)

Native routing logic — no host capabilities required.

| Node | Purpose | Ports / config gist |
|------|---------|---------------------|
| `condition` | Two-way IF branch | Out `true` / `false`; config: boolean expression |
| `switch` | Multi-way branch keyed by an expression | Out one port per case (+ optional `default`); config `expression`, `cases` |
| `merge` | Fan-in barrier combining multiple inputs | Waits for all wired inputs; config `mode` (e.g. `append`) |
| `split_out` | Fan-out: one item per element of a list | Downstream runs per item; config `path` |
| `transform` | Pure, expression-based field mapping | Config `set` (field → `=`-expression map) |

## Capability-backed nodes

Reach the outside world through the host-injected [capability
traits](Capability-Traits).

| Node | Purpose | Ports / config gist |
|------|---------|---------------------|
| `agent` | Runs an LLM agent turn | Sub-ports `chat_model` / `memory` / `tool` / `output_parser`; config `prompt`, `model`, … — via `LlmProvider` |
| `tool_call` | Invokes one specific integration action | Config `slug`, `args` — via `ToolInvoker` |
| `http_request` | Outbound HTTP request | Config `method`, `url`, `headers`, `query`, `body` — via `HttpClient` |
| `code` | Runs sandboxed user code | Config `language` (`javascript`/`python`), `source` — via `CodeRunner` |
| `output_parser` | Parses/validates an agent's output into a structured shape | May use `LlmProvider` for auto-fixing; can nest as a sub-agent |
| `sub_workflow` | Runs another workflow as a nested sub-graph | Config `workflow_id`, `input` mapping |

All 11 node kinds plus the trigger are implemented and dispatched by the engine.
Per-node error handling (`on_error` stop/continue/route, `retry`, an `error`
port) and approval gating (`requires_approval`) are configured through the same
free-form `config`.

See [`docs/03-node-catalog.md`](../blob/main/docs/03-node-catalog.md) for the full
catalog, and [`docs/06-triggers.md`](../blob/main/docs/06-triggers.md) for trigger
kinds.

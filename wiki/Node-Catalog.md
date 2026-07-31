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

A workflow's typed **parameters** are not declared on the trigger. They live in
the graph's top-level `inputs` array, are validated before the run starts, and
are read from any node as `=inputs.<name>` — see
[Architecture → Workflow inputs](Architecture). The trigger payload stays at
`=run.trigger.<path>`.

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
| `sub_workflow` | Runs another workflow as a nested sub-graph | Config: exactly one of `workflow` (inline) / `workflow_id`; optional `inputs` map for the child's declared inputs |

The capability-backed integration nodes (`agent`, `tool_call`, `http_request`)
resolve `=` expressions anywhere in their config against the `{ item, items, run }`
scope before use, so their parameters can data-bind directly from upstream output
(e.g. `args: { "channel": "=item.channel" }`). Non-`=` values pass through as
literals.

All 11 node kinds plus the trigger are implemented and dispatched by the engine.
Per-node error handling (`on_error` stop/continue/route, `retry`, an `error`
port) and approval gating (`requires_approval`) are configured through the same
free-form `config`.

### Per-item fan-out

`agent`, `tool_call`, `http_request`, `memory`, and `sub_workflow` can map over
their input array instead of running once. Three config keys control it, and
they mean the same thing on every one of those kinds:

| Key | Values | Meaning |
|-----|--------|---------|
| `execution` | `once` \| `per_item` | Run once for the whole input array, or once per item. Defaults to `per_item` for `tool_call` / `http_request` / `memory`, and `once` for `agent` / `sub_workflow`. |
| `concurrency` | integer \| `"all"` | How many items run at a time: `1` (default) sequential, `n` at most n in flight, `0` or `"all"` unbounded. Clamped to 64. |
| `on_item_error` | `collect` \| `fail_fast` \| `skip` | What a failing item does to the batch. |

```jsonc
// one agent turn per topic, at most 8 concurrently
{ "id": "research", "kind": "agent", "name": "Research each",
  "config": {
    "execution": "per_item",
    "concurrency": 8,
    "agent_ref": "researcher",
    "prompt": "Research =item.name"
  } }
```

`sub_workflow` in `per_item` mode is the **multiplier**: one complete child run
per item, each seeded with just that item and resolving `workflow_id` against
it. The nesting-depth guard is per child run, so a fan-out widens a run without
deepening it — N siblings at depth d+1, never d+N.

Output items always come back in **input order** with `paired_item` set,
whatever the concurrency, so a fan-out never reorders data.

`on_item_error` defaults to `collect` when the node fans out (`concurrency`
other than `1`) and `fail_fast` when it runs sequentially. That split matters:
`tool_call`, `http_request`, and `memory` are `per_item` *by default*, so
collecting unconditionally would silently disable `on_error`, `retry`, and the
`error` port for the most ordinary nodes in the engine. Under `collect` a failed
item becomes `{ json: { error, failed: true } }` in its own slot, so the node
still emits one output per input and a downstream `condition` can branch on
`=item.json.failed`; under `skip` it is dropped, so the output array may be
shorter than the input.

These keys are rejected at validation time on a node that does not map over its
input — a fan-out knob that silently does nothing is worse than an error.

Each node kind's config keys and ports, along with the available trigger kinds,
are documented in the sections above.

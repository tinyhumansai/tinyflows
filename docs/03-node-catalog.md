# 03 — Node catalog

Every node has a [`NodeKind`](02-workflow-model.md). Kind-specific settings live
in the node's `config` (free-form JSON, validated per kind). Below: purpose,
config keys (indicative), ports, and implementation stage.

Legend for **Stage**: A2 = native control-flow, A3 = capability-backed. See the
[roadmap](08-roadmap.md).

## Trigger node (`trigger`)

The single entry node. Exactly one per workflow. Its firing mode is a
[`TriggerKind`](06-triggers.md) in config (`trigger_kind`). The host is
responsible for actually firing it (see [triggers](06-triggers.md)); tinyflows
treats the trigger as the graph's start node and injects the trigger payload as
the initial run state.

| `trigger_kind` | Fires when |
|----------------|-----------|
| `manual` | User clicks Run |
| `schedule` | Cron / interval elapses |
| `webhook` | Inbound HTTP request arrives |
| `app_event` | A connected-app event fires (dynamic per connected toolkit) |
| `form` | A form is submitted |
| `execute_by_workflow` | Another workflow invokes this one |
| `chat_message` | A chat message arrives (AI workflows) |
| `evaluation` | An evaluation/test run starts |
| `system` | A system event (workflow error, file change, …) |

Output port: `main` (the trigger payload).

## AI / agent nodes

### `agent`  *(Stage A3)*
Runs an LLM agent turn. Models a tool-using AI agent: besides its `main`
data flow, it accepts **sub-ports**:

| Sub-port | Wires in | Backed by |
|----------|----------|-----------|
| `chat_model` | the LLM to use | [`LlmProvider`](05-capability-traits.md) |
| `memory` | conversation/memory backend | host |
| `tool` (repeatable) | tools the agent may call | [`ToolInvoker`](05-capability-traits.md) |
| `output_parser` | structures/validates output | an `output_parser` node |

Config (indicative): `prompt`, `model`, `max_iterations`, `system`. Output: `main`.

### `output_parser`  *(Stage A3)*
Parses/validates an upstream agent's output into a structured shape (a structured
/ auto-fixing output parser). May itself use an
[`LlmProvider`](05-capability-traits.md) for auto-fixing. Can nest as a
sub-agent.

## Integration / effect nodes

### `tool_call`  *(Stage A3)*
Invokes **one specific** integration action deterministically (no LLM). Action
list and input schema come dynamically from the host — in OpenHuman, the curated
Composio catalog (see [security](07-security.md)). Config: `slug`, `args` (may
reference upstream state via expressions). Backed by
[`ToolInvoker`](05-capability-traits.md).

### `http_request`  *(Stage A3)*
Outbound HTTP request. Config: `method`, `url`, `headers`, `query`,
`body`, `timeout_secs`. Backed by [`HttpClient`](05-capability-traits.md); the
host applies an allowlist + network gating (see [security](07-security.md)). This
is how workflows reach arbitrary APIs (VirusTotal, Qdrant, urlscan, …).

### `code`  *(Stage A3)*
Runs sandboxed user code to transform data. Config: `language`
(`javascript` | `python`), `source`. Backed by
[`CodeRunner`](05-capability-traits.md), which the host sandboxes (out-of-process
+ OS jail). See [security](07-security.md).

### `sub_workflow`  *(Stage A3)*
Runs another workflow as a nested sub-graph and returns its output. Config:
`workflow_id`, `input` mapping.

## Control-flow nodes (native, no host capabilities)

### `condition`  *(Stage A2)*
Two-way IF. Config: a boolean expression over the run state. Output ports:
`true`, `false`.

### `switch`  *(Stage A2)*
Multi-way branch keyed by an expression result. Config: `expression` + a list of
`cases`. Output ports: one per case (+ optional `default`).

### `merge`  *(Stage A2)*
Fan-in barrier combining multiple named inputs (`Input 1` / `Input
2`). Config: `mode` (e.g. `append` / `by_key`). Waits for all wired inputs.

### `split_out`  *(Stage A2)*
Fan-out: emits one item per element of a list (split out / "clusters to
list"). Config: `path` (which list to iterate). Downstream nodes run per item.

### `transform`  *(Stage A2)*
Pure data transform / field mapping over the run state via an expression
(set / edit-fields style). Config: `set` (field → expression map). The expression
library is selected in A2 (`jaq` for JSON-native, or `minijinja` for templating —
see [decisions](11-decisions.md)).

## Config validation

Each kind validates its own `config` during compilation (missing required keys,
type mismatches, unknown ports). Structural validation (unique ids, one trigger,
edges reference existing nodes) is done first by
[`validate`](04-execution-engine.md).

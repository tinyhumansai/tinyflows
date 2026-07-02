# 13 — Data model & expressions

This is the deepest semantic decision in tinyflows: **how data flows between nodes**
and **how a node references upstream data**. It builds on the
[`serde_json::Value`](04-execution-engine.md) run state (decision D5) by defining
the *conventions* that live inside that state.

## Item-based data flow

Data on a connection is an **array of items**, not a single value — the model
common to mature workflow tools. Each item is:

```jsonc
{ "json": { /* the item's data */ }, "binary": { /* optional attachments */ } }
```

Consequences:

- **Nodes map over items.** A node typically runs its logic once per input item
  and returns an array of output items. A `tool_call` fed 10 items performs 10
  calls (subject to concurrency limits); a `transform` maps each item.
- **Fan-out / fan-in are item operations.** `split_out` turns one item containing
  a list into many items; `merge` combines item arrays from multiple inputs.
- **Empty arrays are valid** (a node can legitimately produce zero items, which
  short-circuits its downstream branch).

### Item pairing (linking)
Each output item records which input item produced it (a `paired_item` index).
This lets a later node "reach back" to the item that started a sub-chain — needed
for correlation after branches/merges. Pairing is best-effort: identity/map
nodes preserve it automatically; aggregating nodes may drop it.

## The run state layout

The tinyagents graph state (`serde_json::Value`) holds, at minimum, each node's
last output items keyed by node id, plus run metadata:

```jsonc
{
  "nodes": { "<node_id>": { "items": [ /* items */ ] } },
  "run":   { "id": "...", "trigger": { /* payload */ }, "started_at": "..." }
}
```

Nodes read their input by following incoming edges to the source node's `items`;
they write their own `items` back via the reducer. (Exact shape is finalized in
A1 alongside the reducer.)

## Expressions & data referencing

Any node config field may be a **static value** or an **expression** evaluated
against the run at execution time. Convention: a string beginning with `=` is an
expression (e.g. `"=upstream.json.email"`); everything else is a literal.

The evaluation context exposes:

| Binding | Meaning |
|---------|---------|
| `item` | the current input item's `json` |
| `items` | all input items |
| `nodes["Name"]` | a named upstream node's output items |
| `run` | run metadata + trigger payload |

The `transform` node uses the same engine for its `set` field-map. The concrete
expression language is the open decision **O1** ([decisions](11-decisions.md)):
`jaq` (JSON-native, jq-like) or `minijinja` (templating). Whichever is chosen is
used **both** for the `transform` node and for inline `=` field expressions, so
there is one language to learn. Expressions are **pure and sandboxed** (no I/O,
no host access) — side effects only happen in nodes.

## Why this matters

Item-based flow + expressions are what make real automations expressive: "for
each new row, look it up, branch on a field, and post a message." Deciding it now
keeps the `Node` I/O contract and the reducer stable; retrofitting an item model
later would be a breaking change to every node.

## Decision

Logged as **D13** in [decisions](11-decisions.md): adopt item-based data flow +
an `=`-prefixed expression convention over the `serde_json::Value` state. Marked
**Proposed** pending confirmation, since it shapes the `Node` I/O contract and the
reducer implemented in A1.

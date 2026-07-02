# 00 — Overview

## What tinyflows is

tinyflows is a **Rust-native workflow automation engine** in the spirit of modern
workflow automation platforms, but designed to be **agent-first** and
**embeddable**. A workflow is a
directed graph of typed nodes — a trigger, plus actions, control-flow, and AI
nodes — that runs against a user's connected accounts, data, and models.

Two authoring surfaces produce the *same* underlying graph:

- **Agent-first** — the user describes an automation in natural language and the
  host's agent drafts a `WorkflowGraph` for one-click save.
- **Visual canvas** — a node/edge editor (visual, canvas-style) for direct authoring.

Neither surface is privileged; both read and write the same
[`WorkflowGraph`](02-workflow-model.md).

## Goals

- **Full workflow-automation capability**: branch (IF), multi-way switch, merge/fan-in,
  parallel branches, loops, sub-workflows, and a rich node catalog — AI agents
  with model/memory/tool/output-parser sub-ports, integration actions, HTTP,
  sandboxed code, and data transforms.
- **Rust-native and optimized**: build on the existing `tinyagents` state-graph
  engine (durable, parallel, resumable) rather than re-implementing an executor.
- **Host-agnostic**: the crate never hard-codes a vendor. All outside-world
  effects go through [capability traits](05-capability-traits.md) the host
  implements. This keeps tinyflows a reusable open-source engine and confines
  host coupling to a thin adapter seam.
- **Safe by construction**: declarative definitions (no arbitrary embedded
  scripting in the model), sandboxed code execution, gated network access, and a
  trust model where an enabled, user-saved workflow authorizes its own
  pre-declared actions while treating trigger payloads as untrusted data.

## Non-goals

- **Not a hosted SaaS** — tinyflows is a library; scheduling, persistence, and
  background execution belong to the host.
- **Not a hand-maintained integration catalog** — rather than hundreds of bespoke,
  hand-built nodes, integration actions come dynamically from the host (e.g. OpenHuman's
  Composio catalog). tinyflows defines a small set of node *kinds*, not one node
  per third-party API.
- **Not an arbitrary code sandbox itself** — the `code` node delegates execution
  to a host-provided [`CodeRunner`](05-capability-traits.md) that is expected to
  sandbox it (see [security](07-security.md)).

## Prior art

Mainstream visual automation tools inform tinyflows' *product shape and node
vocabulary* only — **no third-party code is reused**, concepts only. Where those
tools are typically canvas-first and built on Node.js/Postgres/Redis, tinyflows is
agent-first-capable, Rust-native, and embeddable, with the durable graph executor
provided by `tinyagents`.

## Relationship to OpenHuman

OpenHuman is the first host. It depends on the published `tinyflows` crate and
adds an adapter seam (`src/openhuman/tinyflows/`) that implements the capability
traits with its inference stack, curated Composio tools, `HttpRequestTool`, and
sandboxed code runtimes, plus an `automations::` domain for persistence, RPC, and
the canvas/chat UI. See [OpenHuman integration](09-openhuman-integration.md).

## The three layers (at a glance)

```
OpenHuman (host)  →  tinyflows (this crate)  →  tinyagents (execution primitive)
```

Details in [architecture](01-architecture.md).

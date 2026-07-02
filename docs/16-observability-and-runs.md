# 16 — Observability & the run model

To debug and to power the canvas "inspect" overlay, a run must capture **what
happened at each node** — inputs, outputs, status, timing, errors. This doc
defines the run/execution data model and the observability hooks.

## The run model

A **Run** is one execution of a workflow:

| Field | Meaning |
|-------|---------|
| `id` | Unique run id |
| `workflow_id` | Which definition ran |
| `status` | `running` \| `completed` \| `failed` \| `interrupted` \| `cancelled` |
| `trigger` | The trigger payload that started it |
| `started_at` / `finished_at` | Timing |
| `steps` | Per-node [`ExecutionStep`](#execution-steps) records |

### Execution steps
One **ExecutionStep** per node activation:

| Field | Meaning |
|-------|---------|
| `node_id` | Which node |
| `status` | `success` \| `error` \| `skipped` \| `waiting` |
| `input` | Input items the node received |
| `output` | Output items it produced (or the error item) |
| `port` | Which output port fired (for branch/switch) |
| `started_at` / `duration_ms` | Timing |
| `attempt` | Retry attempt number |

This is exactly the data the canvas renders when a user clicks a node to inspect
what flowed through it, and what a run-history list summarizes.

## Where runs live

tinyflows **emits** run/step data; the **host persists** it. The crate does not
own a database. Two hooks:

- **Tracing** — the engine emits structured `tracing` spans per run and per node
  (correlation id = run id), so any host tracing subscriber captures them.
- **`RunObserver` hook** *(added in A4)* — an optional trait the host implements
  to receive `on_run_start` / `on_step_finish` / `on_run_finish` callbacks with
  the records above, for durable storage and live UI updates.

In OpenHuman these map onto the existing `workflow_runs` ledger + `run_ledger`
tables (see [OpenHuman integration](09-openhuman-integration.md)); live UI updates
ride the socket layer.

## Checkpoints vs. observability

Two distinct concerns:

- **Checkpointing** (durability/resume) is tinyagents' `Checkpointer`, storing
  graph state at super-step boundaries so a run can resume — see
  [execution engine](04-execution-engine.md).
- **Observability** (this doc) is the human-facing record of what each node did.

They overlap in data but serve different purposes; keep both.

## Redaction

Step input/output may contain sensitive data. The `RunObserver`/tracing layer
supports host-side redaction hooks so secrets and PII aren't persisted or logged
verbatim (the host decides policy).

## Decision

Logged as **D16** in [decisions](11-decisions.md): a `Run` / `ExecutionStep`
record model, emitted via `tracing` spans + an optional `RunObserver` hook; the
host persists and renders it. Marked **Proposed** (hook lands A4).

# 14 — Error handling

Real automations must degrade gracefully: one failing item or one flaky API call
shouldn't necessarily kill the whole run. tinyflows defines a per-node error
policy, an error output port, retries, and a failure-trigger hook.

## Per-node error policy

Every node supports an `on_error` config (default `stop`):

| `on_error` | Behavior |
|------------|----------|
| `stop` | The node's error fails the run (default). |
| `continue` | The failing item is skipped or replaced with an error item; the node keeps processing remaining items and the run proceeds. |
| `route` | The node emits failing items on its **`error`** output port instead of `main`, so the graph can handle them explicitly. |

`continue`/`route` make the failure *data* (an error item: `{ "error": { "message", "kind", "node" } }`) rather than a run-ending event.

## Error output port

Nodes may declare an `error` port in addition to `main` (and any branch ports).
With `on_error: route`, edges from the `error` port feed a recovery sub-graph
(e.g. log to a sheet, notify, retry via a different path). This mirrors the IF/
switch branching model — the error path is just another port.

## Retries

Per-node `retry` config:

```jsonc
{ "retry": { "max_attempts": 3, "backoff_ms": 500, "backoff": "exponential" } }
```

Applied to transient failures (network/capability errors) before `on_error` takes
effect. Backed by the engine's per-node execution wrapper (stage A4). Bounded so
retries can't run unbounded.

## Failure as a trigger (error workflows)

A separate workflow can react to *another* workflow's failure via the `system`
[trigger kind](06-triggers.md) ("on workflow error"). The host bridges the
failure event (workflow id, failing node, error) to the error-handling workflow's
run input. This keeps recovery logic reusable and out of the happy-path graph.

## Run-level semantics

- A run's status is `failed` if an unhandled error reaches a `stop` node.
- Partial outputs from completed nodes are preserved in the
  [run record](16-observability-and-runs.md) for debugging.
- Timeouts (per-node and per-run) surface as errors and follow the same
  `on_error` policy.

## Division of responsibility

| Concern | tinyflows | Host |
|---------|-----------|------|
| `on_error` semantics, `error` port | ✅ engine | — |
| Retry/backoff wrapper | ✅ engine (A4) | — |
| Classifying transient vs permanent capability errors | asks the capability | ✅ capability impls signal error kind |
| Delivering the failure event to an error-workflow | expects it | ✅ host bridges (`system` trigger) |

## Decision

Logged as **D14** in [decisions](11-decisions.md): per-node `on_error`
(stop/continue/route) + `error` port + `retry` config + `system`-trigger error
workflows. Marked **Proposed** (adds fields to `Node` config; implemented A4).

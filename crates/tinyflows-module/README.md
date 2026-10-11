# TinyFlows module

The compiled TinyBus module serves Authoring and Schedule objects under the
shared `ai.tinyhumans.tinyflows.Authoring` bus name. Hosts import the
`tinyflows-bus` contract and call the compiled module. The module manifest
advertises each interface's exact member set.

Schedule owns claims, bounded dispatch, retries, timing, recurrence,
completion, validation, and cron persistence. The host provides context-bound
execution, delivery and direct flow-trigger callbacks through ScheduleHost.
`StartScheduler.storage` selects module-owned persistence. Omitting it retains
the original ScheduleHost storage callbacks. Callback stores implement
DueJobsExcluding(now, excluded_ids), filtering identities before their batch
limit when a running job defers its occurrence. Stores without this operation
fail that poll rather than silently starve later due jobs. The runtime retains
the original batch size and leaves deferred occurrences unchanged.

RecordRun callbacks receive job_created_at, captured from the dispatched job.
Match that creation before appending history; an ID alone can refer to a
replacement flow job. The module-owned backends perform this check. Completion
follows ClaimSlot, which already disables one-shots durably; an explicit
re-enable during execution survives completion and automatic deletion.

`ScheduleStore` accepts one StoreRequest: `{storage, operation}`. Storage is
`{backend: {kind: "sqlite", path: "<scoped jobs.db>"}, max_run_history: 50,
max_tasks: 64}` or `{backend: {kind: "documents", callback: {destination,
object_path}}, max_run_history: 50, max_tasks: 64}`. The host chooses scoped
paths and ports. The module creates/upgrades SQLite or calls CronDocuments;
limits, migrations, validation, truncation, pruning and CAS stay module-owned.

Each operation has an `op` field and these arguments. Responses preserve
existing job/run JSON; mutation-only operations return null.

| op | Arguments | Response |
| --- | --- | --- |
| add | expression, command | job |
| add_shell | name (optional), schedule, command | job |
| add_agent | spec (AgentJobSpec JSON, including enabled and session_target) | job |
| add_flow | flow_id, schedule | job (idempotent per flow) |
| find_flow | flow_id | job or null |
| list | none | jobs |
| get | job_id | job |
| remove | job_id | null |
| clear | none | removed count |
| dedup | none | removed count |
| due | now (RFC3339) | jobs |
| update | job_id, patch (CronJobPatch JSON) | job |
| record_last_run | job_id, finished_at (RFC3339), success, output | null |
| reschedule | job, success, output | null |
| record_run | job_id, started_at, finished_at (RFC3339), status, output (optional), duration_ms, delivery_status (optional) | null |
| delete_queued_runs | job_id | removed count |
| list_runs | job_id, limit | runs |

A document callback serves `ai.tinyhumans.tinyflows.ScheduleDocuments.Document`
with a JSON request and returns `{value: JSON}` or `{error: {kind, message}}`.
The error kinds are conflict, already_exists, not_found, invalid_input,
serialization, unavailable or backend. Preserve conflict separately so module
CAS loops can retry. Core-port values use tinystoragedrivers-core v0.5 serde.

The module first requests capabilities and requires a typed
`{value: {transactions: bool}}` reply; missing or invalid capability reports
fail initialization. Transaction support is advertised only when the scoped
backend provides it.

The request operations are capabilities, atomic_batch (ops), ensure (spec),
get (collection/id), put
(collection/id/doc/precondition), delete (collection/id/precondition), query
(collection/query), count (collection/filter) and delete_where
(collection/filter). Only cron_jobs, cron_runs, cron_counters and legacy
cron_job_policies are used. The host mechanically forwards these to its scoped
document port, without linking TinyFlows storage implementations. Atomic batches
use tinystoragedrivers-core WriteOp and WriteResult JSON and must be forwarded
as one backend transaction. Every collection in the batch is checked before
forwarding; unsupported transactions fail instead of falling back to individual
writes.

Configured scheduler storage applies claims/completions through upstream
transaction/CAS methods, preserving edited schedules and newer occurrences.
Legacy policy rows migrate into jobs before deletion. Stop joins the scheduler
and aborts/drains owned jobs before releasing claims.

Tests run the actual cdylib, verify both exact manifest method sets, exercise
Authoring and Schedule calls, persist through SQLite and a document callback,
and run a stored flow through direct trigger/delivery/history persistence:

```bash
bash scripts/verify-module-artifact.sh
```

# tinyflows-drivers

`tinyflows` storage on
[tinystoragedrivers](https://github.com/tinyhumansai/tinystoragedrivers)
document ports, and nothing else: this crate depends on TinyFlows-owned
`tinyflows-storage` helpers built on the public `tinystoragedrivers-api` and
`tinystoragedrivers-types` contracts, so no SQLite, MongoDB or file code lives
in tinyflows. A host that has
opened a storage backend (SQLite on a desktop, MongoDB in the cloud, memory in
tests) gets the engine's durable pieces on it, scoped per tenant by the
document handle it passes in.

See [Why a separate crate](#why-a-separate-crate) for why this is not a feature
of `tinyflows` itself.

| Type | Implements | Layout |
| --- | --- | --- |
| `DriverStateStore` | `tinyflows::caps::StateStore` | one `tsd.kv` entry per key (owner `flows`, namespace `state`) |
| `DriverCheckpointer<State>` | `tinyflows::graph::Checkpointer<State>` | `graph.checkpoints`, `graph.threads`, `graph.writes` (namespace `flows`; declared by `tinyagents-graph`) |
| `schedule::CronDocuments` | the cron job and run store | `cron.jobs`, `cron.runs`, `cron.admission`; run numbers in `tsd.counters`, claims in `tsd.leases` |
| `catalog::FlowCatalogDocuments` | saved flows, revisions, runs, suggestions, drafts | `flows.*` |
| `store::DriverWorkflowStore` | `tinyflows::store::WorkflowStore` | `flows.workflows`, `flows.workflow_*`; the decision lock in `tsd.leases` |

```rust
use std::sync::Arc;
use tinyflows_drivers::{DriverCheckpointer, DriverStateStore};

let docs = Arc::clone(scoped_storage.documents());
let state = Arc::new(DriverStateStore::new(Arc::clone(&docs)));
let checkpointer = DriverCheckpointer::<serde_json::Value>::new(docs);
```

## Features

| Feature | Default | Brings |
| --- | --- | --- |
| `engine` | yes | `DriverStateStore`, `DriverCheckpointer` (needs `tinyflows`) |
| `catalog` | no | `catalog::FlowCatalogDocuments`, `catalog::FlowStateDocuments` |
| `schedule` | no | `schedule::CronDocuments`; needs only `tinyflows-schedule`, not the engine |
| `store` | no | `store::DriverWorkflowStore` (turns on `tinyflows/store`) |
| `test-fixtures` | no | the catalog's two fixture-only writers |

A host that runs scheduled jobs but no flows builds with
`default-features = false, features = ["schedule"]` and pulls neither the
engine nor its expression stack.

## Storage schema v2

Everything here stores on the uniform schema of `tinystoragedrivers` v2.0.0.
`storage_schema()` (owner `flows`) and `cron_storage_schema()` (owner `cron`)
declare the collections and carry the v1 -> v2 steps; each owning crate runs
its schema migration against the host-bound document port, then calls
`import_legacy_kv` and `import_legacy_cron_counters` once per scope (a document
step cannot write `tsd.*`). All owners share `tsd.migrations` with distinct
owner/item/step progress keys. The graph checkpointer reads and writes the
`graph.*` documents of `tinyagents-graph` under namespace `flows`. With the
`store` feature `storage_schema()` also declares the workflow store
(`flows.workflows`, `flows.workflow_*`). `tinyflows-adaptive` (feature
`storage-drivers`) owns a separate schema for the existing `flows.adaptive_*`
collections and runs its migration on the same bound document port.
`tinyflows_adaptive::drivers::import_legacy_adaptive_counters` moves its id
sequences into `tsd.counters`.

## The flow catalog (`catalog`)

`FlowCatalogDocuments` is the catalog and the authoring drafts: one `async fn`
per operation (`update_flow_graph` reports the shared
`tinyflows_catalog::store::FlowUpdateError`). It replaces the retired
`tinyflows-sqlite` `flows.db` and draft files.

```rust
use std::sync::Arc;
use tinyflows_drivers::catalog::{FlowCatalogDocuments, FlowStateDocuments};

let catalog = FlowCatalogDocuments::new(Arc::clone(scoped_storage.documents()));
let flow = catalog.create_flow("Digest".into(), graph, true, false).await?;
let state = Arc::new(FlowStateDocuments::open(catalog.clone(), flow.id.clone()).await?);
```

| Collection | One document per |
| --- | --- |
| `flows.definitions` | flow |
| `flows.revisions` | superseded graph (newest 20 kept) |
| `flows.runs` | run, with its steps |
| `tsd.kv` (owner `flows`, namespace `kv:<namespace>`) | `(namespace, key)` of flow state |
| `flows.suggestions` | discovery suggestion |
| `flows.drafts` | authoring draft |

- Every guarded SQL `UPDATE … WHERE status = …` (finish, resume, interrupt,
  TTL expiry) is a compare-and-swap on the run, so a transition happens at
  most once across processes sharing one database.
- A run's steps live on the run document (`steps`, a JSON array), so a
  run settles with its step list in one compare-and-swap, and a step written
  by a parallel branch retries on the winner's list rather than losing to it
  (what SQLite's `BEGIN IMMEDIATE`, R-m1, protected).
- `update_flow_graph` writes the prior graph's revision *pending*, swaps the
  graph naming it (`last_revision_id`), then confirms it. A revision shows
  only once confirmed or named, so a lost race or a crash never surfaces a
  revision for an update that did not happen; `updated_at` advances strictly
  on every write, so it is a sound concurrency token and a total order.
- Removing a flow removes its runs and revisions first and the definition
  last (a failure part-way is finished by calling it again). A run or
  revision written concurrently can still land after the last sweep, or its
  writer be cancelled before undoing it, so each definition carries an
  `incarnation` id and every run and revision records the one it was written
  under: readers show only dependents of a flow that still exists with that
  incarnation, never an orphan (not even after a flow with the same id is
  created again), and reclaim the orphans they meet. A run or revision with
  no recorded incarnation (older data, or an older process during a rolling
  upgrade) belongs to whatever flow exists under its id, so it is never
  hidden or reclaimed by mistake.
- Pruning deletes a revision only at the version it read, re-checking an
  abandoned one's age and pending state first; an update whose in-flight
  revision was pruned anyway writes it again after its swap.
- Graphs, steps, drafts and state values are real JSON values, optional
  fields are omitted rather than `null`, and ordering uses epoch-millisecond
  `*_ms` fields with the id as the tie-break.

`FlowStateDocuments::open` proves the catalog collections can be opened before
returning a handle; a storage failure propagates instead of selecting an
unscoped or process-local fallback. `FlowStateDocuments` is one flow's namespaced state over the same `tsd.kv`
documents as `kv_get` / `kv_set` / `kv_delete`. It is the engine's
`StateStore` and the host's synchronous `DedupKv`; the latter runs on the
storage crates' `Blocking` bridge (one per process, or the host's own via
`with_bridge`), so it is safe inside any runtime. `DriverStateStore` is
unchanged: un-namespaced, for a host that gives each run its own collection.

## The workflow store (`store`)

`DriverWorkflowStore` implements `tinyflows::store::WorkflowStore` (the engine's
authoring layer: saved workflows, their undo history, run records, the notes
journal and proposed changes) over a document handle. The trait is synchronous,
so each call runs on the storage crates' `Blocking` bridge, which is safe from
inside any runtime.

| Collection | One document per |
| --- | --- |
| `flows.workflows` | workflow (`Key::of([id])`) |
| `flows.workflow_revisions` | superseded copy (newest 20 per workflow kept) |
| `flows.workflow_runs` | run record |
| `flows.workflow_notes` | workflow's whole journal (capped at 100 notes) |
| `flows.workflow_proposals` | proposal |
| `flows.workflow_quarantine` | journal that could not be parsed, set aside |

The decision lock is a `tsd.leases` lease (owner `flows`, key
`workflow_decision/<id>`). Bodies are bare snake_case objects with `*_ms`
instants; the `workflow_` prefix keeps these records apart from the catalog's
`flows.definitions` / `flows.revisions` / `flows.runs`, which have a different
shape. The v1 `wf_*` collections move with the steps in `storage_schema()`.

- Every definition write is a compare-and-swap on the version it read, and the
  revision of what it replaces is written first and taken back if the swap
  loses, so concurrent saves, deletes and fingerprint-guarded saves from any
  number of processes or store instances never lose a revision or a write.
- `lock_proposal_decision` is a lease document (claimed with a write-if-absent,
  or a swap over an expired one), so it excludes across processes. The lease
  lasts `store::LOCK_LEASE` (two minutes).
- `save_proposal_if_fingerprint` fences the definition in the same atomic batch
  on a backend with transactions.
- A document the host policy now refuses, or that no longer decodes, is hidden
  from `get` and `list` (and logged) rather than served.

## Cron store

`CronDocuments` is the cron job and run-history store (it replaces the retired
`tinyflows-sqlite` `jobs.db`), as `async` methods:

```rust
use tinyflows_drivers::schedule::CronDocuments;

let cron = CronDocuments::new(docs).with_limits(max_run_history, max_tasks);
let job = cron.add_job("0 9 * * *", "echo hi").await?;
for due in cron.due_jobs(chrono::Utc::now()).await? { /* run it */ }
```

Across processes sharing one database:

- every job update is compare-and-swap, so concurrent writers never lose a
  field;
- `reschedule_after_run` advances `next_run` only from the occurrence the
  caller fired, so it never advances twice or overwrites an edited schedule;
- a flow's schedule job has a deterministic id written only if absent, so
  registering it is idempotent, and its `command` (the flow it fires) cannot
  be patched to another flow in either store (`tinyflows_schedule::check_patch`);
- run numbers come from a compare-and-swap counter.

`due_jobs` is a read, so two schedulers can both run the same due job (as
with two processes on one SQLite file); it orders by `next_run_ms`. Recording a
run and pruning history are separate writes, and so are removing a job and
its runs; `sweep_orphan_runs` (run by `clear_all_jobs`) collects runs whose
job is gone or was re-created.

### Occurrence claims and an operator cap

On a transaction-capable backend, install an optional scope-wide cap using
`cron.set_max_active_jobs(Some(max_enabled)).await?`. It counts enabled jobs
of all types; disabled jobs do not consume slots. Re-enabling, creating,
removing, one-shot completion, clearing and deduplication update admission
in the same transaction as the job. A cap below the current count is refused.
`None` removes the cap. Every upgraded handle observes this persisted policy,
including handles created before installation.

For multiple schedulers, treat `due_jobs` as discovery and claim each result
before dispatch:

```rust
for due in cron.due_jobs(chrono::Utc::now()).await? {
    let Some(claim) = cron
        .claim_due_job(&due.id, worker_id, chrono::Utc::now(), chrono::Duration::minutes(2))
        .await?
    else {
        continue;
    };
    // Dispatch claim.job(), preserving this receipt through the whole turn.
    // Renew before expiry during long turns; stop if renewal returns false.
    cron.complete_claim(
        &claim, chrono::Utc::now(), true, "completed", None,
    ).await?;
}
```

A claim carries a fresh token, job incarnation and the exact scheduled instant.
Only its unexpired owner can renew, release, or complete it. An edit or
cancellation invalidates the claim; deleting and re-registering the same flow
creates a new incarnation. Stale completion changes neither outcome nor history.
Successful completion stores the run and schedule advance atomically, disabling
one-shots while keeping their history. For `delete_after_run`, call
`remove_completed_claim(&claim)` after successful completion. This fences the
completion token and incarnation; a later acquisition or edit invalidates the
receipt. Never use `remove_job(id)` for this follow-up, since the id may already
belong to a replacement. History pruning remains a subsequent operation.

Lease time is supplied by the host. Replicas need clocks consistent within the
lease tolerance. Recovery after expiry permits another execution of the same
occurrence. A crash after sending to an external provider but before committing
completion can repeat that send; this API does not guarantee exactly-once HTTP
delivery.

Transactional backends maintain an authoritative membership ledger even before
a cap is installed, which fences an unlimited write already in flight when
policy changes. Initialization imports preexisting rows with their versions
fenced. Policy installation reconciles the authoritative rows again, fencing
both those rows and the ledger so pre-adoption legacy writes are counted.
All writers must run upgraded code; old binaries and direct document
writes bypass this protocol and must be stopped before policy adoption.
Every transactional job mutation, including lease renewal and completion,
rewrites this scope-wide ledger. These operations contend on one document and
serialize the membership map, so this design suits small, bounded job scopes.
Retries stop after 32 conflicts and return an error; hosts must treat renewal
errors as uncertain ownership and stop dispatch until ownership is confirmed.
Initialization and policy installation also reconcile all jobs in one
transaction and must fit the backend's transaction size and lifetime limits.
Large scopes or high write rates need a different admission protocol; no
throughput bound is guaranteed by this API.

Nontransactional backends retain unlimited legacy CRUD only when no ledger
exists; policy installation and occurrence claims are refused. If a ledger
already exists, mutations fail closed. No local fallback is used.

## Checkpointer guarantees

- One document per checkpoint with a per-thread `seq` advanced by
  compare-and-swap, so listing is insertion order and a re-used checkpoint id
  resolves to its latest write, as in the SQLite and file backends.
- `get_scoped` is one indexed `(thread, namespace, seq)` query, so a parent
  run and its subgraphs never load each other's checkpoints, even when ids
  repeat.
- `state_history` reads the namespace's checkpoints and pending writes in two
  queries and walks the lineage in memory, rather than two round trips per
  ancestor.
- Pending writes merge with `merge_writes` under compare-and-swap.
- Ids are `Key::of` keys: parts are escaped and joined with `/`, and a key
  over 400 bytes becomes `~` plus the base32 SHA-256 of the escaped key.

## Why a separate crate

`tinystoragedrivers` is a git dependency: OpenHuman vendors it once (through
TinyAgents) and points the URL at that copy with
`[patch."https://github.com/tinyhumansai/tinystoragedrivers"]`, so there is
exactly one `DocumentStore` trait in its graph. The published `tinyflows`
package cannot carry a git dependency, so these adapters live here
(`publish = false`). The engine stays dependency-free and keeps its MSRV; this
crate needs Rust 1.89, the storage crates' MSRV.

The split is a recorded decision, with what would end it, in
[`docs/storage-crate-split.md`](../../docs/storage-crate-split.md).

## Moving off the retired stores

`tinyflows-sqlite` (`flows.db`, `jobs.db`, `checkpoints.db`, JSON drafts) and the
engine's `FileWorkflowStore`, `FileStateStore` and `FileCheckpointer` are gone;
this crate is the only storage tinyflows has. To bring a host's existing data
across, every store has write-if-absent import methods
(`FlowCatalogDocuments::import_flow` / `import_run` / ..., `CronDocuments::import_job`
/ `import_run`, `DriverWorkflowStore::import_workflow` / ...), and the unpublished
The previous one-time file/SQLite importer was retired after the v0.8.9
transition window. The host owns any remaining legacy file discovery or
compatibility policy; drivers expose document import methods but never read
paths or open files.

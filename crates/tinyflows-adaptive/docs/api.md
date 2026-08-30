# tinyflows-adaptive · API reference for hosts

How to embed the crate, ordered by what you do: implement the seams, construct
the handles, drive the loop, read the results. Signatures are the real ones;
for full detail every public item carries rustdoc — `cargo doc -p
tinyflows-adaptive --open`. The runnable companion is
[`examples/service.rs`](../examples/service.rs).

```toml
[dependencies]
tinyflows-adaptive = "0.1"                       # sqlite ledger/vault on by default
# mongo-only service:
tinyflows-adaptive = { version = "0.1", default-features = false, features = ["mongo"] }
```

Modules: `contracts` · `driver` · `execute` · `intake` · `closing` · `ledger` ·
`workflows` · `inventory` · `promotion` · `recall` · `reuse` · `host`.

---

## 1 · The skeleton

```rust
use std::sync::Arc;
use tinyflows::caps::Capabilities;
use tinyflows::store::WorkflowStore;
use tinyflows_adaptive::contracts::{Budget, Goal};
use tinyflows_adaptive::driver::Loop;
use tinyflows_adaptive::execute::Remote;
use tinyflows_adaptive::host::HostFacts;
use tinyflows_adaptive::ledger::EpisodeStatus;
use tinyflows_adaptive::workflows::Snapshot;

// once, at boot
let ledger_root = MongoLedger::connect(&uri, "adaptive").await?;
let vault_root  = MongoVault::connect(&uri, "adaptive").await?;
let caps = Capabilities { llm: Arc::new(MyTieredClient::new(cfg)), /* … */ };

// per request (handles are free)
let ledger = ledger_root.for_tenant(&user_id);
let vault  = vault_root.for_tenant(&user_id);

// per goal run
let snapshot = Snapshot::load(&vault, policy.clone()).await?;
let store: Arc<dyn WorkflowStore> = Arc::new(snapshot.clone());
let runner = Remote { relay: &my_relay, attempt_id: episode_id.clone() };

let engine = Loop {
    ledger: &ledger, store: &store, caps: &caps,
    facts: &facts, runner: &runner, clock: &my_clock,
    budget: Budget::default(), conn: Some(&tenant_credential_ref),
};
let finished = engine.run(&episode_id, &Goal::new(prompt)).await?;

// the success gate — a device only ever receives graphs from satisfied runs
if finished.status == EpisodeStatus::Satisfied && snapshot.pending() > 0 {
    snapshot.flush(&vault).await?;
}
```

---

## 2 · Traits you implement

### `LlmProvider` (from `tinyflows::caps`) — required

```rust
async fn complete(&self, request: Value, conn: Option<&str>) -> tinyflows::error::Result<Value>;
```

Every request the loop sends has this shape — `tier` is which job is asking,
`conn` is the opaque credential reference off the `Loop`:

```json
{ "tier": "judge",
  "messages": [ {"role":"system","content":"…"}, {"role":"user","content":"…"} ],
  "response_format": { "type": "json_object" } }
```

Route `tier` → model in your config (select cheap, judge strong). The reply may
be a bare JSON object, an OpenAI-style envelope (`choices[0].message.content`),
or prose around an object — all three are parsed. What each tier must contain:

| tier | expected reply |
|---|---|
| `select` | `{"workflow_id": str \| null, "why": str, "inputs": {name: value}}` — null declines; an unknown id reads as declining |
| `author` | `{"graph": <WorkflowGraph>, "why": str, "inputs": {name: value}}` — the graph is validated before it is accepted |
| `judge` | `{"satisfied": bool, "blocker": str, "gap": str, "attributed_to": str, "advanced": bool}` — blocker ∈ `goal_not_met · unverified · missing_evidence · needs_input · external_wait`; unrecognised coerces to `goal_not_met` |
| `consolidate` | `{"lessons": [{"kind","trigger","mechanism","claim","evidence":[row numbers]}], "corroborate": [lesson ids]}` — kind ∈ `strategy · constraint · failure_mode · calibration`; a lesson with no cited rows is dropped |
| `repair` | `{"ops": [<GraphOp>…], "why": str}` — empty/absent ops declines; `rename_node` is refused |
| `generalise` | `{"name": str, "description": str, "reusable": bool}` — prose only; `reusable: false` or an empty description declines |

### `Relay` (`execute`) — required for remote execution

```rust
async fn dispatch(&self, request: &RunRequest) -> Result<RunReport, String>;
```

Serialize, send, correlate the reply, apply a deadline; `Err(reason)` on
timeout or no-device — `Remote` turns it into a judgeable attempt, never a
crash. Mint your own unique wire id per dispatch (attempts within an episode
share `attempt_id`). Reference implementation: `ChannelRelay` in the example.

### `Workspace` (`execute`) — device side, both methods default to empty

```rust
async fn mark(&self) -> String;                     // baseline before the run
async fn changed_since(&self, mark: &str) -> String; // prose diff after
```

`Unobserved` is the honest no-op. What this returns is the judge's third
evidence source; empty means "nothing reported", never "nothing happened".

### `Clock` (`driver`) — required, one method

```rust
fn now(&self) -> String;   // RFC 3339; opaque to the crate, drives tests frozen
```

### `Ledger` (`ledger`) / `Vault` (`workflows`) — only for a custom backend

Three of each ship (`memory`, `sqlite`, `mongo`). A fourth implementation runs
the public conformance suites: `ledger::conformance::{run_all, run_tenants,
run_lineage, run_episodes, run_transcripts}` and
`workflows::conformance::{run_all, run_tenants}`.

### `HostPolicy` (from `tinyflows::store`) — judgement only the host can make

`check_graph(id, &graph)` vetoes a graph naming a harness/slug this deployment
lacks. A permissive default impl is two lines.

---

## 3 · Configuration — every knob in one place

**One storage setting drives both halves** (`storage::Config::parse` +
`Storage::open`), and `Storage::for_tenant` scopes ledger *and* vault in one
call — the two-handle scoping mistake cannot be made:

```rust
let storage = Storage::open(&Config::parse(&cfg.storage)?).await?;   // once, at boot
let tenant  = storage.for_tenant(&user_id);                          // per request
// tenant.ledger() → &impl Ledger      tenant.vault() → &impl Vault
```

| `storage` value | meaning |
|---|---|
| `memory` / `:memory:` | forgets on restart — must be asked for by name, never a fallback |
| `adaptive.db` or any path, `sqlite:<path>` | one SQLite file holding ledger **and** vault |
| `mongodb://host:27017/adaptive` | one Mongo database, both halves; db name from the URI path, default `tinyflows_adaptive` |

A URI for a backend the build lacks fails **at parse time**, naming the missing
feature.

What a service configures, and who consumes it:

| setting | consumed by | values / default |
|---|---|---|
| storage string | `storage::Config::parse`, or `Config::from_env()` / `Storage::from_env()` reading **`TINYFLOWS_ADAPTIVE_STORAGE`** | table above; unset = boot error naming the variable, never a default |
| `TINYFLOWS_ADAPTIVE_DB` env | `SqliteLedger::from_env_or` / `at_default_location` | overrides the sqlite path without a rebuild |
| `Budget { attempts, min_attempts, stall_limit }` | the loop, per `Loop` (per tenant if you like) | `12 / 3 / 2` |
| `conn` | passed verbatim to your `LlmProvider` | opaque tenant credential *reference*, never a secret |
| tier → model map | **your** `LlmProvider`, off the request's `tier` | e.g. select→flash, author/judge→strong, consolidate→mid |
| relay deadline | **your** `Relay` | example uses 30 s; size to your longest workflow |
| `HostFacts` | authoring prompt + post-author check | 15 fields describing the executing machine; `unknown()` forbids nothing |
| `HostPolicy` | store saves + authored graphs | your veto for harnesses/slugs this deployment lacks |
| Cargo features | build | `default = ["sqlite"]`; `mongo`; `default-features = false` for memory-only |

**Deliberately not configurable** (behaviour, not policy): `MIN_TRIALS` = 3
runs before a variant can take a family's slot; `RECALL_LIMIT` = all lessons in
scope; `RECORD_BUDGET`/`PROMPT_BUDGET` = 256 KiB / 4 KiB per node.

## 3b · Storage construction (by hand)

### Ledger

| backend | construct |
|---|---|
| memory (always compiled; forgets) | `MemoryLedger::new()` |
| sqlite (default feature) | `SqliteLedger::open(path)` · `::in_memory()` · `::from_env_or(fallback)` · `::at_default_location()` |
| mongo (feature `mongo`) | `MongoLedger::connect(uri, db).await` · `::with_database(db)` |

All three: `.for_tenant(scope)` → a cheap scoped handle sharing the
connection. `SqliteLedger::open` creates the parent directory; env var
`TINYFLOWS_ADAPTIVE_DB` overrides the path in `from_env_or` /
`at_default_location`. The ledger and the sqlite vault may share one file.

**The scoping rule everywhere**: writes go to the handle's bucket; reads
return the handle's bucket **plus global** (an unscoped handle's bucket *is*
global). `promote`/`save_episode` stamp the handle's scope and ignore the
argument's.

### Workflows

```rust
// any backend → the sync WorkflowStore the loop needs
let snapshot = Snapshot::load(&vault, policy).await?;   // one async read
let store: Arc<dyn WorkflowStore> = Arc::new(snapshot.clone());
// … loop runs; save() buffers in memory, visible to the next attempt at once …
snapshot.pending();              // how many writes wait
snapshot.flush(&vault).await?;   // only what changed goes back
```

Composing catalogues (`workflows::compat`):

```rust
StoreVault::new(any_workflow_store)              // any WorkflowStore as a Vault
Layered::new(vec![("device".into(), theirs)], ours)   // read many, write one
    .degrading(Arc::new(|layer, why| warn!(…)))  // skip an unreachable read-only
                                                 // layer — handler is mandatory
```

Reads are the union, later layers shadow by id, writes/deletes reach only the
writable layer, and the writable layer failing is always fatal.

---

## 4 · Driving the loop (`driver::Loop`)

```rust
pub struct Loop<'a> {
    pub ledger: &'a dyn Ledger,
    pub store:  &'a Arc<dyn WorkflowStore>,
    pub caps:   &'a Capabilities,
    pub facts:  &'a HostFacts,
    pub runner: &'a dyn Runner,        // Local { caps, workspace } | Remote { relay, attempt_id }
    pub clock:  &'a dyn Clock,
    pub budget: Budget,                // default: 12 attempts, min 3, stall 2
    pub conn:   Option<&'a str>,
}
```

`Loop` is a bag of borrows — `Send + Sync`, no per-episode state, build one per
goal run or share one; any replica can pick up any episode.

| method | returns | notes |
|---|---|---|
| `start(episode, goal)` | `Episode` | idempotent; resumes an existing record |
| `attempt(episode, goal)` | `Closed { verdict, row_id, next, stalled }` | one pass: decide → run → judge → record → repair-if-suspect |
| `run(episode, goal)` | `Finished { status, attempts, verdict, lessons }` | drives to `Satisfied`/`StoodDown`; consolidates once at the end |
| `unfinished()` | `Vec<Episode>` | the boot recovery list for this tenant |

Lower-level building blocks (same behaviour the driver composes):
`intake::decide` → `Attempt`, `execute::run_attempt`/`serve` → `Ran`/`RunReport`,
`closing::{close, judge, consolidate, repair, keep}`.

---

## 5 · Reading back

| read | signature | for |
|---|---|---|
| `inventory::shelf(&store, &ledger)` | `Vec<Listing { id, name, description, node_count, enabled, score, standing, parent, learned }>` | a screen/audit — hides nothing, decides nothing |
| `ledger.rows(episode)` | `Vec<LedgerRow>` | one episode's attempt trail |
| `ledger.steps(row_id)` | `Vec<StepRecord>` | one attempt's per-node transcript |
| `ledger.episodes(running_only, Page)` | `Vec<Episode>` | listing; `Page { limit, offset }`, `Page::ALL`, `Page::first(n)` |
| `ledger.lessons(kind)` / `evidence(lesson_id)` | lessons + the rows behind one | the knowledge plane |
| `ledger.lineage(id)` / `workflow_score(id)` | family root-first / `Score { applied, helped }` | families and evidence |
| `promotion::{champion, standing}` | which family member is offered, and why | `MIN_TRIALS = 3` |
| `recall::{retrieve, render_history, render_lessons}` | what a planner is shown | default `RECALL_LIMIT` = everything in scope |
| `reuse::{baked_in, shape_id}` | pasted-input check / content-derived id | the keep gate, dedup |

---

## 6 · Wire reference (`execute::wire`)

Everything is `Serialize + Deserialize`; the envelope is **camelCase**, the
`WorkflowGraph` inside it keeps the engine's **snake_case** — both by contract,
pinned in `tests/contracts_surface.rs`.

```jsonc
// server → device
{ "attemptId": "ep-1#0",
  "graph":    { "schema_version": 1, "nodes": [...], "edges": [...] },
  "inputs":   { "repo": "acme/thing" } }

// device → server
{ "attemptId": "ep-1#0",
  "steps": [ { "nodeId": "report", "status": "success",       // "success" | "error"
               "output": { … },                               // bounded per node, 256 KiB
               "durationMs": 12, "nullBindings": [] } ],
  "pendingApprovals": [], "cancelled": false,
  "changed": "1 file changed", "failed": null, "costUsd": 0.42 }
```

Device obligation: `serde_json::from_str::<RunRequest>` → `serve(&req, &caps,
&workspace).await` → `serde_json::to_string(&report)`. `RunReport::into_ran(&graph)`
on the server rebuilds outcome + diagnosis; steps cross the wire, `Diagnosis`
does not (re-derived server-side). Budgets: `RECORD_BUDGET` 256 KiB/node
stored, `PROMPT_BUDGET` 4 KiB/node shown to the judge.

---

## 7 · Errors

| type | variants | meaning |
|---|---|---|
| `intake::IntakeError` | `Store` · `Ledger` · `Inference` · `Invalid` · `Unsupported` · `Unbindable { id, missing }` | `Invalid` = the graph is wrong; `Unsupported` = the graph is fine, this machine is the constraint |
| `ledger::LedgerError` | `Backend` · `Corrupt` | deliberately coarse — retry or give up |
| execution | *never errors* | a failed compile/run/dispatch becomes a `Ran` with `failed: Some(reason)` and still reaches `close()` |

---

## 8 · Invariants worth knowing before you build on top

- Every inference reply is gated: graphs validated, ops applied to a copy,
  lessons need cited rows, scope stamps are the handle's.
- An attempt always leaves a ledger row — including timeouts and compile
  failures. `Remote`'s no-reply synthesis reports *unknown*, not "nothing
  changed", so a socket blip cannot terminally end an episode.
- `store.save()` inside the loop is a **buffer**; nothing is durable until
  `flush`, which is how the host gates persistence on success.
- Content-derived ids (`learned-…`, `…-fix-…`) mean identical work converges
  instead of accumulating; evidence recorded early reattaches when the graph
  lands.

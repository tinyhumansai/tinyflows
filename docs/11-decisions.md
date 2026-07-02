# 11 — Decisions (ADR log)

Concise record of the decisions behind tinyflows and its OpenHuman integration.
Format: Decision · Rationale · Status.

## D1 — Build a standalone `tinyflows` crate, integrate into OpenHuman second
**Decision:** implement the engine as its own crate/repo, perfect it in isolation
(own tests/CI/docs), then integrate. **Rationale:** decouples hard engine work
from the app, yields a reusable open-source engine, and keeps host coupling in a
thin seam — the proven `tinyagents` model. **Status:** Accepted.

## D2 — Distribute via crates.io + adapter seam (not a submodule)
**Decision:** publish `tinyflows` to crates.io; OpenHuman depends on
`tinyflows = "x.y"` and adds `src/openhuman/tinyflows/`. **Rationale:** identical
to how OpenHuman consumes `tinyagents = "1.2"`; versioned, clean. **Status:**
Accepted.

## D3 — Build on `tinyagents` rather than a new executor
**Decision:** lower the workflow graph onto `tinyagents`. **Rationale:** it already
provides branch/switch/merge/parallel/loop/HITL/checkpoint — re-implementing them
would be wasteful and riskier. **Status:** Accepted.

## D4 — Host-agnostic via capability traits
**Decision:** all outside-world effects go through traits the host implements.
**Rationale:** keeps the crate reusable, dependency-light, and testable with
mocks; confines vendor coupling to the host. **Status:** Accepted.

## D5 — `serde_json::Value` as the graph state
**Decision:** use dynamic JSON as the run state (with a merge-tolerant reducer).
**Rationale:** tinyagents graphs are single-typed-state; JSON gives the
dynamic per-node I/O that a general workflow tool needs. **Status:** Accepted.
**Trade-off:** loses compile-time typing of inter-node data.

## D6 — Declarative model; arbitrary code only in a sandboxed `code` node
**Decision:** control flow is declarative nodes + a bounded expression language;
arbitrary code is confined to the explicit `code` node, executed via a
host-sandboxed `CodeRunner`. **Rationale:** analyzable, storable, shareable
definitions; no in-process interpreter. **Status:** Accepted.

## D7 — Licensing: GPL-3.0-or-later
**Decision:** license tinyflows GPL-3.0-or-later. **Rationale:** OpenHuman is GPL
v3 → compatible; ships as open source. **Status:** Accepted (no blocker).

## D8 — Node catalog is dynamic, not hand-built
**Decision:** define a small set of node *kinds*; integration actions and
app-triggers come dynamically from the host (OpenHuman: Composio). **Rationale:**
avoid hundreds of hand-maintained nodes; grow with zero code changes. **Status:**
Accepted.

## D9 — `tool_call` uses the curated catalog; HTTP for the rest
**Decision:** `tool_call` exposes only the host's curated action set; arbitrary
APIs go through the gated `http_request` node. **Rationale:** safety/consistency
without limiting capability. **Status:** Accepted.

## D10 — Trust boundary: definition authorizes, payload does not
**Decision:** an enabled, user-saved workflow authorizes its pre-declared actions
(Save = trust root); the trigger payload is untrusted data. OpenHuman implements a
`TrustedAutomationSource::Workflow` origin + optional per-automation outbound-
approval toggle. **Rationale:** enables trigger→external-action workflows while
defusing prompt injection from inbound content. **Status:** Accepted.

## D11 — Naming: "Workflows" (UI) / `automations::` (Rust)
**Decision:** user-facing "Workflows"; Rust domain `automations::`; relabel the
legacy `/settings/automations` SKILL panel to "Skills". **Rationale:** the
`workflows::` module is the unrelated SKILL.md runner; avoid collision.
**Status:** Accepted.

## D12 — Background execution: v1 "runs while app open"
**Decision:** OpenHuman v1 fires enabled workflows only while the app is open (its
core is in-process). **Rationale:** the in-process core dies with the GUI; a
headless worker (queue-mode style) is deferred. **Status:** Accepted for v1.

## D13 — Item-based data flow + `=` expressions
**Decision:** data on connections is an **array of items** (`{ json, binary? }`),
nodes map over items with best-effort **pairing**; any config field may be an
`=`-prefixed **expression** evaluated against the run. Layered over the D5
`serde_json::Value` state. **Rationale:** item semantics + data referencing are
core to real automations; deciding now keeps the `Node` I/O contract stable.
**Status:** Proposed (confirm) — shapes the A1 reducer + `Node` I/O. See
[data & expressions](13-data-and-expressions.md).

## D14 — Error-handling model
**Decision:** per-node `on_error` (`stop` | `continue` | `route`), an `error`
output port, `retry` with backoff, and `system`-trigger error workflows.
**Rationale:** graceful degradation without run-ending failures. **Status:**
Proposed — adds `Node` config; implemented A4. See
[error handling](14-error-handling.md).

## D15 — Credentials by opaque reference
**Decision:** nodes carry opaque `connection_ref`s; secrets live host-side and are
resolved inside capability impls; capability traits gain a connection parameter in
A3. **Rationale:** graphs stay safe to store/share; crate stays vendor-agnostic.
**Status:** Proposed. See [credentials](15-credentials-and-connections.md).

## D16 — Run / execution-step model + observability hook
**Decision:** define a `Run` + `ExecutionStep` record; emit via `tracing` spans +
an optional `RunObserver` hook; the host persists/renders it. **Rationale:** powers
the canvas inspect overlay and run history without the crate owning a DB.
**Status:** Proposed — hook lands A4. See [observability](16-observability-and-runs.md).

## D17 — Versioning: schema + per-node `type_version` + migrations
**Decision:** add `schema_version` (WorkflowGraph) + per-node `type_version`, with
registered load-time migrations; treat the JSON format as public API under semver.
**Rationale:** saved definitions must keep loading as the model evolves.
**Status:** Proposed — fields added A1. See
[versioning](18-versioning-and-migration.md).

## Open decisions
- **O1 — Expression library:** `jaq` (JSON-native, jq-like) vs `minijinja`
  (templating). Used for both the `transform` node and inline `=` expressions
  (D13). Decide in A2.
- **O2 — `code` node default:** sandboxed-only (recommended) vs host-exec under
  Full autonomy. Decide in B2.
- **O3 — UI order:** canvas (B3) before or after agent-first authoring (B4).

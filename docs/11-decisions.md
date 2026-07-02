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

## Open decisions
- **O1 — Expression library:** `jaq` (JSON-native, jq-like) vs `minijinja`
  (templating). Decide in A2.
- **O2 — `code` node default:** sandboxed-only (recommended) vs host-exec under
  Full autonomy. Decide in B2.
- **O3 — UI order:** canvas (B3) before or after agent-first authoring (B4).

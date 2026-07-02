# 09 — OpenHuman integration

This is the plan for embedding tinyflows into OpenHuman. It mirrors exactly how
OpenHuman consumes `tinyagents`: a **crates.io dependency** plus a thin **adapter
seam**, with a domain on top. tinyflows stays host-agnostic; all OpenHuman-specific
wiring lives in the seam and the `automations::` domain.

## When to integrate

After **Phase A** — once the crate runs all five [reference
workflows](10-reference-workflows.md) against mock capabilities and is published
to crates.io. Integrating earlier means chasing a moving API; integrating after
A5 means the seam targets a stable, versioned crate.

## The integration mechanism (same as tinyagents)

1. Add `tinyflows = "x.y"` to OpenHuman's root `Cargo.toml` (alongside
   `tinyagents = "1.2"`).
2. Create the adapter seam `src/openhuman/tinyflows/` (mirror
   `src/openhuman/tinyagents/`): `mod.rs`, `convert.rs` (WorkflowGraph ↔ domain
   types if needed), `caps.rs` (capability-trait impls), `observability.rs`,
   `tests.rs`.
3. Add the `automations::` domain following the canonical module shape.

No git submodule — crates.io only, per project convention.

## Capability trait → OpenHuman service mapping

Implement each [capability trait](05-capability-traits.md) in the seam:

| Trait | Implement with |
|-------|----------------|
| `LlmProvider` | OpenHuman inference / provider layer |
| `ToolInvoker` | curated Composio tools — filter via `is_action_visible_with_pref` + per-user scope; agent tools via `session.spawn_agent` |
| `HttpClient` | `HttpRequestTool` (`src/openhuman/tools/impl/network/http_request.rs`) — inherits `allowed_domains` allowlist + DNS guard + Network gating |
| `CodeRunner` | `node_exec` (JS) / `runtime_python` (Py) routed through `sandbox::execute_in_sandbox` (Landlock/Seatbelt/AppContainer/Docker) |
| checkpointing | `SqlRunLedgerCheckpointer` (`src/openhuman/tinyagents/checkpoint.rs`) passed to tinyagents |

## The `automations::` domain

New Rust domain (the name is free; `workflows::` is the unrelated SKILL.md runner
— **leave it alone**). Canonical module shape:

- `types.rs` — thin wrappers around the crate's `WorkflowGraph` + an `Automation`
  entity (definition + trigger binding + `enabled` + owner + last-run metadata).
- `store.rs` — `automation_definitions` SQLite table (WorkflowGraph JSON) under
  `<workspace>/automations/…`.
- `ops.rs` / `schemas.rs` — CRUD + enable/disable + run controllers; reuse the
  crate's `validate` on save. Register in `src/core/all.rs`.
- `bus.rs` — subscribe to trigger events and start runs.

Execution durability reuses `agent_orchestration/workflow_runs/` + the
`session_db/run_ledger` tables rather than a new run store.

## Triggers bridge (stage B2)

Map fired triggers to enabled workflows and start runs:

- `subconscious_triggers` already normalizes cron / inbound-message / Composio /
  sub-agent events into a unified trigger.
- `composio` delivers app events as `DomainEvent::ComposioTriggerReceived`.
- `cron` / `webhooks` provide schedule + inbound-HTTP.

Add `TrustedAutomationSource::Workflow` to `agent/turn_origin.rs` +
`approval/gate.rs` so trigger-driven runs may perform their **pre-declared**
external actions (Save = trust root) while the payload stays untrusted. Add a
per-automation "require approval for outbound actions" toggle.

## UI (stages B3–B5)

- **Canvas** (`@xyflow/react`): node palette, sub-node ports, config panels
  generated from node-kind schema + dynamic Composio schemas, run/inspect overlay
  with HITL approval. Extend `app/src/components/intelligence/*` and
  `workflowRunsApi.ts`.
- **Agent-first**: a propose tool → a `WorkflowProposalCard` (clone the
  `plan_review` + `PlanReviewCard.tsx` pattern) → Save compiles to the same
  WorkflowGraph.
- **Top-level page**: promote `/workflows` (currently a redirect) to a real page +
  bottom tab (`navConfig.ts`, `navIcons.tsx`, `AppRoutes.tsx`, `navigation.spec`
  ROUTES); relabel the legacy `/settings/automations` (Skills) panel to "Skills".

## Naming

- User-facing feature/page/tab: **"Workflows"**.
- Rust domain: `automations::`.
- Legacy `/settings/automations` (SKILL bundles) → relabel to "Skills".

## Verification

- Adapter-seam tests: each capability trait against real services with the mock
  backend (`scripts/test-rust-with-mock.sh`).
- RPC E2E (`tests/json_rpc_e2e.rs`): create → start → assert outcome for a
  workflow exercising IF + Merge + HTTP(mock) + sandboxed Code.
- Frontend: Vitest (canvas + proposal card) and WDIO (`/workflows` nav + ROUTES;
  author-and-run).

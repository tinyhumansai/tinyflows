# 18 — Versioning & migration

A `WorkflowGraph` is durable, user-authored, shareable data. Its JSON is a
**stable contract**: definitions saved today must keep loading as the model and
node kinds evolve. This doc defines the versioning scheme.

## Two version axes

### 1. Graph schema version
A top-level `schema_version` on `WorkflowGraph` (added in A1) identifies the
overall model shape. The loader runs registered **migrations** to upgrade older
graphs to the current shape on read, so persisted definitions never break.

### 2. Per-node type version
Each node carries a `type_version` (integer) for its `kind`. A node kind can evolve
its `config` shape (rename a field, change a default) by bumping `type_version` and
providing a per-kind migration from N → N+1. Old nodes load, migrate, and run.

```jsonc
{ "id": "h1", "kind": "http_request", "type_version": 2, "name": "...", "config": { } }
```

## Migration flow (on load)

```
raw JSON → parse → migrate graph (schema_version) → migrate each node (type_version)
        → validate → compile
```

Migrations are pure functions `(old_json) -> new_json`, registered per version
step. They are covered by round-trip tests with golden fixtures of old versions.

## Crate semver policy

- The **JSON format** is treated as public API: a breaking format change requires
  a migration + a crate minor/major bump per semver.
- Additive changes (new optional fields, new node kinds) are backward compatible;
  unknown-but-optional fields deserialize with defaults (`#[serde(default)]`), and
  unknown node kinds fail validation with a clear error rather than a panic.
- The Rust API follows normal semver; the crate is published to crates.io
  (see [roadmap](08-roadmap.md) A5).

## Forward compatibility

A graph authored on a **newer** crate may contain a node kind an **older** runtime
doesn't know. The loader detects this and fails with an actionable error
(`unknown node kind X, requires tinyflows >= …`) rather than silently dropping
nodes. Hosts should surface this as "update required".

## Host responsibilities

- Store `schema_version`/`type_version` with the definition.
- Run the crate's migration on load before executing.
- Decide UX for "update required" when a saved graph is newer than the runtime.

## Decision

Logged as **D17** in [decisions](11-decisions.md): add `schema_version`
(WorkflowGraph) + per-node `type_version`, with registered load-time migrations
and a semver policy treating the JSON format as public API. Marked **Proposed**
(fields added in A1).

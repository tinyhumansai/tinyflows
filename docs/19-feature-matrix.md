# 19 — Feature matrix

A capability checklist for tinyflows, with the stage each feature lands (see the
[roadmap](08-roadmap.md)). Status: ✅ done · 🟡 planned (stage) · 🔭 future.

## Triggers
| Feature | Status |
|---------|--------|
| Manual / schedule / webhook / form / chat / execute-by-workflow / system | 🟡 B2 (host bridge) |
| App-event (dynamic per connected account) | 🟡 B2 |
| One trigger per workflow, payload as run input | ✅ model |

## Control flow
| Feature | Status |
|---------|--------|
| Linear sequences | ✅ A1 |
| IF (condition, true/false) | ✅ A2 |
| Multi-way switch | ✅ A2 |
| Merge / fan-in (named inputs, barrier) | ✅ A2 |
| Parallel branches | 🟡 A2 (fan-out pending) |
| Split-out / per-item fan-out | ✅ A2 |
| Loops (bounded) | 🟡 A2 (`RecursionPolicy`) |
| Sub-workflow (nested graph) | ✅ A3 |

## AI
| Feature | Status |
|---------|--------|
| Agent node with chat_model / memory / tool / output_parser sub-ports | ✅ A3 |
| Nested sub-agents | 🟡 A3 |
| Structured / auto-fixing output parser | ✅ A3 |

## Actions & data
| Feature | Status |
|---------|--------|
| Integration action (`tool_call`, curated catalog) | ✅ A3 |
| HTTP request (arbitrary APIs, allowlisted) | ✅ A3 |
| Sandboxed code (JS / Python) | ✅ A3 |
| Data transform / field mapping + expressions | ✅ A2, interim `=`-expr; full `jq`/`jaq` deferred (O1) ([data & expressions](13-data-and-expressions.md)) |
| Item-based data flow + pairing | ✅ A1 (D13) |

## Reliability & safety
| Feature | Status |
|---------|--------|
| Per-node error policy (stop/continue/route) + error port | ✅ A4 ([error handling](14-error-handling.md)) |
| Retries with backoff | 🟡 A4 (retry ✅, backoff timing pending) |
| Per-node / per-run timeouts | 🟡 A4 |
| Human-in-the-loop approval (interrupt/resume) | 🟡 A4 |
| Durable checkpointing / resume | 🟡 A4 |
| Sandboxed code + network gating + curated tools | 🟡 A3/B (host) ([security](07-security.md)) |
| Trust boundary (definition authorizes, payload untrusted) | 🟡 B2 |

## Connections & observability
| Feature | Status |
|---------|--------|
| Opaque `connection_ref` (secrets host-side) | 🟡 A3 ([credentials](15-credentials-and-connections.md)) |
| Run / execution-step record + inspect data | 🟡 A4 ([observability](16-observability-and-runs.md)) |
| Tracing spans + `RunObserver` hook | ✅ A4 (tracing spans/events; `RunObserver` hook pending) |

## Authoring & lifecycle
| Feature | Status |
|---------|--------|
| Visual canvas editor | 🟡 B3 (host) |
| Agent-first chat authoring | 🟡 B4 (host) |
| Starter templates | 🟡 B4 (host) |
| Enable / disable, run history UI | 🟡 B1/B5 (host) |
| Schema + node `type_version` migrations | 🟡 A1 ([versioning](18-versioning-and-migration.md)) |
| Import / export JSON | 🟡 A1 (format is the contract) |

## Distribution
| Feature | Status |
|---------|--------|
| Host-agnostic (capability traits) | ✅ |
| Published to crates.io | 🟡 A5 |
| GPL-3.0-or-later | ✅ |

This matrix is the quick answer to "can it do X?" — pair it with the
[reference workflows](10-reference-workflows.md) for worked examples.

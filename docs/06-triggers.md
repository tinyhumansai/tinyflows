# 06 — Triggers

A workflow has exactly one **trigger node** — its entry point. tinyflows models
the trigger as the graph's start node and injects the trigger's payload as the
initial run state. **Firing the trigger is the host's job**; tinyflows does not
poll, listen, or schedule.

## Two categories

### Dynamic (open-ended): `app_event`
Connected-app events are **not** a hard-coded list. In OpenHuman they come from
the Composio trigger catalog, discovered per connected toolkit at runtime (e.g.
`STRIPE_PAYMENT_SUCCEEDED`, a new Gmail message, a new GitHub issue). The set
grows as the user connects accounts — with **zero code changes** to tinyflows or
the host. Config for an `app_event` trigger carries the toolkit + trigger slug +
any required trigger config.

### Built-in (small, fixed set)
These are engine/host primitives, not integrations:

| `trigger_kind` | Host source (OpenHuman) |
|----------------|-------------------------|
| `manual` | user action |
| `schedule` | `cron` domain (`Cron` / `At` / `Every`) |
| `webhook` | `webhooks` domain (inbound HTTP) |
| `form` | host form submission |
| `execute_by_workflow` | another workflow invoking this one |
| `chat_message` | inbound chat message |
| `evaluation` | evaluation/test harness |
| `system` | workflow error, file change, etc. |

## How the host bridges triggers to runs

The host owns trigger delivery and calls into its `automations::` domain to start
a run with the trigger payload as input. In OpenHuman the plumbing already exists:

- `subconscious_triggers` normalizes heterogeneous events (cron tick, inbound
  message, Composio webhook, sub-agent conclusion) into a unified trigger.
- `composio` delivers app events as `DomainEvent::ComposioTriggerReceived`
  `{ toolkit, trigger, payload, … }`.
- `cron` / `webhooks` provide schedule and inbound-HTTP triggers.

The integration work (stage B2) is a **bridge**: map a fired trigger to the
matching enabled workflow(s) and start a run. See
[OpenHuman integration](09-openhuman-integration.md).

## Trust and the trigger payload

The trigger payload is **untrusted data** — it often contains third-party content
(an email body, a comment, a webhook body) that could attempt prompt injection.
tinyflows treats it as data that flows through the graph but must not be able to
re-authorize actions. The host enforces the trust boundary: an *enabled,
user-saved* workflow authorizes its own **pre-declared** actions (the user's Save
is the trust root), while the payload cannot expand that authorization. See
[security](07-security.md).

## Background execution caveat (host concern)

Whether an enabled workflow fires while the host app is closed is a **host**
decision. OpenHuman v1 runs workflows only while the app is open (its core is
in-process and dies with the GUI); a headless worker is a future option. This does
not affect tinyflows, which is invoked per run.

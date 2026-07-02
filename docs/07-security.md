# 07 — Security model

tinyflows is designed to run automations that touch real accounts and external
data. Its safety posture combines a **declarative model**, **host-enforced
sandboxing/gating**, and a **trust boundary** between the workflow definition and
its (untrusted) trigger payload.

## 1. Declarative model — no arbitrary embedded scripting in the definition

The [`WorkflowGraph`](02-workflow-model.md) is data, not code. Control flow is
expressed with declarative nodes (`condition`, `switch`, `merge`, `split_out`,
`transform`) using a bounded expression language — not a general in-process
interpreter. This keeps the definition analyzable and safe to store/share. The
only place arbitrary code runs is the explicit `code` node, which is sandboxed
(below).

## 2. Code node — sandboxed, out-of-process

The `code` node delegates to the host's [`CodeRunner`](05-capability-traits.md).
tinyflows never executes code in-process. The host is expected to:

- run code **out-of-process** (OpenHuman: `node_exec` for JS, `runtime_python`
  for Python, via managed toolchains), and
- **sandbox** it (OpenHuman: `sandbox::execute_in_sandbox` → Landlock / Seatbelt
  / AppContainer / Docker), denying network where the OS backend supports it.

Recommended host default: allow the `code` node only under a sandboxed execution
mode (see [decisions](11-decisions.md), open item).

## 3. Network — allowlist + gating

The `http_request` node delegates to [`HttpClient`](05-capability-traits.md). The
host enforces:

- an **allowlist** of permitted domains + DNS-rebind protection (OpenHuman:
  `HttpRequestTool` with `allowed_domains` + `url_guard`), and
- **autonomy gating**: outbound network is a gated capability class (OpenHuman:
  `CommandClass::Network` → `SecurityPolicy::gate_decision`), so it can be
  blocked/prompted per the user's autonomy tier.

## 4. Integration tools — curated

`tool_call` nodes invoke integration actions through
[`ToolInvoker`](05-capability-traits.md). The host decides the catalog. OpenHuman
exposes only the **curated** Composio action set (filtered by
`is_action_visible_with_pref` + per-user read/write scope), not the full
uncurated catalog. Arbitrary external APIs are reached via the gated
`http_request` node instead — so curation does not limit capability.

## 5. Trust boundary — the definition authorizes; the payload does not

The pivotal rule for trigger-driven automation:

- A workflow's actions are authorized by the **user saving/enabling it** — the
  Save click is the trust root. This lets an enabled workflow perform its
  **pre-declared** external actions (post to Slack, send a DM) even when fired
  automatically.
- The **trigger payload is untrusted data**. It flows through the graph but must
  not be able to *expand* authorization or redirect the workflow to actions it
  didn't declare. This defuses prompt-injection via inbound content.

In OpenHuman this is implemented with a dedicated execution origin
(`TrustedAutomationSource::Workflow`) so trigger-driven runs are permitted their
declared external effects, while the payload stays tainted/untrusted — plus an
optional per-automation "require approval for outbound actions" toggle. Without
this origin, OpenHuman's gate denies external-effect tools on
externally-triggered runs. See [OpenHuman integration](09-openhuman-integration.md).

## 6. Resource bounds

Loops and recursion are bounded by tinyagents' `RecursionPolicy`
(`max_total_steps`, `max_visits_per_node`, `max_depth`), and per-node timeouts
are available — so a malformed graph cannot run unbounded.

## Division of responsibility

| Concern | tinyflows | Host |
|---------|-----------|------|
| Declarative, analyzable model | ✅ defines | — |
| Bounded expressions (no arbitrary in-model code) | ✅ | — |
| Sandbox code execution | delegates via `CodeRunner` | ✅ enforces |
| Network allowlist + gating | delegates via `HttpClient` | ✅ enforces |
| Tool curation/scoping | delegates via `ToolInvoker` | ✅ enforces |
| Trust origin (definition vs payload) | assumes it | ✅ enforces |
| Recursion/timeout budgets | ✅ sets | — |

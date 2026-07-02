# 10 — Reference workflows

These five reference workflows are the acceptance target: tinyflows must express and run
all of them. Each is decomposed into tinyflows node kinds, and a coverage table at
the end proves every construct maps to a real primitive. In stage A0 these become
golden JSON fixtures; in A3 they must run green against
[mock capabilities](05-capability-traits.md).

## 1. Create-user onboarding
*Form submission → AI agent → branch → Slack.*

- `trigger` (`form`) → `agent` (chat_model=Anthropic, memory=Postgres, tools=[Entra, Jira])
- → `condition` "Is a manager?" → **true**: `tool_call` Slack *add to channel* /
  **false**: `tool_call` Slack *update profile*.

Exercises: form trigger, agent sub-ports (model/memory/tool), IF branch, curated
tool_call actions.

## 2. Security triage
*New issue → code → parallel HTTP → merge → post.*

- `trigger` (`app_event`) → `code` "extract IPs & domains"
- → **parallel**: (`http_request` VirusTotal scan → `http_request` VirusTotal
  report) and (`http_request` urlscan.io)
- → `merge` (Input 1 + Input 2, append) → `tool_call`/`http_request` "post results".

Exercises: app-event trigger, `code` node, `http_request`, parallel branches,
`merge` fan-in.

## 3. API router agent
*Webhook → agent (+ output parser) → switch → HTTP branches.*

- `trigger` (`webhook`, GET) → `agent` (model=Gemini, tools=[Proxmox docs/wiki/api],
  output_parser=Auto-fixing → sub-agent with Groq model + structured output)
- → `switch` GET/POST/DELETE → GET: `http_request` get props → `code` structure;
  POST: `http_request`; DELETE: `condition` → `http_request` delete / `http_request` return.

Exercises: webhook trigger, agent output_parser sub-port, **nested sub-agent**,
`switch` (multi-way), `condition`, `http_request`, `code`.

## 4. Customer insights
*Vector fetch → code (ML) → split → agent → sheet.*

- `http_request` "get reviews" (Qdrant) → `code` "apply K-means" →
  `split_out` "clusters to list" → `agent` "customer insights" (model=OpenAI) →
  `tool_call` "append to Google Sheets".

Exercises: `http_request` (arbitrary API), `code` (compute), `split_out` fan-out,
agent, curated tool_call.

## 5. Trigger palette
*The set of trigger kinds a user can pick* — see [triggers](06-triggers.md):
manual, app-event, schedule, webhook, form, execute-by-another-workflow,
chat-message, evaluation, system (error/file-change). All map to
[`TriggerKind`](03-node-catalog.md).

## Coverage matrix

| Construct | Node kind / primitive | Seen in |
|-----------|-----------------------|---------|
| AI agent w/ sub-ports | `agent` (chat_model/memory/tool/output_parser) | 1, 3, 4 |
| Nested sub-agent | `agent`/`output_parser` as subgraph | 3 |
| Integration action | `tool_call` (curated Composio) | 1, 4 |
| Arbitrary HTTP | `http_request` | 2, 3, 4 |
| Sandboxed code | `code` | 2, 3, 4 |
| IF branch | `condition` (true/false ports) | 1, 3 |
| Multi-way switch | `switch` | 3 |
| Fan-in merge | `merge` | 2 |
| Parallel branches | tinyagents `with_parallel` | 2 |
| Fan-out per item | `split_out` | 4 |
| Data transform | `transform` / `code` | 2, 4 |
| All trigger kinds | `trigger` + `TriggerKind` | 1–5 |

**Conclusion:** every construct across the five workflows maps to a tinyflows node
kind backed by an existing tinyagents primitive or a host capability. The only new
construction versus reuse is the compiler, the code/HTTP capability wiring, the
expression library, and (host-side) the canvas UI.

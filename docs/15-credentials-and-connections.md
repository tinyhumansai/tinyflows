# 15 — Credentials & connections

Nodes that reach external systems (`tool_call`, `http_request`, an `agent`'s
`chat_model`) usually need to act **as a specific connected account** — which
Slack workspace, which API key, which model provider credential. tinyflows models
this **by reference only**; it never stores or sees secrets.

## The rule: opaque references, host-held secrets

- A node's config may carry a **`connection_ref`** — an opaque id that names a
  host-managed connection (e.g. `"composio:slack:acct_123"`,
  `"http_cred:virustotal"`).
- The `WorkflowGraph` stores **only the reference**, never tokens/keys. Graphs are
  therefore safe to persist, export, and share.
- At run time the host **resolves** the reference to real credentials inside its
  capability implementation. tinyflows passes the ref through; the host does the
  secret handling, OAuth refresh, and scoping.

## How it reaches the capability

The [capability traits](05-capability-traits.md) receive the connection ref as
part of the call context. Concretely (finalized in A3), the trait calls gain a
connection parameter, e.g.:

```rust
// illustrative — exact signature settled in A3
async fn invoke(&self, slug: &str, args: Value, conn: Option<&str>) -> Result<Value>;
```

or the ref is threaded via a per-run context object. Either way, the crate stays
vendor-agnostic: it knows *that* a node uses connection `X`, not *what* `X`'s
secret is.

## Selection UX (host)

- In the visual canvas, a node with a `connection_ref` shows a connection picker
  populated by the host (its connected accounts for that toolkit/provider).
- In agent-first authoring, the agent proposes a `connection_ref` (or leaves it
  unset for the user to pick on save).
- Missing//invalid connections surface as a "needs attention" state on the
  workflow (the connection-chip ⚠ in the host UI).

## OpenHuman mapping

| Node need | OpenHuman connection source |
|-----------|-----------------------------|
| `tool_call` account | Composio connection id (`composio_list_connections`) |
| `http_request` auth | host-managed HTTP credential / secret store |
| `agent` model | provider credential in the inference layer |

## Security ties

Secrets never enter the `WorkflowGraph` (see [security](07-security.md)); only
opaque refs do. Credential resolution is gated by the host's autonomy/approval
model, and connection scoping (read/write) is enforced host-side.

## Decision

Logged as **D15** in [decisions](11-decisions.md): node config carries opaque
`connection_ref`s; hosts resolve secrets in their capability impls; the capability
trait signatures gain a connection parameter in A3. Marked **Proposed**.

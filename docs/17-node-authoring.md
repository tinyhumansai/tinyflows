# 17 — Node authoring (extension guide)

Adding a new node kind is the primary extension point. This guide shows the steps;
the goal is that a contributor can add a node without touching the compiler or the
host.

## Two categories of node

- **Native** (in [`nodes::control_flow`](../src/nodes/control_flow.rs)) — pure
  logic, no outside world: `condition`, `switch`, `merge`, `split_out`,
  `transform`. Add these here.
- **Capability-backed** (in [`nodes::integration`](../src/nodes/integration.rs)) —
  needs the host via a [capability trait](05-capability-traits.md): `agent`,
  `tool_call`, `http_request`, `code`, `output_parser`, `sub_workflow`. **Never**
  call a vendor directly — go through a capability trait so the crate stays
  host-agnostic.

## Steps to add a node kind

1. **Declare the kind.** Add a variant to
   [`NodeKind`](../src/model/node_kind.rs) (serialized `snake_case`). If it's a new
   trigger firing mode, add to `TriggerKind` instead.
2. **Implement the executor.** Create a struct implementing
   [`NodeExecutor`](../src/nodes/mod.rs):
   ```rust
   #[async_trait]
   impl NodeExecutor for MyNode {
       async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
           // read ctx.node.config + ctx.state; call ctx.caps.* if needed
           // return NodeOutput::main(value) or NodeOutput::routed(value, "port")
       }
   }
   ```
3. **Declare config + ports.** Provide the node's config schema and its output
   ports so validation and the canvas config panel can be generated. (The schema
   descriptor type is finalized alongside the registry in A2/A3.)
4. **Register it.** Add the kind → executor mapping in the node registry (built by
   the compiler) so `compile()` can dispatch it.
5. **Validate.** Extend per-kind config validation (required keys, types, port
   names) in [`validate`](../src/validate.rs) / the compiler.
6. **Test.** Unit-test the executor with [mock capabilities](05-capability-traits.md);
   add a fixture graph that uses it.

## Config schema convention

Each kind owns its `config` shape (free-form JSON in the model, validated per
kind). Publishing a small JSON-schema-like descriptor per kind serves three
consumers at once: compile-time validation, the canvas config form, and the
agent-first proposer (so the agent knows how to fill it).

## Keeping it host-agnostic

- Side effects only through capability traits — no direct HTTP/SDK/file calls in a
  node.
- No host types in the crate. If a node needs something the traits don't offer,
  extend a capability trait (a deliberate, reviewed change), don't reach around it.
- Pure control-flow nodes must remain deterministic and side-effect-free.

## Checklist

- [ ] `NodeKind`/`TriggerKind` variant
- [ ] `NodeExecutor` impl (native or capability-backed)
- [ ] config schema + ports
- [ ] registry entry
- [ ] validation rules
- [ ] unit tests + fixture
- [ ] doc row in [node catalog](03-node-catalog.md)

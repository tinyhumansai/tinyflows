//! Mocks that answer the shape a node *declared*, not the shape it was asked.
//!
//! [`mock`](super::mock)'s `MockLlm` and `MockAgentRunner` echo their request
//! back. That is right for exercising the engine and wrong for exercising a
//! *graph*: an echo lets a downstream binding appear to resolve when it never
//! could, and — worse — it fails an `agent` node's own `output_parser` sub-port,
//! because the echo satisfies no declared schema. A dry run of a perfectly good
//! graph then reports `value failed schema validation after auto-fix`, which
//! reads as the author's bug and is the mock's.
//!
//! These two read `output_parser.schema` off the request and synthesise a value
//! of the declared shape, so a dry run fails on the wiring the author actually
//! got wrong and on nothing else. With no schema declared they mirror the echo
//! mocks byte for byte, so schema-less behaviour is unchanged.
//!
//! **Both are needed, and which one runs is not obvious.** The `agent` node
//! routes to an [`AgentRunner`] only when the node carries a non-empty
//! `agent_ref` *and* the host wired a runner; every other case — including the
//! agent nodes an authoring copilot generates, which carry no `agent_ref` —
//! falls through to the `llm` slot. Wiring only the runner therefore fixes
//! nothing for the most common node in a generated graph.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::caps::{AgentRunner, LlmProvider};
use crate::error::Result;

/// An [`AgentRunner`] that respects an `agent` node's
/// `config.output_parser.schema` instead of echoing its request.
///
/// A dry run wires this in place of [`MockAgentRunner`](super::mock::MockAgentRunner) so its null-resolution check (every `tool_call`
/// arg that resolves to `null`) doesn't **false-positive** on a CORRECTLY-built
/// agent node. Without it: [`MockAgentRunner`](super::mock::MockAgentRunner) always echoes
/// `{ agent, request, connection }` regardless of schema, and the
/// `agent` node's output-parser sub-port (`nodes::integration::schema`)
/// then fails that shape against ANY declared schema (no field matches) and
/// falls to a one-shot LLM auto-fix that the sandbox's plain `MockLlm` also
/// can't satisfy — so the whole dry run would error out even for a workflow a
/// real run (via a real host runner, whose completion the same sub-port
/// validates/repairs against the schema) would execute cleanly.
///
/// When `request` (the resolved node config `run_agent` receives — see
/// [`AgentRunner::run_agent`]) carries a non-null `output_parser.schema`
/// describing an object with `properties`, returns an object with every
/// declared property present, populated with a type-appropriate placeholder
/// (`string` → `""`, `number`/`integer` → `0`, `boolean` → `false`, `object` →
/// `{}`, `array` → `[]`, anything else → `null`; a property with a non-empty
/// `enum` gets its FIRST allowed value instead — see [`placeholder_for_type`])
/// — enough to satisfy the schema validator's `type`/`required`/`enum`
/// checks (see ``nodes::integration::schema::validate``) without a
/// real model call. With no schema, mirrors [`MockAgentRunner`](super::mock::MockAgentRunner)'s
/// default echo shape so dry-run behavior for schema-less agents is unchanged.
#[derive(Debug, Default, Clone)]
pub struct SchemaAwareMockAgentRunner;

#[async_trait]
impl AgentRunner for SchemaAwareMockAgentRunner {
    async fn run_agent(
        &self,
        agent_ref: &str,
        request: Value,
        conn: Option<&str>,
    ) -> Result<Value> {
        let schema = request
            .get("output_parser")
            .and_then(|parser| parser.get("schema"))
            .filter(|schema| !schema.is_null());
        match schema {
            Some(schema) => {
                let placeholder = placeholder_for_schema(schema);
                tracing::debug!(
                    target: "tinyflows",
                    agent_ref,
                    "[tinyflows] schema-aware mock: schema-aware mock agent synthesized a placeholder \
                     matching output_parser.schema"
                );
                Ok(placeholder)
            }
            None => {
                tracing::debug!(
                    target: "tinyflows",
                    agent_ref,
                    "[tinyflows] schema-aware mock: schema-aware mock agent has no output_parser.schema — \
                     mirroring the MockAgentRunner echo shape"
                );
                Ok(json!({ "agent": agent_ref, "request": request, "connection": conn }))
            }
        }
    }
}

/// An [`LlmProvider`] that respects an `agent` node's
/// `config.output_parser.schema` instead of echoing its request.
///
/// This closes the OTHER half of the same gap [`SchemaAwareMockAgentRunner`]
/// closes. The `agent` node only routes to an [`AgentRunner`] when the
/// node carries a **non-empty `agent_ref`** AND the host wired an agent registry
/// (``nodes::integration::agent``, `run_turn`:
/// `(Some(agent_ref), Some(runner)) => runner.run_agent(...)`); **every other
/// case** — and builder-generated agent nodes carry NO `agent_ref` — falls back
/// to `ctx.caps.llm.complete(cfg.clone(), conn)`. So in the sandbox those plain
/// agent nodes never reach `SchemaAwareMockAgentRunner` at all: they hit the
/// `llm` slot, which (with [`MockLlm`](super::mock::MockLlm)) echoes
/// `{ "completion": <config>, "connection": <conn> }`. The agent node's
/// output-parser sub-port then validates that echo against the declared schema
/// (`schema::parse_and_validate` — it validates the WHOLE completion value, not
/// a `.text` field), no field matches, and it falls to a one-shot LLM auto-fix
/// that the same `MockLlm` also can't satisfy — so the dry run errors with
/// `output_parser: value failed schema validation after auto-fix: missing
/// required property ...` even for a workflow a real run would execute cleanly.
/// This false-failure burned many dry-run cycles for correctly-built graphs.
///
/// When `request` (the node config the node hands to `complete` — see the
/// `_ => ctx.caps.llm.complete(cfg.clone(), conn)` arm above) carries a non-null
/// `output_parser.schema`, this returns [`placeholder_for_schema`] DIRECTLY.
/// The sub-port receives that already-schema-valid object as its `value`
/// (`validate` returns no errors), so it returns `Ok` WITHOUT ever invoking the
/// auto-fix LLM path — exactly the shape the schema validator's
/// `type`/`required`/`enum` checks accept, with no real model call. With no
/// schema, it mirrors [`MockLlm`](super::mock::MockLlm) echo shape byte-for-byte
/// (`{ "completion": request, "connection": conn }`) so schema-less agent
/// dry-run behavior — and downstream `=nodes.<agent>.item.json.completion...`
/// bindings — stay identical to today.
#[derive(Debug, Default, Clone)]
pub struct SchemaAwareMockLlm;

#[async_trait]
impl LlmProvider for SchemaAwareMockLlm {
    async fn complete(&self, request: Value, conn: Option<&str>) -> Result<Value> {
        let schema = request
            .get("output_parser")
            .and_then(|parser| parser.get("schema"))
            .filter(|schema| !schema.is_null());
        match schema {
            Some(schema) => {
                let placeholder = placeholder_for_schema(schema);
                tracing::debug!(
                    target: "tinyflows",
                    "[tinyflows] schema-aware mock: schema-aware mock LLM synthesized a placeholder \
                     matching output_parser.schema (plain agent node, no agent_ref)"
                );
                Ok(placeholder)
            }
            None => {
                tracing::debug!(
                    target: "tinyflows",
                    "[tinyflows] schema-aware mock: schema-aware mock LLM has no output_parser.schema — \
                     mirroring the MockLlm echo shape"
                );
                Ok(json!({ "completion": request, "connection": conn }))
            }
        }
    }
}

/// Builds a placeholder JSON value satisfying `schema`'s `properties`/`type`
/// constraints, for [`SchemaAwareMockAgentRunner`]. Only the shallow, top-level
/// `properties` map is populated — enough for the minimal validator in
/// `nodes::integration::schema` (`type`, `required`, `properties`);
/// deeply-nested `required` constraints on a nested `object`/`array` property
/// are a documented limitation (the placeholder for those is an empty `{}`/`[]`).
pub fn placeholder_for_schema(schema: &Value) -> Value {
    match schema.get("properties").and_then(Value::as_object) {
        Some(props) => {
            let placeholders = props
                .iter()
                .map(|(key, subschema)| (key.clone(), placeholder_for_type(subschema)));
            Value::Object(placeholders.collect())
        }
        // No `properties` to enumerate (e.g. a bare `{"type": "array"}`
        // schema) — fall back to a type-only placeholder for the schema itself.
        None => placeholder_for_type(schema),
    }
}

/// The placeholder value for one property's subschema, keyed by its
/// declared JSON-Schema `type` (see [`placeholder_for_schema`]).
///
/// An `enum` constraint is honored FIRST, before falling back to the
/// type-only placeholder: the schema validator
/// (``nodes::integration::schema::validate``) rejects any value not
/// listed in a schema's `enum`, and a generic type placeholder (e.g. `""` for
/// `{"type": "string", "enum": ["urgent", "normal"]}`) is essentially never
/// one of the allowed values — that would fail the dry run even though a real
/// agent, prompted with the schema, could easily satisfy it. The schema
/// author's own first listed value is always allowed by construction, so it's
/// returned as-is (whatever its JSON type).
pub fn placeholder_for_type(subschema: &Value) -> Value {
    if let Some(first_allowed) = subschema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return first_allowed.clone();
    }
    match subschema.get("type").and_then(Value::as_str) {
        Some("string") => json!(""),
        Some("number" | "integer") => json!(0),
        Some("boolean") => json!(false),
        Some("object") => json!({}),
        Some("array") => json!([]),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SchemaAwareMockAgentRunner ───────────────────────────────────────────

    #[tokio::test]
    async fn schema_aware_mock_agent_mirrors_the_echo_without_a_schema() {
        // No `output_parser.schema` on the request: identical shape to the
        // `MockAgentRunner` so schema-less dry runs are unaffected.
        let runner = SchemaAwareMockAgentRunner;
        let request = json!({ "prompt": "hi" });
        let out = runner
            .run_agent("researcher", request.clone(), Some("conn_1"))
            .await
            .expect("run_agent");
        assert_eq!(out["agent"], "researcher");
        assert_eq!(out["request"], request);
        assert_eq!(out["connection"], "conn_1");
    }

    #[tokio::test]
    async fn schema_aware_mock_agent_populates_declared_properties() {
        let runner = SchemaAwareMockAgentRunner;
        let request = json!({
            "prompt": "extract",
            "output_parser": { "schema": { "type": "object",
                "required": ["email", "count", "active", "meta", "tags"],
                "properties": {
                    "email": { "type": "string" },
                    "count": { "type": "integer" },
                    "active": { "type": "boolean" },
                    "meta": { "type": "object" },
                    "tags": { "type": "array" }
                } } }
        });
        let out = runner
            .run_agent("researcher", request, None)
            .await
            .expect("run_agent");
        assert_eq!(out["email"], "");
        assert_eq!(out["count"], 0);
        assert_eq!(out["active"], false);
        assert_eq!(out["meta"], json!({}));
        assert_eq!(out["tags"], json!([]));
    }

    #[tokio::test]
    async fn schema_aware_mock_agent_populates_an_enum_property_with_an_allowed_value() {
        // A generic string placeholder (`""`) would fail the vendored
        // validator's `enum` check even though a real agent could easily
        // satisfy it — the mock must pick one of the schema's own allowed
        // values (see `placeholder_for_type`'s enum handling).
        let runner = SchemaAwareMockAgentRunner;
        let request = json!({
            "prompt": "triage",
            "output_parser": { "schema": { "type": "object",
                "required": ["priority"],
                "properties": {
                    "priority": { "type": "string", "enum": ["urgent", "normal"] }
                } } }
        });
        let out = runner
            .run_agent("researcher", request, None)
            .await
            .expect("run_agent");
        let allowed = ["urgent", "normal"];
        assert!(
            allowed.contains(&out["priority"].as_str().unwrap()),
            "expected an allowed enum value, got: {out}"
        );
    }

    #[tokio::test]
    async fn schema_aware_mock_agent_ignores_null_schema() {
        // `output_parser: { schema: null }` (or no `output_parser` at all) is
        // treated identically to "no schema" — the vendored echo shape.
        let runner = SchemaAwareMockAgentRunner;
        let request = json!({ "prompt": "hi", "output_parser": { "schema": null } });
        let out = runner
            .run_agent("researcher", request.clone(), None)
            .await
            .expect("run_agent");
        assert_eq!(out["agent"], "researcher");
        assert_eq!(out["request"], request);
    }

    // ── SchemaAwareMockLlm ───────────────────────────────────────────────────

    #[tokio::test]
    async fn schema_aware_mock_llm_mirrors_the_echo_without_a_schema() {
        // No `output_parser.schema`: byte-identical to the `MockLlm`
        // so schema-less agent dry runs (which route to the `llm` slot, not the
        // runner) keep today's `{ completion, connection }` shape.
        let llm = SchemaAwareMockLlm;
        let request = json!({ "prompt": "hi" });
        let out = llm
            .complete(request.clone(), Some("conn_1"))
            .await
            .expect("complete");
        assert_eq!(out["completion"], request);
        assert_eq!(out["connection"], "conn_1");

        let without_conn = llm.complete(request, None).await.expect("complete");
        assert!(without_conn["connection"].is_null());
    }

    #[tokio::test]
    async fn schema_aware_mock_llm_synthesizes_a_schema_valid_completion() {
        // A plain agent node (no `agent_ref`) hands its config to the `llm`
        // slot; the returned object must pass the output-parser sub-port's
        // validator directly (no auto-fix hop) for every declared type.
        let llm = SchemaAwareMockLlm;
        let request = json!({
            "prompt": "extract",
            "output_parser": { "schema": { "type": "object",
                "required": ["email", "count", "active", "meta", "tags"],
                "properties": {
                    "email": { "type": "string" },
                    "count": { "type": "integer" },
                    "active": { "type": "boolean" },
                    "meta": { "type": "object" },
                    "tags": { "type": "array" }
                } } }
        });
        let out = llm.complete(request, None).await.expect("complete");
        assert_eq!(out["email"], "");
        assert_eq!(out["count"], 0);
        assert_eq!(out["active"], false);
        assert_eq!(out["meta"], json!({}));
        assert_eq!(out["tags"], json!([]));
    }

    #[tokio::test]
    async fn schema_aware_mock_llm_ignores_null_schema() {
        // `output_parser: { schema: null }` is treated as "no schema" — the
        // vendored echo shape, same as the runner's null-schema handling.
        let llm = SchemaAwareMockLlm;
        let request = json!({ "prompt": "hi", "output_parser": { "schema": null } });
        let out = llm.complete(request.clone(), None).await.expect("complete");
        assert_eq!(out["completion"], request);
    }

    #[test]
    fn placeholder_for_schema_falls_back_to_type_without_properties() {
        assert_eq!(
            placeholder_for_schema(&json!({ "type": "array" })),
            json!([])
        );
        assert_eq!(
            placeholder_for_schema(&json!({ "type": "string" })),
            json!("")
        );
    }

    #[test]
    fn placeholder_for_type_covers_every_json_schema_type() {
        assert_eq!(
            placeholder_for_type(&json!({ "type": "string" })),
            json!("")
        );
        assert_eq!(placeholder_for_type(&json!({ "type": "number" })), json!(0));
        assert_eq!(
            placeholder_for_type(&json!({ "type": "integer" })),
            json!(0)
        );
        assert_eq!(
            placeholder_for_type(&json!({ "type": "boolean" })),
            json!(false)
        );
        assert_eq!(
            placeholder_for_type(&json!({ "type": "object" })),
            json!({})
        );
        assert_eq!(placeholder_for_type(&json!({ "type": "array" })), json!([]));
        assert_eq!(placeholder_for_type(&json!({})), Value::Null);
    }

    #[test]
    fn placeholder_for_type_prefers_the_first_enum_value_over_the_generic_type() {
        // A generic type placeholder (`""`) is essentially never one of an
        // enum's allowed values, so it must never be used when `enum` is set.
        assert_eq!(
            placeholder_for_type(&json!({ "type": "string", "enum": ["urgent", "normal"] })),
            json!("urgent")
        );
        // The first enum value wins even when its JSON type doesn't match
        // `type` (schema authors sometimes skip `type` entirely with `enum`).
        assert_eq!(
            placeholder_for_type(&json!({ "enum": [1, 2, 3] })),
            json!(1)
        );
    }

    #[test]
    fn placeholder_for_type_ignores_an_empty_enum() {
        // An empty `enum` array has no first value to prefer — fall back to
        // the type-only placeholder rather than panicking or returning null.
        assert_eq!(
            placeholder_for_type(&json!({ "type": "string", "enum": [] })),
            json!("")
        );
    }
}

use super::*;
// Caller-input provenance regressions.
use super::tests::{graph, trigger};
use serde_json::json;

#[tokio::test]
async fn outputs_derived_from_unknown_inputs_remain_unverifiable() {
    let mut g = graph(
        json!([
            trigger(),
            {"id":"stable","kind":"transform","name":"Stable","config":{"set":{"body":{}}}},
            {"id":"shape","kind":"transform","name":"Shape","config":{"set":{"subject":"=if .inputs.enabled then \"hello\" else null end"}}},
            {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{
                "subject":"=nodes.shape.item.subject", "body":"=nodes.stable.item.body.missing"
            }}}
        ]),
        json!([{"from_node":"t","to_node":"stable"},{"from_node":"stable","to_node":"shape"},{"from_node":"shape","to_node":"send"}]),
    );
    g.inputs = vec![
        crate::model::WorkflowInput::new("enabled", crate::model::InputType::Boolean).required(),
    ];
    let errors = unresolvable_tool_args(&g, &[]).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("arg `body`"), "{errors:?}");
    g.nodes.insert(3, serde_json::from_value(json!({"id":"relay","kind":"transform","name":"Relay","config":{"set":{"subject":"=item.subject"}}})).unwrap());
    g.edges = serde_json::from_value(json!([
        {"from_node":"t","to_node":"stable"},{"from_node":"stable","to_node":"shape"},
        {"from_node":"shape","to_node":"relay"},{"from_node":"relay","to_node":"send"}
    ]))
    .unwrap();
    g.nodes[4].config["args"]["subject"] = json!("=item.subject");
    // Dependency propagation must also work when declaration order is reversed.
    g.nodes.reverse();
    let errors = unresolvable_tool_args(&g, &[]).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("arg `body`"), "{errors:?}");
}

#[tokio::test]
async fn bracketed_runtime_scope_and_identity_child_mappings_remain_opaque() {
    let mut child = graph(
        json!([
            trigger(), {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{"subject":"=inputs.payload.subject"}}}
        ]),
        json!([{"from_node":"t","to_node":"send"}]),
    );
    child.inputs =
        vec![crate::model::WorkflowInput::new("payload", crate::model::InputType::Json).required()];
    for (mapping, argument) in [
        ("=.[\"run\"].payload", "=inputs.payload.subject"),
        ("=.", "=inputs.payload.run.subject"),
        ("=.[\"r\"+\"un\"].payload", "=inputs.payload.subject"),
        ("=.[(\"r\"+\"un\")].payload", "=inputs.payload.subject"),
    ] {
        child.nodes[1].config["args"]["subject"] = json!(argument);
        let parent = graph(
            json!([
                trigger(), {"id":"child","kind":"sub_workflow","name":"Child","config":{"workflow":child,"inputs":{"payload":mapping}}}
            ]),
            json!([{"from_node":"t","to_node":"child"}]),
        );
        let errors = unresolvable_tool_args(&parent, &[]).await;
        assert!(errors.is_empty(), "{mapping}: {errors:?}");
    }
}

#[tokio::test]
async fn mixed_known_inputs_and_runtime_scopes_keep_child_values_unverifiable() {
    let mut child = graph(
        json!([trigger(), {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{"subject":"=inputs.payload.subject"}}}]),
        json!([{"from_node":"t","to_node":"send"}]),
    );
    child.inputs =
        vec![crate::model::WorkflowInput::new("payload", crate::model::InputType::Json).required()];
    for mapping in ["=.inputs.base + .run.extra", "=inputs.a // run.b"] {
        let root = graph(
            json!([trigger(), {"id":"child","kind":"sub_workflow","name":"Child","config":{"workflow":child,"inputs":{"payload":mapping}}}]),
            json!([{"from_node":"t","to_node":"child"}]),
        );
        let errors = unresolvable_tool_args(&root, &[]).await;
        assert!(errors.is_empty(), "{mapping}: {errors:?}");
    }
}

#[tokio::test]
async fn constant_object_scope_keys_do_not_hide_missing_child_fields() {
    let mut child = graph(
        json!([trigger(), {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{"subject":"=inputs.payload.item.subject"}}}]),
        json!([{"from_node":"t","to_node":"send"}]),
    );
    child.inputs =
        vec![crate::model::WorkflowInput::new("payload", crate::model::InputType::Json).required()];
    let parent = graph(
        json!([trigger(), {"id":"child","kind":"sub_workflow","name":"Child","config":{"workflow":child,"inputs":{"payload":"={item: {}}"}}}]),
        json!([{"from_node":"t","to_node":"child"}]),
    );
    let errors = unresolvable_tool_args(&parent, &[]).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("arg `subject`"), "{errors:?}");
}

#[test]
fn mixed_input_and_runtime_mappings_retain_unknown_provenance() {
    let mut child = graph(json!([trigger()]), json!([]));
    child.inputs =
        vec![crate::model::WorkflowInput::new("payload", crate::model::InputType::Json).required()];
    for expression in ["=.inputs.base + .run.extra", "=inputs.a // run.b"] {
        let inputs = MockInputs::default().child(&child, &json!({"inputs":{"payload":expression}}));
        assert!(
            inputs.is_unverifiable("=inputs.payload.subject"),
            "{expression}"
        );
    }
}

#[test]
fn unquoted_scope_keys_are_literals_but_their_values_can_read_runtime_data() {
    for scope in ["inputs", "run", "item", "items", "nodes"] {
        assert!(input_scope::scope_reads(&format!("={{{scope}: {{}}}}")).is_empty());
        assert_eq!(
            input_scope::scope_reads(&format!("={{{scope}: .run.value}}"))[0].0,
            "run"
        );
    }
}

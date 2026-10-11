use super::*;
// Saved workflows reached with a larger remaining budget must be revisited.
use super::tests::{graph, trigger};
use serde_json::json;
use std::collections::HashMap;

struct SavedHost(HashMap<String, WorkflowGraph>);

#[async_trait::async_trait]
impl crate::caps::WorkflowResolver for SavedHost {
    async fn resolve(&self, id: &str) -> crate::error::Result<WorkflowGraph> {
        self.0
            .get(id)
            .cloned()
            .ok_or_else(|| crate::error::EngineError::Capability(id.into()))
    }
}

fn saved_child(id: &str) -> WorkflowGraph {
    graph(
        json!([trigger(), {"id":"child","kind":"sub_workflow","name":"Child","config":{"workflow_id":id}}]),
        json!([{"from_node":"t","to_node":"child"}]),
    )
}

#[tokio::test]
async fn shallower_saved_workflow_visits_discover_descendants_with_remaining_budget() {
    let leaf = graph(
        json!([
            trigger(),
            {"id":"shape","kind":"transform","name":"Shape","config":{"set":{"subject":null}}},
            {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{"subject":"=nodes.shape.item.subject"}}}
        ]),
        json!([{"from_node":"t","to_node":"shape"},{"from_node":"shape","to_node":"send"}]),
    );
    let shared = saved_child("leaf");
    let shallow = saved_child("shared");
    let deep = graph(
        json!([trigger(), {"id":"inner","kind":"sub_workflow","name":"Inner","config":{"workflow":shallow}}]),
        json!([{"from_node":"t","to_node":"inner"}]),
    );
    // LIFO discovery visits the later-declared deep branch before the shallow one.
    let root = graph(
        json!([
            {"id":"t","kind":"trigger","name":"Start","config":{"max_sub_workflow_depth":3}},
            {"id":"shallow","kind":"sub_workflow","name":"Shallow","config":{"workflow":shallow}},
            {"id":"deep","kind":"sub_workflow","name":"Deep","config":{"workflow":deep}}
        ]),
        json!([{"from_node":"t","to_node":"shallow"},{"from_node":"t","to_node":"deep"}]),
    );
    let host = SavedHost(HashMap::from([
        ("shared".into(), shared),
        ("leaf".into(), leaf),
    ]));
    let errors = unresolvable_tool_args_with_resolver(&root, &[], Some(&host)).await;
    assert!(
        errors.iter().any(|error| error.contains("arg `subject`")),
        "{errors:?}"
    );
}

#[tokio::test]
async fn dynamic_workflow_ids_do_not_resolve_unrelated_literal_catalog_entries() {
    let leaf = graph(
        json!([trigger(), {"id":"shape","kind":"transform","name":"Shape","config":{"set":{"subject":{}}}},
            {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{"subject":"=item.subject.missing"}}}]),
        json!([{"from_node":"t","to_node":"shape"},{"from_node":"shape","to_node":"send"}]),
    );
    let host = SavedHost(HashMap::from([("=run.workflow_id".into(), leaf)]));
    let errors =
        unresolvable_tool_args_with_resolver(&saved_child("=run.workflow_id"), &[], Some(&host))
            .await;
    assert!(errors.is_empty(), "{errors:?}");
}

#[tokio::test(start_paused = true)]
async fn traversal_timeout_retains_errors_from_completed_mock_runs() {
    let settled = graph(
        json!([trigger(), {"id":"shape","kind":"transform","name":"Shape","config":{"set":{"subject":{}}}},
            {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{"subject":"=item.subject.missing"}}}]),
        json!([{"from_node":"t","to_node":"shape"},{"from_node":"shape","to_node":"send"}]),
    );
    let completed = std::sync::Mutex::new(Vec::new());
    let check = async {
        let findings = graph_unresolvable_tool_args(
            &settled,
            &[],
            MockInputs::root(&settled),
            Arc::new(crate::caps::mock::MockWorkflowResolver::default()),
            (0, crate::compat::max_sub_workflow_depth(&settled)),
        )
        .await;
        assert_eq!(
            findings.len(),
            1,
            "the settled engine run must reject the known missing field"
        );
        completed.lock().unwrap().extend(findings);
        // A later lookup or child check never answers; virtual time reaches the shared deadline.
        std::future::pending::<()>().await;
    };
    let errors = traversal::within_deadline(check, &completed).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("arg `subject`"), "{errors:?}");
}

#[tokio::test]
async fn expression_workflow_ids_are_not_passed_to_the_saved_catalog() {
    struct RejectLookup;
    #[async_trait::async_trait]
    impl crate::caps::WorkflowResolver for RejectLookup {
        async fn resolve(&self, id: &str) -> crate::error::Result<WorkflowGraph> {
            panic!("dynamic ID passed to catalog: {id}");
        }
    }
    let root = saved_child("=run.workflow_id");
    let errors = unresolvable_tool_args_with_resolver(&root, &[], Some(&RejectLookup)).await;
    assert!(errors.is_empty(), "{errors:?}");
}

#[tokio::test(start_paused = true)]
async fn completed_diagnostics_survive_a_later_child_timeout() {
    let slow = graph(
        json!([trigger(),
            {"id":"scatter","kind":"scatter","name":"Scatter","config":{}},
            {"id":"body","kind":"transform","name":"Body","config":{"set":{"value":1}}},
            {"id":"gather","kind":"gather","name":"Gather","config":{"from":[],"poll_interval_ms":1000,"max_polls":100}}
        ]),
        json!([{"from_node":"t","to_node":"scatter"},{"from_node":"scatter","to_node":"body"},{"from_node":"body","to_node":"gather"}]),
    );
    let compiled = crate::compiler::compile(&slow).expect("slow graph compiles");
    let caps = crate::caps::mock::mock_capabilities();
    let probe = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        crate::engine::run(&compiled, crate::engine::RunInput::new(json!({})), &caps),
    )
    .await;
    assert!(probe.is_err(), "slow graph settled: {probe:?}");
    let root = graph(
        json!([trigger(),
            {"id":"shape","kind":"transform","name":"Shape","config":{"set":{"subject":null}}},
            {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{"subject":"=nodes.shape.item.subject"}}},
            {"id":"slow","kind":"sub_workflow","name":"Slow","config":{"workflow":slow}}
        ]),
        json!([{"from_node":"t","to_node":"shape"},{"from_node":"shape","to_node":"send"}]),
    );
    let errors = unresolvable_tool_args(&root, &[]).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("arg `subject`"), "{errors:?}");
}

#[tokio::test]
async fn child_mock_runs_inherit_the_root_nesting_budget() {
    let mut nested = graph(json!([trigger()]), json!([]));
    for _ in 0..2 {
        nested = graph(
            json!([trigger(), {"id":"child","kind":"sub_workflow","name":"Child","config":{"workflow":nested}}]),
            json!([{"from_node":"t","to_node":"child"}]),
        );
    }
    for child_cap in [None, Some(1)] {
        let mut child = graph(
            json!([trigger(),
                {"id":"shape","kind":"transform","name":"Shape","config":{"set":{"subject":{}}}},
                {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{"subject":"=item.subject.missing"}}},
                {"id":"nested","kind":"sub_workflow","name":"Nested","config":{"workflow":nested}}]),
            json!([{"from_node":"t","to_node":"shape"},{"from_node":"shape","to_node":"send"},
                {"from_node":"send","to_node":"nested"}]),
        );
        if let Some(cap) = child_cap {
            child.nodes[0].config["max_sub_workflow_depth"] = json!(cap);
        }
        let mut root = graph(
            json!([
                {"id":"t","kind":"trigger","name":"Start","config":{"max_sub_workflow_depth":4}},
                {"id":"child","kind":"sub_workflow","name":"Child","config":{"workflow":child}}
            ]),
            json!([{"from_node":"t","to_node":"child"}]),
        );
        let errors = unresolvable_tool_args(&root, &[]).await;
        assert_eq!(errors.len(), 1, "child cap {child_cap:?}: {errors:?}");
        assert!(errors[0].contains("arg `subject`"), "{errors:?}");
        root.nodes[0].config["max_sub_workflow_depth"] = json!(2);
        let errors = unresolvable_tool_args(&root, &[]).await;
        assert!(
            errors.is_empty(),
            "out-of-budget runs must stay best effort: {errors:?}"
        );
    }
}

#[tokio::test]
async fn child_mock_runs_inherit_the_root_depth_cap_and_current_depth() {
    let mut nested = graph(json!([trigger()]), json!([]));
    for _ in 0..1 {
        nested = graph(
            json!([trigger(), {"id":"child","kind":"sub_workflow","name":"Child","config":{"workflow":nested}}]),
            json!([{"from_node":"t","to_node":"child"}]),
        );
    }
    let child = graph(
        json!([
            {"id":"t","kind":"trigger","name":"Start","config":{"max_sub_workflow_depth":1}},
            {"id":"nested","kind":"sub_workflow","name":"Nested","config":{"workflow":nested}},
            {"id":"shape","kind":"transform","name":"Shape","config":{"set":{"subject":null}}},
            {"id":"send","kind":"tool_call","name":"Send","config":{"slug":"provider:send","args":{"subject":"=item.subject"}}}
        ]),
        json!([
            {"from_node":"t","to_node":"nested"},
            {"from_node":"nested","to_node":"shape"},
            {"from_node":"shape","to_node":"send"}
        ]),
    );
    let root = graph(
        json!([
            {"id":"t","kind":"trigger","name":"Start","config":{"max_sub_workflow_depth":3}},
            {"id":"child","kind":"sub_workflow","name":"Child","config":{"workflow":child}}
        ]),
        json!([{"from_node":"t","to_node":"child"}]),
    );
    let errors = unresolvable_tool_args(&root, &[]).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("arg `subject`"), "{errors:?}");
    assert!(
        errors[0].contains("Inline sub_workflow node 'child'"),
        "{errors:?}"
    );
}

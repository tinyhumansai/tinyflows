//! Tests for breakpoint specs and their conditions.
//!
//! Conditions are evaluated against a real [`StepFrame`], built here by hand so
//! each predicate can be exercised without driving a whole run.

use super::*;
use crate::data::Item;
use crate::model::{Node, NodeKind};
use serde_json::json;

fn node() -> Node {
    Node {
        id: "call".to_string(),
        kind: NodeKind::ToolCall,
        type_version: 1,
        name: "call".to_string(),
        config: json!({ "slug": "svc.do" }),
        ports: vec![],
        position: None,
    }
}

/// A frame for `node`, at `phase`, carrying `input`.
fn frame<'a>(
    node: &'a Node,
    phase: StepPhase,
    input: &'a [Item],
    run: &'a serde_json::Value,
    nodes: &'a serde_json::Value,
    state: &'a serde_json::Value,
    error: Option<&'a crate::error::EngineError>,
) -> StepFrame<'a> {
    StepFrame {
        phase,
        node,
        step: 0,
        attempts: 0,
        input,
        run,
        nodes,
        state,
        lane: None,
        resume: None,
        output: None,
        error,
    }
}

#[test]
fn always_holds_everywhere() {
    let node = node();
    let (run, nodes, state) = (json!({}), json!({}), json!({}));
    let f = frame(&node, StepPhase::Before, &[], &run, &nodes, &state, None);
    assert!(Condition::Always.holds(&f, 1));
}

#[test]
fn on_error_never_holds_before_a_node_runs() {
    // No error exists yet at `Before`. A silent no-match rather than an error,
    // because "break where it breaks" is reasonable to ask without naming a
    // phase.
    let node = node();
    let (run, nodes, state) = (json!({}), json!({}), json!({}));
    let f = frame(&node, StepPhase::Before, &[], &run, &nodes, &state, None);
    assert!(!Condition::OnError.holds(&f, 1));
}

#[test]
fn on_error_holds_for_a_failed_activation() {
    let node = node();
    let (run, nodes, state) = (json!({}), json!({}), json!({}));
    let err = crate::error::EngineError::Capability("boom".into());
    let f = frame(
        &node,
        StepPhase::After,
        &[],
        &run,
        &nodes,
        &state,
        Some(&err),
    );
    assert!(Condition::OnError.holds(&f, 1));
}

#[test]
fn activation_selects_one_pass_of_a_loop() {
    let node = node();
    let (run, nodes, state) = (json!({}), json!({}), json!({}));
    let f = frame(&node, StepPhase::Before, &[], &run, &nodes, &state, None);
    assert!(!Condition::Activation(3).holds(&f, 1));
    assert!(!Condition::Activation(3).holds(&f, 2));
    assert!(Condition::Activation(3).holds(&f, 3));
}

#[test]
fn an_expression_condition_reads_the_activations_scope() {
    let node = node();
    let (run, nodes, state) = (json!({}), json!({}), json!({}));
    let input = vec![Item::new(json!({ "status": "error" }))];
    let f = frame(&node, StepPhase::Before, &input, &run, &nodes, &state, None);

    assert!(Condition::Expr("=item.status == \"error\"".into()).holds(&f, 1));
    assert!(!Condition::Expr("=item.status == \"ok\"".into()).holds(&f, 1));
}

#[test]
fn an_expression_that_resolves_to_null_or_false_does_not_hold() {
    // jq's notion of truthy, so a binding onto a field nothing produces reads
    // as "no" rather than firing on every activation.
    let node = node();
    let (run, nodes, state) = (json!({}), json!({}), json!({}));
    let f = frame(&node, StepPhase::Before, &[], &run, &nodes, &state, None);
    assert!(!Condition::Expr("=item.nope".into()).holds(&f, 1));
    assert!(!Condition::Expr("=false".into()).holds(&f, 1));
}

#[test]
fn all_and_any_compose() {
    let node = node();
    let (run, nodes, state) = (json!({}), json!({}), json!({}));
    let f = frame(&node, StepPhase::Before, &[], &run, &nodes, &state, None);

    assert!(Condition::All(vec![Condition::Always, Condition::Activation(1)]).holds(&f, 1));
    assert!(!Condition::All(vec![Condition::Always, Condition::OnError]).holds(&f, 1));
    assert!(Condition::Any(vec![Condition::OnError, Condition::Always]).holds(&f, 1));
    assert!(!Condition::Any(vec![Condition::OnError]).holds(&f, 1));
}

#[test]
fn a_spec_matches_only_its_own_node_and_phase() {
    let node = node();
    let (run, nodes, state) = (json!({}), json!({}), json!({}));
    let f = frame(&node, StepPhase::Before, &[], &run, &nodes, &state, None);

    assert!(BreakpointSpec::before("call").matches(&f, 1));
    assert!(!BreakpointSpec::before("other").matches(&f, 1));
    // An after-breakpoint does not fire at the before phase.
    assert!(!BreakpointSpec::after("call").matches(&f, 1));
}

#[test]
fn an_any_target_matches_every_node() {
    let node = node();
    let (run, nodes, state) = (json!({}), json!({}), json!({}));
    let f = frame(&node, StepPhase::Before, &[], &run, &nodes, &state, None);
    let spec = BreakpointSpec {
        target: NodeTarget::Any,
        before: true,
        after: false,
        condition: Condition::Always,
        mode: PauseMode::Live,
        max_hits: None,
    };
    assert!(spec.matches(&f, 1));
}

#[test]
fn a_spec_round_trips_through_json() {
    // Breakpoints arrive over the tool surface as JSON.
    let spec = BreakpointSpec::before("send")
        .when(Condition::Activation(2))
        .once();
    let encoded = serde_json::to_string(&spec).expect("serialize");
    let decoded: BreakpointSpec = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(spec, decoded);
}

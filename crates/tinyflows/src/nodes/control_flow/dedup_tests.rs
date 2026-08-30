use super::*;
use crate::caps::Capabilities;
use crate::caps::mock::mock_capabilities;
use crate::data::Item;
use crate::model::{Node, NodeKind};
use serde_json::json;

fn dedup_node(id: &str, config: Value) -> Node {
    Node {
        id: id.to_string(),
        kind: NodeKind::Dedup,
        type_version: 1,
        name: id.to_string(),
        config,
        ports: Vec::new(),
        position: None,
    }
}

async fn run_dedup(caps: &Capabilities, node: &Node, input: &[Item]) -> NodeOutput {
    let run = Value::Null;
    let ctx = NodeContext {
        node,
        input,
        run: &run,
        nodes: &Value::Null,
        caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    DedupNode.execute(ctx).await.expect("execute")
}

#[tokio::test]
async fn unseen_keys_pass_through_and_are_staged_tentative() {
    // Test 1 (spec): pre-seed committed with "a"; input a,b,c → only b,c
    // pass, and b,c land in tentative.
    let caps = mock_capabilities();
    caps.state
        .store(&committed_key("dd"), json!(["a"]))
        .await
        .unwrap();

    let node = dedup_node("dd", json!({ "key": "=item.id" }));
    let input = vec![
        Item::new(json!({ "id": "a" })),
        Item::new(json!({ "id": "b" })),
        Item::new(json!({ "id": "c" })),
    ];
    let out = run_dedup(&caps, &node, &input).await;

    let passed_ids: Vec<&str> = out
        .items
        .iter()
        .map(|i| i.json["id"].as_str().unwrap())
        .collect();
    assert_eq!(passed_ids, vec!["b", "c"], "a was already committed");
    assert_eq!(out.port, None, "dedup emits on the default main port");

    let tentative = caps
        .state
        .load(&tentative_key("dd"))
        .await
        .unwrap()
        .expect("tentative written");
    let mut tentative_arr: Vec<&str> = tentative
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    tentative_arr.sort_unstable();
    assert_eq!(tentative_arr, vec!["b", "c"]);
}

#[tokio::test]
async fn committed_set_is_never_mutated_by_the_node_itself() {
    // Test 2 (spec): after the node runs, committed is UNCHANGED — proves
    // the node doesn't self-commit. The host (not this crate) is what
    // unions tentative into committed on run success, and clears
    // tentative on run failure — a crashed/failed run leaves committed
    // exactly as it was, so a retry can safely reprocess the same items.
    let caps = mock_capabilities();
    caps.state
        .store(&committed_key("dd"), json!(["a"]))
        .await
        .unwrap();

    let node = dedup_node("dd", json!({ "key": "=item.id" }));
    let input = vec![
        Item::new(json!({ "id": "a" })),
        Item::new(json!({ "id": "b" })),
    ];
    run_dedup(&caps, &node, &input).await;

    let committed = caps
        .state
        .load(&committed_key("dd"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed, json!(["a"]), "committed must be untouched");
}

#[tokio::test]
async fn null_or_empty_key_fails_open_and_is_not_recorded() {
    // Test 3 (spec): a null/absent/empty key passes through and is NOT
    // added to tentative.
    let caps = mock_capabilities();
    let node = dedup_node("dd", json!({ "key": "=item.missing" }));
    let input = vec![
        Item::new(json!({ "other": 1 })),    // "=item.missing" resolves null
        Item::new(json!({ "missing": "" })), // resolves to an empty string
    ];
    let out = run_dedup(&caps, &node, &input).await;

    assert_eq!(out.items.len(), 2, "both fail-open items pass through");
    assert!(
        caps.state
            .load(&tentative_key("dd"))
            .await
            .unwrap()
            .is_none(),
        "a fail-open key must never be written to tentative"
    );
}

#[tokio::test]
async fn two_dedup_nodes_keep_separate_committed_and_tentative_sets() {
    // Test 4 (spec): two dedup nodes (different node ids) in one flow keep
    // separate sets — the node-id discriminator in the StateStore key.
    let caps = mock_capabilities();
    caps.state
        .store(&committed_key("dd1"), json!(["shared"]))
        .await
        .unwrap();

    let node1 = dedup_node("dd1", json!({ "key": "=item.id" }));
    let node2 = dedup_node("dd2", json!({ "key": "=item.id" }));
    let input = vec![Item::new(json!({ "id": "shared" }))];

    let out1 = run_dedup(&caps, &node1, &input).await;
    assert!(out1.items.is_empty(), "dd1 already committed \"shared\"");

    let out2 = run_dedup(&caps, &node2, &input).await;
    assert_eq!(
        out2.items.len(),
        1,
        "dd2's committed set is independent of dd1's and has never seen \"shared\""
    );

    let dd2_tentative = caps
        .state
        .load(&tentative_key("dd2"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dd2_tentative, json!(["shared"]));
    assert!(
        caps.state
            .load(&tentative_key("dd1"))
            .await
            .unwrap()
            .is_none(),
        "dd1 emitted nothing this run, so it wrote no tentative keys"
    );
}

#[tokio::test]
async fn same_key_twice_in_one_run_passes_once_only() {
    // Test 5 (spec, pinned convention): a duplicate key within one run's
    // input batch passes on its first occurrence only; later duplicates
    // are dropped by the same rule as an already-committed key.
    let caps = mock_capabilities();
    let node = dedup_node("dd", json!({ "key": "=item.id" }));
    let input = vec![
        Item::new(json!({ "id": "x", "n": 1 })),
        Item::new(json!({ "id": "x", "n": 2 })),
        Item::new(json!({ "id": "y", "n": 3 })),
    ];
    let out = run_dedup(&caps, &node, &input).await;

    assert_eq!(out.items.len(), 2, "the second \"x\" is dropped");
    assert_eq!(out.items[0].json["n"], 1, "the FIRST occurrence wins");
    assert_eq!(out.items[1].json["id"], "y");

    let tentative = caps
        .state
        .load(&tentative_key("dd"))
        .await
        .unwrap()
        .unwrap();
    let mut arr: Vec<&str> = tentative
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    arr.sort_unstable();
    assert_eq!(arr, vec!["x", "y"], "x is staged exactly once, not twice");
}

#[tokio::test]
async fn a_key_already_present_in_stale_tentative_is_preserved_across_runs() {
    // A prior run's tentative write survives into the NEXT run's load —
    // this node only ever unions into tentative, it never overwrites it
    // wholesale. (Whether the host should have cleared stale tentative
    // before this run is a host-side concern this node has no opinion on.)
    let caps = mock_capabilities();
    caps.state
        .store(&tentative_key("dd"), json!(["stale"]))
        .await
        .unwrap();

    let node = dedup_node("dd", json!({ "key": "=item.id" }));
    let input = vec![Item::new(json!({ "id": "fresh" }))];
    run_dedup(&caps, &node, &input).await;

    let tentative = caps
        .state
        .load(&tentative_key("dd"))
        .await
        .unwrap()
        .unwrap();
    let mut arr: Vec<&str> = tentative
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    arr.sort_unstable();
    assert_eq!(arr, vec!["fresh", "stale"]);
}

#[tokio::test]
async fn non_string_resolved_key_is_canonicalized() {
    // A key expression that resolves to a JSON number still dedupes.
    let caps = mock_capabilities();
    let node = dedup_node("dd", json!({ "key": "=item.id" }));
    let input = vec![Item::new(json!({ "id": 7 })), Item::new(json!({ "id": 7 }))];
    let out = run_dedup(&caps, &node, &input).await;
    assert_eq!(out.items.len(), 1, "numeric key 7 dedupes against itself");
}

#[tokio::test]
async fn empty_input_is_a_no_op_and_writes_nothing() {
    let caps = mock_capabilities();
    let node = dedup_node("dd", json!({ "key": "=item.id" }));
    let out = run_dedup(&caps, &node, &[]).await;
    assert!(out.items.is_empty());
    assert!(
        caps.state
            .load(&tentative_key("dd"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn missing_key_config_fails_open_for_every_item() {
    // No `key` in config at all (e.g. a bare/default config in a generic
    // dispatch test) — same fail-open path as a key that resolves null.
    let caps = mock_capabilities();
    let node = dedup_node("dd", Value::Null);
    let input = vec![Item::new(json!({ "id": "a" }))];
    let out = run_dedup(&caps, &node, &input).await;
    assert_eq!(out.items.len(), 1);
    assert!(
        caps.state
            .load(&tentative_key("dd"))
            .await
            .unwrap()
            .is_none()
    );
}

use super::*;
use serde_json::json;

fn lane(index: usize, value: &str) -> Arrived {
    Arrived {
        index,
        items: vec![Item::new(json!({ "v": value }))],
        failed: None,
    }
}

/// The determinism property: output follows lane order, not arrival order.
#[test]
fn lanes_are_emitted_in_index_order_whatever_the_arrival_order() {
    let arrived = vec![lane(2, "c"), lane(0, "a"), lane(1, "b")];
    let items = emit_items(arrived, OnLaneError::Collect);
    let values: Vec<&str> = items
        .iter()
        .filter_map(|item| item.json["v"].as_str())
        .collect();
    assert_eq!(values, vec!["a", "b", "c"]);
    assert_eq!(
        items.iter().map(|i| i.paired_item).collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(2)],
        "each item keeps the index of the lane it came from"
    );
}

#[test]
fn a_failed_lane_becomes_a_branchable_item_by_default() {
    let arrived = vec![
        lane(0, "ok"),
        Arrived {
            index: 1,
            items: vec![],
            failed: Some("boom".to_string()),
        },
    ];
    let items = emit_items(arrived, OnLaneError::Collect);
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].json["failed"], true);
    assert_eq!(items[1].json["error"], "boom");
}

#[test]
fn skip_drops_a_failed_lane_entirely() {
    let arrived = vec![
        lane(0, "ok"),
        Arrived {
            index: 1,
            items: vec![],
            failed: Some("boom".to_string()),
        },
    ];
    let items = emit_items(arrived, OnLaneError::Skip);
    assert_eq!(items.len(), 1, "only the successful lane survives");
}

#[test]
fn on_lane_error_defaults_to_collect() {
    assert_eq!(OnLaneError::from_config(&json!({})), OnLaneError::Collect);
    assert_eq!(
        OnLaneError::from_config(&json!({ "on_lane_error": "nonsense" })),
        OnLaneError::Collect,
        "an unrecognised policy must not silently fail the run"
    );
}

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

/// The lane indices a gather over `evaluate` would count, and its expected total.
fn counted(nodes: &Value) -> (Vec<usize>, Option<usize>) {
    let slots = lane_slots(nodes, &["evaluate".to_string()]);
    let scatter = scatter_of(&slots);
    let mut indices: Vec<usize> = arrivals(current_activation(nodes, slots, scatter))
        .iter()
        .map(|lane| lane.index)
        .collect();
    indices.sort_unstable();
    (indices, expected_lanes(nodes, scatter))
}

fn stamped(index: usize, step: u64) -> Value {
    json!({
        "items": [], "status": "ok", "index": index,
        "_scatter": "scatter", "_scatter_step": step,
    })
}

/// A second loop pass opened 2 lanes at step 9; pass 1's lane #2 (step 3) is
/// still in state and must not be counted.
#[test]
fn lanes_from_an_earlier_scatter_activation_are_ignored() {
    let nodes = json!({
        "scatter": { "lane_count": 2, "_activation_step": 9 },
        "evaluate": { "lanes": {
            "scatter#0": stamped(0, 9),
            "scatter#1": stamped(1, 9),
            "scatter#2": stamped(2, 3),
        } },
    });
    assert_eq!(counted(&nodes), (vec![0, 1], Some(2)));
}

/// Before any of this pass's lanes land, the earlier pass's lanes count for
/// nothing — the gather waits rather than releasing stale results.
#[test]
fn an_activation_with_no_lanes_landed_yet_counts_none() {
    let nodes = json!({
        "scatter": { "lane_count": 2, "_activation_step": 9 },
        "evaluate": { "lanes": { "scatter#0": stamped(0, 3), "scatter#1": stamped(1, 3) } },
    });
    assert_eq!(counted(&nodes), (vec![], Some(2)));
}

/// The expected total is this gather's own scatter's, not the first
/// `lane_count` anywhere in state.
#[test]
fn the_expected_count_comes_from_the_scatter_that_opened_the_lanes() {
    let nodes = json!({
        "an_earlier_scatter": { "lane_count": 7, "_activation_step": 1 },
        "scatter": { "lane_count": 1, "_activation_step": 4 },
        "evaluate": { "lanes": { "scatter#0": stamped(0, 4) } },
    });
    assert_eq!(counted(&nodes), (vec![0], Some(1)));
}

/// Slots written from a pre-stamp envelope (a checkpoint from an older binary)
/// all count when nothing is stamped, so an in-flight run still completes.
#[test]
fn unstamped_slots_count_when_no_slot_is_stamped() {
    let nodes = json!({
        "scatter": { "lane_count": 2, "_activation_step": 9 },
        "evaluate": { "lanes": {
            "scatter#0": { "items": [], "status": "ok", "index": 0 },
            "scatter#1": { "items": [], "status": "ok", "index": 1 },
        } },
    });
    assert_eq!(counted(&nodes), (vec![0, 1], Some(2)));
}

/// Once this pass's lanes are stamped, unstamped slots are an earlier pass's.
#[test]
fn unstamped_slots_are_stale_once_a_stamped_slot_exists() {
    let nodes = json!({
        "scatter": { "lane_count": 1, "_activation_step": 9 },
        "evaluate": { "lanes": {
            "scatter#0": stamped(0, 9),
            "scatter#1": { "items": [], "status": "ok", "index": 1 },
        } },
    });
    assert_eq!(counted(&nodes), (vec![0], Some(1)));
}

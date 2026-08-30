use super::*;
use serde_json::json;

fn items(n: usize) -> Vec<Item> {
    (0..n).map(|i| Item::new(json!({ "i": i }))).collect()
}

#[test]
fn without_a_lane_count_every_item_gets_its_own_lane() {
    let lanes = split(&items(4), None);
    assert_eq!(lanes.len(), 4);
    assert!(lanes.iter().all(|lane| lane.len() == 1));
}

#[test]
fn a_lane_count_chunks_the_input_and_preserves_order() {
    let lanes = split(&items(7), Some(3));
    assert_eq!(lanes.len(), 3, "at most the requested number of lanes");
    let flattened: Vec<i64> = lanes
        .iter()
        .flatten()
        .filter_map(|item| item.json["i"].as_i64())
        .collect();
    assert_eq!(
        flattened,
        (0..7).collect::<Vec<i64>>(),
        "chunking must not reorder the work"
    );
}

/// Asking for more lanes than there are items yields one lane each, not a
/// pile of empty lanes a gather would then wait on forever.
#[test]
fn more_lanes_than_items_yields_one_lane_per_item() {
    let lanes = split(&items(2), Some(9));
    assert_eq!(lanes.len(), 2);
}

#[test]
fn an_empty_input_opens_no_lanes() {
    assert!(split(&[], None).is_empty());
    assert!(split(&[], Some(4)).is_empty());
}

#[test]
fn the_lane_count_is_clamped_to_the_ceiling() {
    let lanes = split(&items(MAX_LANES + 50), Some(MAX_LANES + 50));
    assert!(
        lanes.len() <= MAX_LANES,
        "opened {} lanes, above the ceiling",
        lanes.len()
    );
}

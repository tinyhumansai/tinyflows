use super::*;

fn envelope(lane: Value) -> Value {
    json!({ LANE_KEY: lane, "items": [] })
}

#[test]
fn a_well_formed_envelope_decodes() {
    let lane = lane_context(Some(&envelope(json!({
        "id": "fan#2", "origin": "fan", "index": 2, "count": 5
    }))))
    .expect("a complete envelope should decode");
    assert_eq!(lane.id, "fan#2");
    assert_eq!(lane.origin, "fan");
    assert_eq!(lane.index, 2);
    assert_eq!(lane.count, 5);
}

/// The ordinary case, and the only one that occurs until a fan-out node
/// exists: an activation scheduled by a plain route carries no arg at all.
#[test]
fn an_activation_without_a_send_arg_has_no_lane() {
    assert!(lane_context(None).is_none());
}

/// A `send_arg` that is not a lane envelope belongs to something else and
/// must not be read as a lane.
#[test]
fn a_send_arg_without_the_lane_key_has_no_lane() {
    assert!(lane_context(Some(&json!({ "items": [] }))).is_none());
}

/// Decoding is total: a malformed envelope degrades to "no lane" rather
/// than panicking or failing the run, so a bad packet cannot take the run
/// down. Each case drops or corrupts exactly one required field.
#[test]
fn a_malformed_envelope_degrades_to_no_lane() {
    for broken in [
        json!({ "origin": "fan", "index": 0, "count": 1 }), // no id
        json!({ "id": "fan#0", "index": 0, "count": 1 }),   // no origin
        json!({ "id": "fan#0", "origin": "fan", "count": 1 }), // no index
        json!({ "id": "fan#0", "origin": "fan", "index": 0 }), // no count
        json!({ "id": 7, "origin": "fan", "index": 0, "count": 1 }), // id not a string
        json!({ "id": "fan#0", "origin": "fan", "index": -1, "count": 1 }), // negative index
    ] {
        assert!(
            lane_context(Some(&envelope(broken.clone()))).is_none(),
            "malformed envelope should not decode: {broken}"
        );
    }
}

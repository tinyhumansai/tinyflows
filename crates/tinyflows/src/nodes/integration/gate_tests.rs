use super::*;
use serde_json::json;

#[test]
fn wait_mode_defaults_to_polling() {
    assert_eq!(WaitMode::from_config(&json!({})), WaitMode::Poll);
    assert_eq!(
        WaitMode::from_config(&json!({ "wait_mode": "suspend" })),
        WaitMode::Suspend
    );
    assert_eq!(
        WaitMode::from_config(&json!({ "wait_mode": "nonsense" })),
        WaitMode::Poll,
        "an unrecognised mode must not silently suspend the run"
    );
}

/// Failing closed: an unknown `on_timeout` must not be read as `partial`,
/// which would emit an incomplete result as though it were complete.
#[test]
fn on_timeout_defaults_to_error() {
    assert_eq!(OnTimeout::from_config(&json!({})), OnTimeout::Error);
    assert_eq!(
        OnTimeout::from_config(&json!({ "on_timeout": "eventually" })),
        OnTimeout::Error
    );
    assert_eq!(
        OnTimeout::from_config(&json!({ "on_timeout": "partial" })),
        OnTimeout::Partial
    );
}

/// Emission order follows the ticket list, not completion order — the
/// property that keeps a gate deterministic under any timing.
#[test]
fn results_are_emitted_in_ticket_order_regardless_of_arrival() {
    // Deliberately out of order, as a real race would deliver them.
    let results = vec![
        (2, json!("third")),
        (0, json!("first")),
        (1, json!("second")),
    ];
    let items = emit_items(results);
    let values: Vec<&Value> = items.iter().map(|item| &item.json).collect();
    assert_eq!(
        values,
        vec![&json!("first"), &json!("second"), &json!("third")]
    );
    assert_eq!(
        items.iter().map(|i| i.paired_item).collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(2)],
        "each result keeps the index of the ticket it came from"
    );
}

#[test]
fn positive_config_fields_fall_back_on_zero_or_garbage() {
    assert_eq!(positive_u64(&json!({}), "max_polls", 7), 7);
    assert_eq!(positive_u64(&json!({ "max_polls": 0 }), "max_polls", 7), 7);
    assert_eq!(
        positive_u64(&json!({ "max_polls": "x" }), "max_polls", 7),
        7
    );
    assert_eq!(positive_u64(&json!({ "max_polls": 3 }), "max_polls", 7), 3);
}

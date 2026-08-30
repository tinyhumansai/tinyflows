|state: &Value| -> (Vec<Item>, Value, Value) {
    // Which set of incoming edges this activation draws from. A node
    // with no back-edges always uses its forward edges. A loop head
    // uses its forward edges on the first activation (the seed) and
    // its back-edges on every re-entry, detected by whether it has
    // already recorded an output slot. Without this the seed's items
    // are re-delivered alongside the body's on every iteration.
    let re_entry = !back_incoming.is_empty()
        && state
            .get("nodes")
            .and_then(|nodes| nodes.get(&node.id))
            .is_some_and(|slot| !slot.is_null());
    // A lane activation carries its own work. It must not read
    // predecessor slots: every branch of a super-step sees the same
    // committed snapshot, so `collect_input` would hand all N lanes
    // the identical items.
    let input = if let Some(lane_arg) = lane_send_arg.as_ref() {
        lane_input(Some(lane_arg))
    } else if re_entry {
        let latest_step = back_incoming
            .iter()
            .filter_map(|(pred, _)| state["nodes"][pred]["_activation_step"].as_u64())
            .max();
        collect_input_since(state, &back_incoming, latest_step)
    } else {
        collect_input(state, &incoming)
    };
    let run_meta = state.get("run").cloned().unwrap_or(Value::Null);
    // Every completed node's output slot, keyed by id. Handed to the
    // executor so `=`-expressions can address any upstream node
    // (`nodes.<id>.item.<field>`), not just the direct predecessors
    // flattened into `input` — see `crate::nodes::expr_scope`.
    let nodes_state = state.get("nodes").cloned().unwrap_or(Value::Null);
    (input, run_meta, nodes_state)
}

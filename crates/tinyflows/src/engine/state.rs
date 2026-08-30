use super::*;

/// Reducer that deep-merges each node's partial `{ "nodes": { id: { items } } }`
/// update into the run state. Because every node writes under its own id, updates
/// from independent nodes never collide — this stays correct for A2 parallelism.
pub(super) struct MergeReducer;

impl StateReducer<Value, Value> for MergeReducer {
    fn apply(&self, mut state: Value, update: Value) -> crate::graph::Result<Value> {
        merge(&mut state, update);
        Ok(state)
    }
}

/// The sentinel key that makes an update *assign* rather than merge.
///
/// An update object shaped exactly `{"$replace": v}` sets its slot to `v`
/// wholesale.
pub(crate) const REPLACE: &str = "$replace";

/// Wraps `value` so [`merge`] assigns it instead of merging into what is there.
pub(crate) fn replace(value: Value) -> Value {
    json!({ REPLACE: value })
}

/// Recursively merges `update` into `base`: objects merge key-by-key; any other
/// value (array, scalar, null) overwrites; and an update of exactly
/// `{"$replace": v}` assigns `v` wholesale.
///
/// # Why the sentinel exists
///
/// Key-by-key merging means an object-valued slot can only ever *gain* keys. A
/// node that keeps state across its own activations — a `loop` node's
/// accumulator — could therefore never drop one: `{"attempts": [...], "err":
/// "x"}` has no way to become `{"attempts": [...]}`. Arrays and scalars already
/// overwrite, so this is specifically the object case, which is the interesting
/// one for an accumulator.
///
/// # Why user data cannot be mistaken for it
///
/// `merge` only ever recurses through the object-valued subtrees of an *update*,
/// and the only object subtrees an update contains are the root, `"nodes"`, each
/// node's slot, and the `meta` values a node records about itself. Item payloads
/// live inside `slot["items"]`, which is an **array** — it hits the overwrite arm
/// without being walked into. So a workflow whose data happens to contain a
/// `$replace` key is never examined by this function, and cannot trigger it.
pub(super) fn merge(base: &mut Value, update: Value) {
    // Checked before the object/object arm: the sentinel *is* an object, and
    // merging it key-by-key would write a literal `$replace` key into state.
    if let Value::Object(map) = &update
        && map.len() == 1
        && let Some(value) = map.get(REPLACE)
    {
        *base = value.clone();
        return;
    }
    match update {
        Value::Object(update) => {
            // Recurse even when this subtree does not exist yet. Assigning the
            // incoming object wholesale would leave any nested `$replace`
            // sentinel as literal state on its first write.
            if !base.is_object() {
                *base = Value::Object(Map::new());
            }
            let base = base.as_object_mut().expect("object created above");
            for (key, value) in update {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        update => *base = update,
    }
}

/// Builds a lane activation's state update.
///
/// A lane writes under `nodes.<id>.lanes.<lane id>` and **never** touches the
/// slot's top-level `items`/`port`. That is the whole reason N concurrent
/// activations of one node do not clobber each other: the reducer merges
/// objects key-by-key, so distinct lane keys are collision-free without the
/// reducer needing to know lanes exist.
///
/// This is the single writer of lane slots, deliberately — the "lanes never
/// write the top level" rule is structural, enforced by there being one
/// constructor, rather than by anything the engine checks at run time.
pub(super) fn lane_items_update(
    node_id: &str,
    lane: &crate::nodes::LaneContext,
    items: &[Item],
    port: Option<&str>,
    status: &str,
    meta: Option<&Value>,
) -> crate::graph::Result<Value> {
    let mut slot = json!({
        "items": serde_json::to_value(items)?,
        "port": port.map(Value::from).unwrap_or(Value::Null),
        "status": status,
        "index": lane.index,
    });
    if let (Some(Value::Object(extra)), Some(map)) = (meta, slot.as_object_mut()) {
        for (key, value) in extra {
            map.insert(key.clone(), value.clone());
        }
    }
    Ok(json!({ "nodes": { node_id: { "lanes": { lane.id.clone(): slot } } } }))
}

/// The lane envelope a fan-out schedules one activation with.
pub(super) fn lane_envelope(
    origin: &str,
    index: usize,
    count: usize,
    items: &[Item],
) -> crate::graph::Result<Value> {
    Ok(json!({
        LANE_KEY: {
            "id": format!("{origin}#{index}"),
            "origin": origin,
            "index": index,
            "count": count,
        },
        "items": serde_json::to_value(items)?,
    }))
}

/// Reads a lane activation's items back out of its envelope.
///
/// A lane takes its input from here rather than from [`collect_input`], and
/// must: every branch of a super-step reads the same committed snapshot, so
/// `collect_input` would hand all N lanes the identical items.
pub(super) fn lane_input(send_arg: Option<&Value>) -> Vec<Item> {
    send_arg
        .and_then(|arg| arg.get("items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<Item>(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The key a lane envelope records its lane identity under, inside the
/// `send_arg` a fan-out schedules each concurrent activation with.
///
/// Underscore-prefixed because it shares the envelope with the lane's `items`
/// and must never be mistaken for user data.
pub(crate) const LANE_KEY: &str = "_lane";

/// Decodes the lane identity from an activation's `send_arg`.
///
/// `None` for an ordinary activation — one scheduled by a plain route rather
/// than by a fan-out packet — which is every activation until a fan-out node
/// exists. A malformed envelope also yields `None` rather than failing the run:
/// the activation then behaves as an ordinary one, which is the safe reading.
pub(super) fn lane_context(send_arg: Option<&Value>) -> Option<crate::nodes::LaneContext> {
    let lane = send_arg?.get(LANE_KEY)?;
    Some(crate::nodes::LaneContext {
        id: lane.get("id")?.as_str()?.to_string(),
        origin: lane.get("origin")?.as_str()?.to_string(),
        index: usize::try_from(lane.get("index")?.as_u64()?).ok()?,
        count: usize::try_from(lane.get("count")?.as_u64()?).ok()?,
    })
}

/// Collects a node's input items from the `items` its predecessors emitted into
/// the run state, **honoring the port each edge is wired to**.
///
/// `incoming` is the node's incoming edges as `(predecessor id, edge from_port)`
/// pairs. For each edge, the predecessor's items are included only when the
/// predecessor actually emitted on that edge's `from_port` — the port it recorded
/// into its run-state slot (defaulting to `"main"` on both sides). This makes the
/// common linear / parallel-fan-out / merge case (everything on `"main"`) a
/// no-op, while preventing an untaken conditional branch (e.g. a `condition` that
/// took `"true"`) from leaking its data into a fan-in wired to a different port.
pub(super) fn collect_input(state: &Value, incoming: &[(String, String)]) -> Vec<Item> {
    collect_input_since(state, incoming, None)
}

/// Collects matching inputs, optionally ignoring predecessor slots older than
/// `min_step`. Loop re-entry uses this to avoid replaying an alternate return
/// arm whose slot was written by an earlier iteration.
pub(super) fn collect_input_since(
    state: &Value,
    incoming: &[(String, String)],
    min_step: Option<u64>,
) -> Vec<Item> {
    let mut items = Vec::new();
    for (pred, from_port) in incoming {
        let slot = state.get("nodes").and_then(|nodes| nodes.get(pred));
        if min_step.is_some_and(|minimum| {
            slot.and_then(|slot| slot.get("_activation_step"))
                .and_then(Value::as_u64)
                .is_none_or(|step| step < minimum)
        }) {
            continue;
        }
        // The port this predecessor actually emitted on (defaulting to `"main"`),
        // compared against the port the edge draws from (also `"main"` by
        // default). A mismatch means this edge's branch was not taken.
        let emitted = slot
            .and_then(|slot| slot.get("port"))
            .and_then(Value::as_str)
            .unwrap_or("main");
        if emitted != from_port.as_str() {
            continue;
        }
        if let Some(array) = slot
            .and_then(|slot| slot.get("items"))
            .and_then(Value::as_array)
        {
            for value in array {
                if let Ok(item) = serde_json::from_value::<Item>(value.clone()) {
                    items.push(item);
                }
            }
        }
    }
    items
}

/// Records the super-step that produced a node slot. This private marker lets
/// a loop head distinguish the return edge that activated it from stale slots
/// left by alternate return arms in earlier iterations.
pub(super) fn stamp_activation_step(update: &mut Value, node_id: &str, step: usize) {
    if let Some(slot) = update
        .get_mut("nodes")
        .and_then(Value::as_object_mut)
        .and_then(|nodes| nodes.get_mut(node_id))
        .and_then(Value::as_object_mut)
    {
        slot.insert("_activation_step".to_string(), json!(step));
    }
}

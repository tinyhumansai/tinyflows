//! Channel-per-field state model (additive).
//!
//! See [`types`] for the type definitions and the high-level model. This file
//! supplies the concrete [`Channel`] merge rules, the [`ChannelSet`] map
//! operations, and the [`ChannelState`] ⇒ [`StateReducer`] bridge that lets a
//! channel graph run on the existing executor.
//!
//! ## How a channel graph runs on the unchanged executor
//!
//! The executor folds a superstep's branch results one at a time:
//! `state = reducer.apply(state, update)` for each branch's
//! [`ChannelUpdate`]. [`ChannelState`] is its own reducer, so each `apply`
//! dispatches every write in the update to the owning channel's
//! [`Channel::merge`].
//!
//! ## Concurrent-write conflict detection
//!
//! When two fan-out branches write the *same* channel in *one* superstep, the
//! merge must decide whether that is legal:
//!
//! - **Aggregate channels** ([`Topic`], [`BinaryAggregate`], [`Delta`],
//!   [`Messages`], [`Barrier`], [`NamedBarrier`]) set
//!   [`Channel::allows_concurrent`] to `true`; both writes fold in
//!   deterministic active-set index order.
//! - **Overwrite channels** ([`LastValue`], [`Ephemeral`], [`Untracked`])
//!   return `false`; a second same-step write to such a channel raises
//!   [`GraphError::InvalidConcurrentUpdate`] because there is no
//!   deterministic winner.
//!
//! Because the executor applies a step's updates as a contiguous batch, "same
//! step" is tracked by stamping each [`ChannelUpdate`] with the node's
//! `ctx.step` via [`ChannelUpdate::at_step`]. When updates are stamped, the
//! reducer resets its per-step bookkeeping (and clears [`Ephemeral`] channels)
//! whenever the step number advances. Unstamped updates are each treated as
//! their own step (last-value writes always win, no conflict detection and no
//! ephemeral clearing) — so existing whole-state habits keep working and
//! conflict detection is strictly opt-in.

mod types;

pub use types::{
    Barrier, BinaryAggregate, Channel, ChannelSet, ChannelState, ChannelUpdate, Delta, Ephemeral,
    LastValue, Messages, NamedBarrier, Topic, Untracked,
};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::graph::error::{GraphError, Result};
use crate::graph::reducer::StateReducer;

// --- Channel merge rules ---

impl Channel for LastValue {
    fn kind(&self) -> &'static str {
        "last_value"
    }

    fn merge(&self, _current: Option<Value>, incoming: Value) -> Result<Value> {
        Ok(incoming)
    }

    fn clone_box(&self) -> Box<dyn Channel> {
        Box::new(*self)
    }
}

impl Channel for Topic {
    fn kind(&self) -> &'static str {
        "topic"
    }

    fn merge(&self, current: Option<Value>, incoming: Value) -> Result<Value> {
        // Reuse the existing array in place instead of cloning it per merge.
        let mut list = match current {
            Some(Value::Array(items)) => items,
            Some(other) => vec![other],
            None => Vec::new(),
        };
        match incoming {
            Value::Array(items) => list.extend(items),
            other => list.push(other),
        }
        Ok(Value::Array(list))
    }

    fn allows_concurrent(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn Channel> {
        Box::new(*self)
    }
}

impl Channel for Delta {
    fn kind(&self) -> &'static str {
        "delta"
    }

    fn merge(&self, current: Option<Value>, incoming: Value) -> Result<Value> {
        let add_err = || GraphError::Graph("Delta channel only accepts numeric writes".to_string());
        let incoming_num = incoming.as_f64().ok_or_else(add_err)?;
        let Some(current) = current else {
            return Ok(incoming);
        };
        let current_num = current.as_f64().ok_or_else(add_err)?;

        // Stay in integer space when both operands are integers.
        if current.is_i64() && incoming.is_i64() {
            let sum = current.as_i64().unwrap() + incoming.as_i64().unwrap();
            return Ok(Value::from(sum));
        }
        Ok(Value::from(current_num + incoming_num))
    }

    fn allows_concurrent(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn Channel> {
        Box::new(*self)
    }
}

impl Channel for Messages {
    fn kind(&self) -> &'static str {
        "messages"
    }

    fn merge(&self, current: Option<Value>, incoming: Value) -> Result<Value> {
        // Reuse the existing array in place instead of cloning it per merge.
        let mut list = match current {
            Some(Value::Array(items)) => items,
            Some(_) => {
                return Err(GraphError::Graph(
                    "Messages channel value must be a JSON array".to_string(),
                ));
            }
            None => Vec::new(),
        };
        let incoming = match incoming {
            Value::Array(items) => items,
            other => vec![other],
        };
        // Build an id -> index map over the existing list once (O(existing)) so
        // each incoming message is an O(1) lookup instead of a linear scan.
        // Previously this dedup was O(existing x incoming), which bit at a few
        // thousand messages.
        let mut index: HashMap<String, usize> = list
            .iter()
            .enumerate()
            .filter_map(|(i, existing)| {
                existing
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_string(), i))
            })
            .collect();
        for msg in incoming {
            match msg.get("id").and_then(Value::as_str).map(str::to_string) {
                // Keyed message: replace the same id in place, or append and
                // remember its position for later incoming writes.
                Some(id) => match index.get(&id) {
                    Some(&i) => list[i] = msg,
                    None => {
                        index.insert(id, list.len());
                        list.push(msg);
                    }
                },
                // Unkeyed message: always appended (unchanged behavior).
                None => list.push(msg),
            }
        }
        Ok(Value::Array(list))
    }

    fn allows_concurrent(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn Channel> {
        Box::new(*self)
    }
}

impl Channel for Ephemeral {
    fn kind(&self) -> &'static str {
        "ephemeral"
    }

    fn merge(&self, _current: Option<Value>, incoming: Value) -> Result<Value> {
        Ok(incoming)
    }

    fn is_ephemeral(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn Channel> {
        Box::new(*self)
    }
}

impl Channel for Untracked {
    fn kind(&self) -> &'static str {
        "untracked"
    }

    fn merge(&self, _current: Option<Value>, incoming: Value) -> Result<Value> {
        Ok(incoming)
    }

    fn is_tracked(&self) -> bool {
        false
    }

    fn clone_box(&self) -> Box<dyn Channel> {
        Box::new(*self)
    }
}

impl Barrier {
    /// Creates a count-based barrier that is ready after `expected` arrivals.
    pub fn new(expected: usize) -> Self {
        Self { expected }
    }
}

impl Channel for Barrier {
    fn kind(&self) -> &'static str {
        "barrier"
    }

    fn merge(&self, current: Option<Value>, incoming: Value) -> Result<Value> {
        // Reuse the accumulated array in place instead of cloning it per merge.
        let mut list = match current {
            Some(Value::Array(items)) => items,
            Some(other) => vec![other],
            None => Vec::new(),
        };
        match incoming {
            Value::Array(items) => list.extend(items),
            other => list.push(other),
        }
        Ok(Value::Array(list))
    }

    fn allows_concurrent(&self) -> bool {
        true
    }

    fn is_ready(&self, current: Option<&Value>) -> bool {
        current
            .and_then(Value::as_array)
            .map(|items| items.len() >= self.expected)
            .unwrap_or(self.expected == 0)
    }

    fn clone_box(&self) -> Box<dyn Channel> {
        Box::new(*self)
    }
}

impl NamedBarrier {
    /// Creates a name-based barrier that is ready once every name has arrived.
    pub fn new(expected: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            expected: expected.into_iter().map(Into::into).collect(),
        }
    }
}

impl Channel for NamedBarrier {
    fn kind(&self) -> &'static str {
        "named_barrier"
    }

    fn merge(&self, current: Option<Value>, incoming: Value) -> Result<Value> {
        // Reuse the accumulated object in place instead of cloning it per merge.
        let mut map = match current {
            Some(Value::Object(map)) => map,
            Some(_) => {
                return Err(GraphError::Graph(
                    "NamedBarrier channel value must be a JSON object".to_string(),
                ));
            }
            None => serde_json::Map::new(),
        };
        let Value::Object(incoming) = incoming else {
            return Err(GraphError::Graph(
                "NamedBarrier writes must be JSON objects of named arrivals".to_string(),
            ));
        };
        for (key, value) in incoming {
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }

    fn allows_concurrent(&self) -> bool {
        true
    }

    fn is_ready(&self, current: Option<&Value>) -> bool {
        let Some(Value::Object(map)) = current else {
            return self.expected.is_empty();
        };
        self.expected.iter().all(|name| map.contains_key(name))
    }

    fn clone_box(&self) -> Box<dyn Channel> {
        Box::new(self.clone())
    }
}

impl BinaryAggregate {
    /// Creates an aggregate channel from a binary fold closure. The first write
    /// becomes the value directly; later writes are `fold(current, incoming)`.
    pub fn new<F>(fold: F) -> Self
    where
        F: Fn(Value, Value) -> Result<Value> + Send + Sync + 'static,
    {
        Self {
            fold: Arc::new(fold),
        }
    }

    /// Builds an aggregate channel from a [`crate::graph::Reducer<Value>`].
    pub fn from_reducer<R>(reducer: R) -> Self
    where
        R: crate::graph::Reducer<Value> + 'static,
    {
        Self::new(move |current, incoming| reducer.reduce(current, incoming))
    }
}

impl Channel for BinaryAggregate {
    fn kind(&self) -> &'static str {
        "binary_aggregate"
    }

    fn merge(&self, current: Option<Value>, incoming: Value) -> Result<Value> {
        match current {
            Some(current) => (self.fold)(current, incoming),
            None => Ok(incoming),
        }
    }

    fn allows_concurrent(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn Channel> {
        Box::new(self.clone())
    }
}

// --- ChannelSet ---

mod state;

#[cfg(test)]
#[path = "channel_tests.rs"]
mod test;

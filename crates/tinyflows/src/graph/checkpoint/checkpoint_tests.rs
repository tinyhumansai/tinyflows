//! Unit tests for the in-memory checkpointer: `put`/`get`/`list` roundtrips
//! (including latest-vs-specific lookup and missing threads) and the shared
//! storage guarantee across cheap clones.

use super::*;
use crate::graph::ids::NodeId;
use serde_json::json;

fn checkpoint(thread: &str, id: &str, parent: Option<&str>, step: usize) -> Checkpoint<i32> {
    Checkpoint {
        thread_id: thread.to_string(),
        checkpoint_id: id.to_string(),
        run_id: None,
        parent_checkpoint_id: parent.map(|s| s.to_string()),
        namespace: vec![],
        state: step as i32,
        next_nodes: vec![NodeId::from("n")],
        completed_tasks: vec![],
        pending_writes: vec![],
        interrupts: vec![],
        pending_activations: None,
        barrier_arrivals: vec![],
        metadata: json!({ "source": "loop", "step": step }),
    }
}

include!("checkpoint_tests/checkpoint_tests_part_01_tests.rs");
include!("checkpoint_tests/checkpoint_tests_part_02_tests.rs");

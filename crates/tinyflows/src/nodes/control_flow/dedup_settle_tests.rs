use super::*;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
struct MemKv {
    map: Mutex<HashMap<String, Value>>,
    fail_set: Option<&'static str>,
    fail_get: Option<&'static str>,
}

impl DedupKv for MemKv {
    fn kv_get(&self, key: &str) -> Result<Option<Value>, String> {
        if self.fail_get == Some(key) {
            return Err("read timeout".into());
        }
        Ok(self.map.lock().unwrap().get(key).cloned())
    }
    fn kv_set(&self, key: &str, value: &Value) -> Result<(), String> {
        if self.fail_set == Some(key) {
            return Err("disk full".into());
        }
        self.map.lock().unwrap().insert(key.into(), value.clone());
        Ok(())
    }
    fn kv_delete(&self, key: &str) -> Result<(), String> {
        self.map.lock().unwrap().remove(key);
        Ok(())
    }
}

fn seeded() -> MemKv {
    let kv = MemKv::default();
    kv.map
        .lock()
        .unwrap()
        .insert("dedup:n1:tentative".into(), json!(["b", "c"]));
    kv.map
        .lock()
        .unwrap()
        .insert("dedup:n1:committed".into(), json!(["a", "b"]));
    kv
}

#[test]
fn success_unions_tentative_into_committed_and_clears_tentative() {
    let kv = seeded();
    let out = settle(&kv, "n1", None, true);
    assert_eq!(
        out,
        Settlement::Commit(CommitOutcome::Committed {
            added: 1,
            committed_len: 3,
            clear_error: None
        })
    );
    let map = kv.map.lock().unwrap();
    assert_eq!(map["dedup:n1:committed"], json!(["a", "b", "c"]));
    assert!(!map.contains_key("dedup:n1:tentative"));
}

#[test]
fn failure_releases_tentative_and_leaves_committed() {
    let kv = seeded();
    assert_eq!(settle(&kv, "n1", None, false), Settlement::Release(Ok(())));
    let map = kv.map.lock().unwrap();
    assert_eq!(map["dedup:n1:committed"], json!(["a", "b"]));
    assert!(!map.contains_key("dedup:n1:tentative"));
}

#[test]
fn nothing_tentative_is_a_no_op() {
    let kv = MemKv::default();
    assert_eq!(commit(&kv, "n1", None), CommitOutcome::NothingTentative);
    assert!(kv.map.lock().unwrap().is_empty());
}

#[test]
fn failed_committed_write_keeps_tentative_for_retry() {
    let mut kv = seeded();
    kv.fail_set = Some("dedup:n1:committed");
    assert_eq!(
        commit(&kv, "n1", None),
        CommitOutcome::CommitFailed("disk full".into())
    );
    assert_eq!(
        kv.map.lock().unwrap()["dedup:n1:tentative"],
        json!(["b", "c"])
    );
}

#[test]
fn only_completed_statuses_count_as_success() {
    assert!(is_success_status("completed"));
    assert!(is_success_status("completed_with_warnings"));
    for s in [
        "failed",
        "cancelled",
        "interrupted",
        "",
        "COMPLETED",
        "weird",
    ] {
        assert!(!is_success_status(s), "{s}");
    }
}

#[test]
fn malformed_stored_sets_degrade_to_empty() {
    let kv = MemKv::default();
    kv.map
        .lock()
        .unwrap()
        .insert("dedup:n1:tentative".into(), json!(["x", 1, null]));
    kv.map
        .lock()
        .unwrap()
        .insert("dedup:n1:committed".into(), json!("not-an-array"));
    assert!(matches!(
        commit(&kv, "n1", None),
        CommitOutcome::Committed {
            added: 1,
            committed_len: 1,
            ..
        }
    ));
    assert_eq!(kv.map.lock().unwrap()["dedup:n1:committed"], json!(["x"]));
}

#[test]
fn failed_committed_read_aborts_without_touching_either_set() {
    let mut kv = seeded();
    kv.fail_get = Some("dedup:n1:committed");
    assert_eq!(
        commit(&kv, "n1", None),
        CommitOutcome::CommitFailed("read timeout".into())
    );
    let map = kv.map.lock().unwrap();
    assert_eq!(map["dedup:n1:committed"], json!(["a", "b"]));
    assert_eq!(map["dedup:n1:tentative"], json!(["b", "c"]));
}

#[test]
fn failed_tentative_read_aborts_the_commit() {
    let mut kv = seeded();
    kv.fail_get = Some("dedup:n1:tentative");
    assert_eq!(
        commit(&kv, "n1", None),
        CommitOutcome::CommitFailed("read timeout".into())
    );
    assert_eq!(
        kv.map.lock().unwrap()["dedup:n1:committed"],
        json!(["a", "b"])
    );
}

#[test]
fn settling_one_run_leaves_an_overlapping_runs_tentative_set_alone() {
    let kv = MemKv::default();
    kv.map
        .lock()
        .unwrap()
        .insert(tentative_key("n1", Some("r1")), json!(["a"]));
    kv.map
        .lock()
        .unwrap()
        .insert(tentative_key("n1", Some("r2")), json!(["b"]));

    // r2 finishes first and fails; r1 then succeeds.
    assert_eq!(
        settle(&kv, "n1", Some("r2"), false),
        Settlement::Release(Ok(()))
    );
    assert!(matches!(
        settle(&kv, "n1", Some("r1"), true),
        Settlement::Commit(CommitOutcome::Committed { added: 1, .. })
    ));

    let map = kv.map.lock().unwrap();
    assert_eq!(
        map.get("dedup:n1:committed"),
        Some(&json!(["a"])),
        "r2's failed item is not committed by r1's success"
    );
    assert!(!map.contains_key(&tentative_key("n1", Some("r1"))));
    assert!(!map.contains_key(&tentative_key("n1", Some("r2"))));
}

#[test]
fn a_failed_run_scoped_commit_is_retried_by_the_next_successful_run() {
    let mut kv = MemKv::default();
    kv.map
        .lock()
        .unwrap()
        .insert(tentative_key("n1", Some("r1")), json!(["a"]));
    kv.fail_set = Some("dedup:n1:committed");
    assert_eq!(
        commit(&kv, "n1", Some("r1")),
        CommitOutcome::CommitFailed("disk full".into())
    );
    assert_eq!(kv.map.lock().unwrap()["dedup:n1:uncommitted"], json!(["a"]));

    kv.fail_set = None;
    kv.map
        .lock()
        .unwrap()
        .insert(tentative_key("n1", Some("r2")), json!(["b"]));
    assert!(matches!(
        commit(&kv, "n1", Some("r2")),
        CommitOutcome::Committed { added: 2, .. }
    ));
    let map = kv.map.lock().unwrap();
    assert_eq!(map["dedup:n1:committed"], json!(["a", "b"]));
    assert!(!map.contains_key("dedup:n1:uncommitted"));
    assert!(!map.contains_key(&tentative_key("n1", Some("r1"))));
    assert!(!map.contains_key(&tentative_key("n1", Some("r2"))));
}

#[test]
fn an_unreadable_carryover_neither_blocks_a_commit_nor_is_overwritten() {
    let mut kv = MemKv::default();
    kv.map
        .lock()
        .unwrap()
        .insert("dedup:n1:uncommitted".into(), json!(["x"]));
    kv.map
        .lock()
        .unwrap()
        .insert(tentative_key("n1", Some("r1")), json!(["a"]));
    kv.fail_get = Some("dedup:n1:uncommitted");
    assert!(matches!(
        commit(&kv, "n1", Some("r1")),
        CommitOutcome::Committed { added: 1, .. }
    ));
    let map = kv.map.lock().unwrap();
    assert_eq!(map["dedup:n1:committed"], json!(["a"]));
    assert_eq!(map["dedup:n1:uncommitted"], json!(["x"]));
}

#[test]
fn an_unscoped_commit_folds_in_keys_a_scoped_commit_parked() {
    let mut kv = MemKv::default();
    kv.map
        .lock()
        .unwrap()
        .insert("dedup:n1:uncommitted".into(), json!(["a"]));
    kv.map
        .lock()
        .unwrap()
        .insert(tentative_key("n1", None), json!(["b"]));

    // A failed unscoped commit keeps the shared set and the carryover as is.
    kv.fail_set = Some("dedup:n1:committed");
    assert_eq!(
        commit(&kv, "n1", None),
        CommitOutcome::CommitFailed("disk full".into())
    );
    assert_eq!(kv.map.lock().unwrap()["dedup:n1:uncommitted"], json!(["a"]));
    assert_eq!(
        kv.map.lock().unwrap()[&tentative_key("n1", None)],
        json!(["b"])
    );

    kv.fail_set = None;
    assert!(matches!(
        commit(&kv, "n1", None),
        CommitOutcome::Committed { added: 2, .. }
    ));
    let map = kv.map.lock().unwrap();
    assert_eq!(map["dedup:n1:committed"], json!(["a", "b"]));
    assert!(!map.contains_key("dedup:n1:uncommitted"));
    assert!(!map.contains_key(&tentative_key("n1", None)));
}

//! Tests for proposals on disk.
//!
//! A proposal is the one artifact in this feature that can change a saved
//! graph, so what is asserted here is mostly about *not* doing that by
//! accident: a proposal that is not applicable, a listing that cannot leak
//! another workflow's proposals, a file that survives a decision.

use super::*;
use crate::store::types::{ProposalStatus, ProposalVerification};
use serde_json::json;

fn proposal(workflow_id: &str, created_at: u64) -> WorkflowProposal {
    WorkflowProposal {
        id: mint_id(created_at),
        workflow_id: workflow_id.to_string(),
        created_at,
        rationale: "the timeout is too short for a cold cache".into(),
        ops: json!([{ "op": "update_node_config", "id": "build", "config": { "timeout": 600 } }]),
        evidence_runs: vec!["run-1".into()],
        note_ids: Vec::new(),
        base_fingerprint: "abc123".into(),
        verification: None,
        status: ProposalStatus::Pending,
        decided_at: None,
        decision_reason: None,
    }
}

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temp dir")
}

#[test]
fn a_proposal_round_trips_through_disk() {
    let home = dir();
    let written = proposal("sweep", 1);
    save(home.path(), &written).expect("save");

    let read_back = read(home.path(), &written.id)
        .expect("read")
        .expect("the proposal is there");

    assert_eq!(read_back, written);
}

#[test]
fn an_unknown_proposal_is_none_rather_than_an_error() {
    let home = dir();
    assert!(
        read(home.path(), "no-such-proposal")
            .expect("read")
            .is_none()
    );
}

#[test]
fn listing_is_scoped_to_one_workflow() {
    let home = dir();
    save(home.path(), &proposal("sweep", 1)).expect("save");
    save(home.path(), &proposal("sweep", 2)).expect("save");
    save(home.path(), &proposal("deploy", 3)).expect("save");

    assert_eq!(list_for(home.path(), "sweep").expect("list").len(), 2);
    assert_eq!(list_for(home.path(), "deploy").expect("list").len(), 1);
    assert!(list_for(home.path(), "unrelated").expect("list").is_empty());
}

#[test]
fn proposals_come_back_newest_first() {
    let home = dir();
    let first = proposal("sweep", 1);
    let second = proposal("sweep", 2);
    save(home.path(), &first).expect("save");
    save(home.path(), &second).expect("save");

    let listed = list_for(home.path(), "sweep").expect("list");

    assert_eq!(listed[0].id, second.id);
    assert_eq!(listed[1].id, first.id);
}

#[test]
fn a_decision_replaces_the_file_rather_than_adding_one() {
    let home = dir();
    let mut written = proposal("sweep", 1);
    save(home.path(), &written).expect("save");

    written.status = ProposalStatus::Rejected;
    written.decided_at = Some(9);
    written.decision_reason = Some("the cache is the real problem".into());
    save(home.path(), &written).expect("save the decision");

    let listed = list_for(home.path(), "sweep").expect("list");
    assert_eq!(
        listed.len(),
        1,
        "a decision is a state change, not a new row"
    );
    assert_eq!(listed[0].status, ProposalStatus::Rejected);
    assert_eq!(
        listed[0].decision_reason.as_deref(),
        Some("the cache is the real problem")
    );
}

#[test]
fn only_a_verified_pending_proposal_is_applicable() {
    let mut subject = proposal("sweep", 1);
    assert!(
        !subject.is_applicable(),
        "an unverified proposal must not be offered"
    );

    subject.verification = Some(ProposalVerification {
        ok: false,
        verified_at: 2,
        messages: vec!["node 'build' does not exist".into()],
        diagnosis: None,
    });
    assert!(
        !subject.is_applicable(),
        "a proposal that failed its check is evidence, not an offer"
    );

    subject.verification = Some(ProposalVerification {
        ok: true,
        verified_at: 3,
        messages: Vec::new(),
        diagnosis: None,
    });
    assert!(subject.is_applicable());

    subject.status = ProposalStatus::Accepted;
    assert!(
        !subject.is_applicable(),
        "a proposal cannot be applied twice"
    );
}

#[test]
fn an_unreadable_proposal_is_skipped_rather_than_failing_the_listing() {
    let home = dir();
    save(home.path(), &proposal("sweep", 1)).expect("save");
    std::fs::write(home.path().join("broken.json"), b"not json at all").expect("write junk");

    let listed = list_for(home.path(), "sweep").expect("one bad file is not a failure");

    assert_eq!(listed.len(), 1);
}

#[test]
fn a_proposal_id_that_is_not_a_filename_is_refused() {
    let home = dir();
    let mut escaping = proposal("sweep", 1);
    escaping.id = "../../escape".into();

    assert!(save(home.path(), &escaping).is_err());
    assert!(read(home.path(), "../../escape").is_err());
}

#[test]
fn ops_survive_as_the_json_they_arrived_as() {
    // The reason `ops` is a raw `Value`: a stored proposal has to stay readable
    // even if the engine's op enum changes shape under it.
    let home = dir();
    let mut written = proposal("sweep", 1);
    written.ops = json!([{ "op": "some_future_op", "wholly": { "unknown": ["shape"] } }]);
    save(home.path(), &written).expect("save");

    let read_back = read(home.path(), &written.id)
        .expect("read")
        .expect("the proposal is there");

    assert_eq!(read_back.ops, written.ops);
}

#[test]
fn fingerprints_distinguish_graphs_and_agree_with_themselves() {
    use crate::store::types::fingerprint;

    let graph: crate::model::WorkflowGraph = serde_json::from_value(json!({
        "nodes": [{ "id": "t", "kind": "trigger", "name": "start" }],
        "edges": []
    }))
    .expect("the fixture graph should parse");
    let changed: crate::model::WorkflowGraph = serde_json::from_value(json!({
        "nodes": [{ "id": "t", "kind": "trigger", "name": "start again" }],
        "edges": []
    }))
    .expect("the fixture graph should parse");

    assert_eq!(fingerprint(&graph), fingerprint(&graph));
    assert_ne!(
        fingerprint(&graph),
        fingerprint(&changed),
        "a graph that moved must not look unchanged to an accept"
    );
}

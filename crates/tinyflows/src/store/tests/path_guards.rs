//! An id that would escape the workflow or run directory is refused, whether
//! it names a workflow, a save target, or a run record — none of these ids are
//! any more trusted than input from a peer.

use super::*;

#[test]
fn an_id_that_would_escape_the_workflow_directory_is_refused() {
    // A document's own `id` overrides whatever the caller asked for, and a
    // document may have been written by an agent — so this is the guard that
    // stops a save from writing outside the store with the daemon's rights.
    for hostile in [
        "../escape",
        "../../etc/authorized_keys",
        "sub/dir",
        "back\\slash",
        "..",
        ".",
        "   ",
        "/absolute",
    ] {
        assert!(
            super::super::file::safe_component(hostile).is_err(),
            "{hostile:?} should be refused"
        );
    }
    // Ordinary ids, including ones with dots, still work.
    for ordinary in ["sweep", "nightly-sweep", "a.b", "review_and_fix"] {
        assert!(
            super::super::file::safe_component(ordinary).is_ok(),
            "{ordinary:?} should be allowed"
        );
    }
}

#[test]
fn saving_a_workflow_whose_id_escapes_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let mut record = parse_workflow(&valid_document("ok"), "ok").unwrap();
    record.id = "../escaped".into();

    let err = store.save(&record).expect_err("must refuse");

    assert!(matches!(err, WorkflowError::Malformed(_)), "got {err:?}");
    assert!(
        !root.path().join("escaped.json").exists(),
        "nothing may be written outside the workflow directory"
    );
}

#[test]
fn a_run_id_that_escapes_is_refused_too() {
    // Run ids arrive on task frames from peers, so they are no more trusted
    // than a workflow id.
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());

    let err = store
        .record_run(&new_run_record("../escaped", "alpha", 1))
        .expect_err("must refuse");

    assert!(matches!(err, WorkflowError::Malformed(_)), "got {err:?}");
    assert!(!root.path().join("escaped.json").exists());
}

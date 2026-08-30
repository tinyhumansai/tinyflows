//! Tests for the on-disk journal.
//!
//! The behaviour worth pinning here is what happens when things go wrong: a
//! journal that cannot be parsed, an id that is not a filename, a cap reached
//! by automation. The happy path is a JSON array; the failure paths are where
//! a workflow either keeps working or stops.

use std::path::Path;

use super::*;
use crate::store::types::{NoteKind, NoteSource, WorkflowNote};

fn note(workflow_id: &str, recorded_at: u64, text: &str) -> WorkflowNote {
    WorkflowNote {
        id: mint_id(recorded_at),
        workflow_id: workflow_id.to_string(),
        kind: NoteKind::Observation,
        text: text.to_string(),
        recorded_at,
        source: NoteSource::System,
        run_ids: Vec::new(),
        superseded_by: None,
        pinned: false,
    }
}

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temp dir")
}

#[test]
fn notes_come_back_newest_first() {
    let home = dir();
    for (at, text) in [(1, "first"), (2, "second"), (3, "third")] {
        append(home.path(), &note("sweep", at, text)).expect("append");
    }

    let listed = list(home.path(), "sweep").expect("list");

    let texts: Vec<&str> = listed.iter().map(|n| n.text.as_str()).collect();
    assert_eq!(texts, ["third", "second", "first"]);
}

#[test]
fn a_workflow_with_no_journal_has_no_notes_rather_than_an_error() {
    let home = dir();
    assert!(
        list(home.path(), "never-written")
            .expect("a missing journal is the normal state")
            .is_empty()
    );
}

#[test]
fn journals_are_kept_apart_by_workflow() {
    let home = dir();
    append(home.path(), &note("sweep", 1, "about sweep")).expect("append");
    append(home.path(), &note("deploy", 1, "about deploy")).expect("append");

    assert_eq!(list(home.path(), "sweep").expect("list").len(), 1);
    assert_eq!(
        list(home.path(), "deploy").expect("list")[0].text,
        "about deploy"
    );
}

#[test]
fn superseding_marks_the_note_without_removing_it() {
    let home = dir();
    let first = note("sweep", 1, "the timeout is too short");
    append(home.path(), &first).expect("append");
    let second = note("sweep", 2, "the timeout was never the problem");
    append(home.path(), &second).expect("append");

    supersede(home.path(), "sweep", &first.id, &second.id).expect("supersede");

    let listed = list(home.path(), "sweep").expect("list");
    assert_eq!(listed.len(), 2, "history keeps the superseded note");
    let superseded = listed
        .iter()
        .find(|n| n.id == first.id)
        .expect("the superseded note is still listed");
    assert_eq!(
        superseded.superseded_by.as_deref(),
        Some(second.id.as_str())
    );
    assert!(
        !superseded.is_current(),
        "a superseded note must stay out of briefs"
    );
}

#[test]
fn superseding_a_note_that_is_not_there_is_not_a_failure() {
    let home = dir();
    append(home.path(), &note("sweep", 1, "something")).expect("append");

    // A caller naming a note that has already been pruned has nothing left to
    // fix, and failing here would turn tidying into an error path.
    assert!(
        !supersede(home.path(), "sweep", "no-such-note", "whatever")
            .expect("supersede is forgiving"),
        "a missing predecessor was not superseded"
    );
}

#[test]
fn a_workflow_id_that_is_not_a_filename_is_refused() {
    let home = dir();
    let escaping = note("../../etc/passwd", 1, "nope");

    assert!(
        append(home.path(), &escaping).is_err(),
        "an id that escapes the journal directory must not be written"
    );
    assert!(list(home.path(), "../../etc/passwd").is_err());
}

#[test]
fn an_unreadable_journal_reads_as_empty_rather_than_failing() {
    let home = dir();
    append(home.path(), &note("sweep", 1, "something")).expect("append");
    std::fs::write(home.path().join("sweep.json"), b"{ this is not json").expect("corrupt it");

    // One bad file must not make the workflow unreadable everywhere its notes
    // are shown — the same bargain run history already makes.
    assert!(
        list(home.path(), "sweep")
            .expect("a corrupt journal is not an error")
            .is_empty()
    );
}

#[test]
fn repeated_corruption_preserves_each_quarantined_journal() {
    let home = dir();
    for body in [b"{ first corruption".as_slice(), b"{ second corruption"] {
        std::fs::write(home.path().join("sweep.json"), body).expect("corrupt it");
        assert!(
            list(home.path(), "sweep")
                .expect("a corrupt journal is not an error")
                .is_empty()
        );
    }

    let quarantined: Vec<_> = std::fs::read_dir(home.path())
        .expect("journal directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("sweep.json.corrupt.")
        })
        .collect();
    assert_eq!(quarantined.len(), 2);
}

#[test]
fn the_journal_is_capped_and_drops_the_oldest_first() {
    let home = dir();
    for at in 0..(MAX_NOTES as u64 + 5) {
        append(home.path(), &note("sweep", at, &format!("note {at}"))).expect("append");
    }

    let listed = list(home.path(), "sweep").expect("list");

    assert_eq!(listed.len(), MAX_NOTES);
    assert_eq!(
        listed.last().expect("a note").text,
        "note 5",
        "the five oldest went, not the five newest"
    );
}

#[test]
fn pinned_notes_survive_the_cap() {
    let home = dir();
    let mut pinned = note("sweep", 0, "what the operator said");
    pinned.pinned = true;
    append(home.path(), &pinned).expect("append");
    for at in 1..(MAX_NOTES as u64 + 20) {
        append(home.path(), &note("sweep", at, &format!("note {at}"))).expect("append");
    }

    let listed = list(home.path(), "sweep").expect("list");

    assert_eq!(listed.len(), MAX_NOTES);
    assert!(
        listed.iter().any(|n| n.id == pinned.id),
        "automation writing a hundred observations must not evict a person's note"
    );
}

#[test]
fn an_old_supersession_chain_is_evicted_together_at_the_cap() {
    let home = dir();
    let first = note("sweep", 0, "obsolete");
    let replacement = note("sweep", 1, "current");
    append(home.path(), &first).expect("append predecessor");
    append(home.path(), &replacement).expect("append replacement");
    supersede(home.path(), "sweep", &first.id, &replacement.id).expect("supersede");
    for at in 2..(MAX_NOTES as u64 + 20) {
        append(home.path(), &note("sweep", at, &format!("note {at}"))).expect("append");
    }

    let listed = list(home.path(), "sweep").expect("list");
    assert_eq!(listed.len(), MAX_NOTES);
    assert!(!listed.iter().any(|note| note.id == first.id));
    assert!(!listed.iter().any(|note| note.id == replacement.id));
    assert!(listed.iter().all(|note| {
        note.superseded_by
            .as_ref()
            .is_none_or(|id| listed.iter().any(|replacement| &replacement.id == id))
    }));
}

#[test]
fn a_recent_replacement_survives_with_its_superseded_predecessor() {
    let home = dir();
    for at in 0..MAX_NOTES as u64 {
        append(home.path(), &note("sweep", at, &format!("note {at}"))).expect("append");
    }
    let first = note("sweep", MAX_NOTES as u64, "obsolete");
    let replacement = note("sweep", MAX_NOTES as u64 + 1, "current");
    append(home.path(), &first).expect("append predecessor");
    append(home.path(), &replacement).expect("append replacement");
    supersede(home.path(), "sweep", &first.id, &replacement.id).expect("supersede");

    let listed = list(home.path(), "sweep").expect("list");
    assert_eq!(listed.len(), MAX_NOTES);
    assert!(listed.iter().any(|note| note.id == first.id));
    assert!(listed.iter().any(|note| note.id == replacement.id));
}

#[test]
fn concurrent_appenders_do_not_overwrite_each_other() {
    let home = dir();
    let journal_dir = std::sync::Arc::new(home.path().to_path_buf());
    let gate = std::sync::Arc::new(std::sync::Barrier::new(16));
    let writers: Vec<_> = (0..16)
        .map(|at| {
            let journal_dir = journal_dir.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                append(&journal_dir, &note("sweep", at, &format!("note {at}"))).expect("append");
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer completed");
    }

    assert_eq!(list(&journal_dir, "sweep").expect("list").len(), 16);
}

#[test]
fn ids_minted_in_the_same_millisecond_still_sort_in_order() {
    // A pass writes several notes at once; without the counter their order
    // would fall through to a random token and the listing would be arbitrary.
    let ids: Vec<String> = (0..8).map(|_| mint_id(1_700_000_000_000)).collect();
    let mut sorted = ids.clone();
    sorted.sort();

    assert_eq!(ids, sorted);
}

#[test]
fn a_note_round_trips_every_field_through_disk() {
    let home = dir();
    let written = WorkflowNote {
        id: mint_id(7),
        workflow_id: "sweep".into(),
        kind: NoteKind::Constraint,
        text: "the deploy step must never run before tests".into(),
        recorded_at: 7,
        source: NoteSource::Agent {
            model: Some("claude-opus-5".into()),
        },
        run_ids: vec!["run-1".into(), "run-2".into()],
        superseded_by: None,
        pinned: true,
    };
    append(home.path(), &written).expect("append");

    let read_back = list(home.path(), "sweep").expect("list").remove(0);

    assert_eq!(read_back, written);
}

/// The journal directory is created on demand, like every other store path.
#[test]
fn appending_creates_the_directory() {
    let home = dir();
    let nested = home.path().join("state").join("workflows").join("journal");
    assert!(!Path::new(&nested).exists());

    append(&nested, &note("sweep", 1, "first")).expect("append");

    assert_eq!(list(&nested, "sweep").expect("list").len(), 1);
}

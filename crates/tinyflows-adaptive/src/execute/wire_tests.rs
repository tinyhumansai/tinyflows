//! Tests for [`super`] — the adaptive execution wire form.
//!
//! In their own file per the repository's rule that Rust tests live in
//! `_tests.rs` files rather than as inline modules in production source.

use super::*;
use tinyflows::evidence::is_truncated;

fn step(node_id: &str, status: StepStatus, output: Value) -> ExecutionStep {
    ExecutionStep {
        node_id: node_id.into(),
        status,
        output,
        duration_ms: 12,
        diagnostics: Vec::new(),
        transcript: Vec::new(),
    }
}

fn graph() -> WorkflowGraph {
    WorkflowGraph {
        schema_version: 1,
        id: Some("g".into()),
        name: "g".into(),
        inputs: Vec::new(),
        agents: Vec::new(),
        nodes: Vec::new(),
        edges: Vec::new(),
    }
}

#[test]
fn one_fat_node_does_not_take_the_rest_of_the_record_with_it() {
    // The whole reason bounding is per node. `bounded_within` is
    // non-recursive: applied to the aggregate, the big one would replace
    // every other node's output with a string preview.
    let big = json!({ "body": "x".repeat(600 * 1024) });
    let report = RunReport {
        steps: vec![
            StepRecord::bounded(
                &step("small", StepStatus::Success, json!({"ok": 1})),
                RECORD_BUDGET,
            ),
            StepRecord::bounded(&step("huge", StepStatus::Success, big), RECORD_BUDGET),
        ],
        ..RunReport::default()
    };

    assert!(
        !is_truncated(&report.steps[0].output),
        "the small node is intact"
    );
    assert!(
        is_truncated(&report.steps[1].output),
        "the big one is trimmed"
    );
    assert_eq!(report.steps[0].output, json!({"ok": 1}));
}

#[test]
fn a_swallowed_error_survives_the_round_trip() {
    // `output` alone cannot express this, which is why steps cross.
    let record = StepRecord::bounded(
        &step(
            "fetch",
            StepStatus::Error,
            json!({"error": "connection refused"}),
        ),
        RECORD_BUDGET,
    );
    let json = serde_json::to_string(&record).expect("serializes");
    let back: StepRecord = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(back.status, StepOutcome::Error);
    assert!(matches!(back.to_step().status, StepStatus::Error));
}

#[test]
fn every_iteration_of_a_looped_node_is_kept() {
    let report = RunReport {
        steps: vec![
            StepRecord::bounded(
                &step("body", StepStatus::Success, json!({"i": 1})),
                RECORD_BUDGET,
            ),
            StepRecord::bounded(
                &step("body", StepStatus::Success, json!({"i": 2})),
                RECORD_BUDGET,
            ),
            StepRecord::bounded(
                &step("body", StepStatus::Success, json!({"i": 3})),
                RECORD_BUDGET,
            ),
        ],
        ..RunReport::default()
    };
    assert_eq!(report.steps.len(), 3);

    // The reconstructed final state keeps only the last, as the engine's own
    // does — the history lives on `steps`.
    let ran = report.into_ran(&graph());
    assert_eq!(ran.outcome.output["nodes"]["body"], json!({"i": 3}));
    assert_eq!(ran.steps.len(), 3);
}

#[test]
fn the_judges_view_is_bounded_tighter_than_the_record() {
    let body = json!({ "body": "x".repeat(64 * 1024) });
    let report = RunReport {
        steps: vec![StepRecord::bounded(
            &step("agent", StepStatus::Success, body),
            RECORD_BUDGET,
        )],
        ..RunReport::default()
    };
    // Well under the record budget, so kept whole there...
    assert!(!is_truncated(&report.steps[0].output));

    let ran = report.into_ran(&graph());
    // ...and trimmed in the projection the model reads.
    assert!(is_truncated(&ran.outcome.output["nodes"]["agent"]));
    assert!(
        !is_truncated(&ran.steps[0].output),
        "the record is untouched"
    );
}

#[test]
fn a_failed_run_still_carries_every_step_it_managed() {
    // The case `output` cannot express at all: the engine returned Err, so
    // there is no outcome, but eleven steps happened.
    let report = RunReport {
        steps: (0..11)
            .map(|i| {
                StepRecord::bounded(
                    &step("loop", StepStatus::Success, json!({ "i": i })),
                    RECORD_BUDGET,
                )
            })
            .collect(),
        failed: Some("loop node exceeded its maximum of 5 iterations".into()),
        ..RunReport::default()
    };
    let ran = report.into_ran(&graph());
    assert_eq!(ran.steps.len(), 11);
    assert_eq!(
        ran.outcome.output["error"],
        json!("loop node exceeded its maximum of 5 iterations")
    );
    // And the nodes are there too, so the judge sees what did happen rather
    // than only that something broke.
    assert!(ran.outcome.output["nodes"]["loop"].is_object());
}

#[test]
fn the_whole_report_round_trips_as_json() {
    let report = RunReport {
        attempt_id: "ep-1/3".into(),
        steps: vec![StepRecord::bounded(
            &step("write", StepStatus::Success, json!({"path": "report.md"})),
            RECORD_BUDGET,
        )],
        pending_approvals: vec!["publish".into()],
        cancelled: false,
        changed: "1 file changed".into(),
        failed: None,
        cost_usd: 0.42,
    };
    let text = serde_json::to_string(&report).expect("serializes");
    assert!(text.contains("attemptId"), "camelCase on the wire: {text}");
    let back: RunReport = serde_json::from_str(&text).expect("deserializes");
    assert_eq!(back.attempt_id, "ep-1/3");
    assert_eq!(back.pending_approvals, vec!["publish".to_string()]);
    assert!((back.cost_usd - 0.42).abs() < f64::EPSILON);
}

/// A harness transcript survives the wire form in both directions.
///
/// `Ran::steps` is documented as the archival record — "every node
/// activation, at full record fidelity" — so dropping the transcript here
/// would silently empty the richest part of an `agent` node's history on
/// every local and remote adaptive run.
#[test]
fn a_transcript_round_trips_through_the_record() {
    let entries = vec![
        TranscriptEntry::bounded(1, "agent_thinking", "memoise the chain"),
        TranscriptEntry::bounded(2, "tool_call", "shell: python3 solve.py"),
        TranscriptEntry::bounded(3, "tool_result", "837799"),
    ];
    let original = ExecutionStep {
        transcript: entries.clone(),
        ..step("solve", StepStatus::Success, json!([{ "json": 837_799 }]))
    };

    let record = StepRecord::bounded(&original, 4096);
    assert_eq!(record.transcript, entries, "the record keeps it");

    let back = record.to_step();
    assert_eq!(back.transcript, entries, "and hands it back");
}

/// The transcript is NOT clipped to the record budget.
///
/// `output` is, because it is one payload whose tail is the least
/// interesting part. A transcript is many already-bounded entries, and
/// cutting it mid-way loses the end of a thought rather than the tail of a
/// value — so the budget deliberately does not reach it.
#[test]
fn the_record_budget_does_not_clip_the_transcript() {
    let entries: Vec<TranscriptEntry> = (0..64)
        .map(|n| TranscriptEntry::bounded(n, "agent_thinking", "x".repeat(256)))
        .collect();
    let original = ExecutionStep {
        transcript: entries.clone(),
        ..step(
            "solve",
            StepStatus::Success,
            json!([{ "json": "x".repeat(9_000) }]),
        )
    };

    let record = StepRecord::bounded(&original, 128);
    assert!(
        is_truncated(&record.output),
        "the output IS clipped to the budget"
    );
    assert_eq!(
        record.transcript.len(),
        entries.len(),
        "the transcript is not"
    );
}

/// A record written before the field existed still deserializes.
#[test]
fn a_legacy_record_reads_as_having_no_transcript() {
    // camelCase, as the type serializes — a legacy record is a real wire
    // document, not a snake_case approximation of one.
    let legacy = json!({
        "nodeId": "solve",
        "status": "success",
        "output": [],
        "durationMs": 12,
        "nullBindings": [],
    });
    let record: StepRecord = serde_json::from_value(legacy).expect("deserialize");
    assert!(record.transcript.is_empty());
}

/// An empty transcript serializes exactly as it did before the field.
#[test]
fn an_empty_transcript_adds_nothing_to_the_wire() {
    let record = StepRecord::bounded(&step("cost", StepStatus::Success, json!([])), 4096);
    let wire = serde_json::to_string(&record).expect("serialize");
    assert!(!wire.contains("transcript"), "{wire}");
}

/// A transcript large enough to threaten the Mongo document cap is trimmed.
///
/// Per-entry bounds are not enough: a `per_item` node folds every item's
/// turn into ONE step, so thousands of 4 KiB entries reach the 16 MB limit
/// a document may hold — and `save_steps` deletes before it upserts, so the
/// oversized write would destroy the previous record and then fail.
#[test]
fn an_oversized_transcript_is_trimmed_to_the_budget() {
    let entries: Vec<TranscriptEntry> = (0..4_000)
        .map(|n| TranscriptEntry::bounded(n, "agent_thinking", "x".repeat(1024)))
        .collect();
    let original = ExecutionStep {
        transcript: entries,
        ..step("solve", StepStatus::Success, json!([]))
    };

    let record = StepRecord::bounded(&original, RECORD_BUDGET);
    let bytes: usize = record.transcript.iter().map(|e| e.text.len()).sum();
    assert!(
        bytes < TRANSCRIPT_BUDGET,
        "trimmed to {bytes} bytes, over the {TRANSCRIPT_BUDGET} budget"
    );
}

/// Trimming keeps BOTH ends, and says how much it dropped.
///
/// The start says how the agent approached the work and the end says how it
/// concluded; clipping only the tail would lose the conclusion, which is
/// usually why someone opened the transcript.
#[test]
fn trimming_keeps_the_start_and_the_end() {
    let mut entries: Vec<TranscriptEntry> = (0..4_000)
        .map(|n| TranscriptEntry::bounded(n, "agent_thinking", "x".repeat(1024)))
        .collect();
    entries[0] = TranscriptEntry::bounded(0, "agent_thinking", "FIRST");
    let last = entries.len() - 1;
    entries[last] = TranscriptEntry::bounded(9_999, "agent_message", "LAST");

    let original = ExecutionStep {
        transcript: entries,
        ..step("solve", StepStatus::Success, json!([]))
    };
    let kept = StepRecord::bounded(&original, RECORD_BUDGET).transcript;

    assert_eq!(kept.first().map(|e| e.text.as_str()), Some("FIRST"));
    assert_eq!(kept.last().map(|e| e.text.as_str()), Some("LAST"));
    assert!(
        kept.iter().any(|e| e.text.contains("elided")),
        "the gap announces itself rather than being silent"
    );
}

/// A transcript within budget is untouched.
#[test]
fn a_transcript_within_budget_keeps_every_entry() {
    let entries: Vec<TranscriptEntry> = (0..64)
        .map(|n| TranscriptEntry::bounded(n, "agent_thinking", "x".repeat(256)))
        .collect();
    let original = ExecutionStep {
        transcript: entries.clone(),
        ..step("solve", StepStatus::Success, json!([]))
    };
    assert_eq!(
        StepRecord::bounded(&original, RECORD_BUDGET).transcript,
        entries
    );
}

/// A handful of huge entries is bounded too, not just a long list.
///
/// `TranscriptEntry`'s fields are public, so nothing forces a harness to
/// build them through `bounded` — one entry larger than a whole Mongo
/// document can arrive. A count-based rule would wave this through, which
/// is exactly the hole the first version of the bound had.
#[test]
fn a_few_oversized_entries_are_bounded_too() {
    let entries: Vec<TranscriptEntry> = (0..4)
        .map(|n| TranscriptEntry {
            at_ms: n,
            kind: "agent_thinking".to_string(),
            // Built directly, past the per-entry cap.
            text: "x".repeat(8 * 1024 * 1024),
        })
        .collect();
    let original = ExecutionStep {
        transcript: entries,
        ..step("solve", StepStatus::Success, json!([]))
    };

    let kept = StepRecord::bounded(&original, RECORD_BUDGET).transcript;
    let bytes: usize = kept.iter().map(|e| e.kind.len() + e.text.len()).sum();
    assert!(
        bytes <= TRANSCRIPT_BUDGET,
        "four 8 MB entries survived as {bytes} bytes"
    );
}

/// `kind` counts against the budget as well as `text`.
///
/// It is host-supplied and an open set, so a budget that ignored it could
/// be walked past by a harness that puts its payload there.
#[test]
fn the_budget_counts_the_kind_as_well_as_the_text() {
    let entries: Vec<TranscriptEntry> = (0..4)
        .map(|n| TranscriptEntry {
            at_ms: n,
            kind: "k".repeat(8 * 1024 * 1024),
            text: String::new(),
        })
        .collect();
    let original = ExecutionStep {
        transcript: entries,
        ..step("solve", StepStatus::Success, json!([]))
    };

    let kept = StepRecord::bounded(&original, RECORD_BUDGET).transcript;
    let bytes: usize = kept.iter().map(|e| e.kind.len() + e.text.len()).sum();
    assert!(bytes <= TRANSCRIPT_BUDGET, "kind was not counted: {bytes}");
}

/// Trimming a very large transcript stays fast.
///
/// The first version rescanned the whole vector per iteration and removed from
/// its middle — quadratic, on work that runs *after* the agent has finished and
/// while a report is waiting to go out. A generous ceiling: the point is to
/// catch a return to quadratic, not to benchmark.
#[test]
fn trimming_a_huge_transcript_is_not_quadratic() {
    let entries: Vec<TranscriptEntry> = (0..80_000)
        .map(|n| TranscriptEntry::bounded(n, "agent_thinking", "x".repeat(64)))
        .collect();
    let original = ExecutionStep {
        transcript: entries,
        ..step("solve", StepStatus::Success, json!([]))
    };

    let started = std::time::Instant::now();
    let kept = StepRecord::bounded(&original, RECORD_BUDGET).transcript;
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "took {elapsed:?} — the trim has gone quadratic again"
    );
    let bytes: usize = kept.iter().map(|e| e.kind.len() + e.text.len()).sum();
    assert!(bytes <= TRANSCRIPT_BUDGET);
    assert!(kept.iter().any(|e| e.text.contains("elided")));
}

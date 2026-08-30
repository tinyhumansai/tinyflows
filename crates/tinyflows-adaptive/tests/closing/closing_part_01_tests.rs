/// A plan that calls two saved workflows, with the second one's step failing.
fn composed() -> WorkflowGraph {
    use tinyflows::model::{Edge, Node, NodeKind};
    let call = |id: &str, workflow: &str| Node {
        id: id.to_string(),
        kind: NodeKind::SubWorkflow,
        type_version: 1,
        name: id.to_string(),
        config: json!({ "workflow_id": workflow }),
        ports: Vec::new(),
        position: None,
    };
    WorkflowGraph {
        schema_version: 1,
        id: None,
        name: "composed".into(),
        inputs: Vec::new(),
        agents: Vec::new(),
        nodes: vec![
            Node {
                id: "start".into(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "start".into(),
                config: json!({ "trigger_kind": "manual" }),
                ports: Vec::new(),
                position: None,
            },
            call("write_haiku", "haiku-writer"),
            call("write_limerick", "limerick-writer"),
            call("never_reached", "epic-writer"),
        ],
        edges: vec![
            Edge {
                from_node: "start".into(),
                from_port: "main".into(),
                to_node: "write_haiku".into(),
                to_port: "main".into(),
            },
            Edge {
                from_node: "write_haiku".into(),
                from_port: "main".into(),
                to_node: "write_limerick".into(),
                to_port: "main".into(),
            },
            Edge {
                from_node: "write_limerick".into(),
                from_port: "main".into(),
                to_node: "never_reached".into(),
                to_port: "main".into(),
            },
        ],
    }
}

fn step(node: &str, ok: bool) -> tinyflows_adaptive::execute::StepRecord {
    use tinyflows_adaptive::execute::{StepOutcome, StepRecord};
    StepRecord {
        node_id: node.to_string(),
        status: if ok {
            StepOutcome::Success
        } else {
            StepOutcome::Error
        },
        output: Value::Null,
        duration_ms: 1,
        null_bindings: Vec::new(),
        transcript: Vec::new(),
    }
}

#[tokio::test]
async fn a_workflow_called_by_a_plan_earns_the_same_record_a_chosen_one_does() {
    // Without this a workflow only ever used as a component stays Unproven
    // forever: the chooser distrusts it and the promotion gate cannot see it,
    // so composition becomes a place procedures go to stop earning a
    // reputation.
    let llm = Scripted::new(vec![json!({
        "satisfied": true, "blocker": "none", "gap": "", "advanced": true
    })]);
    let ledger = MemoryLedger::new();
    let diagnosis = Diagnosis::default();
    let outcome = RunOutcome {
        output: json!({}),
        pending_approvals: Vec::new(),
        cancelled: false,
    };
    let mut finished = ran(&outcome, &diagnosis, "wrote the document");
    finished.steps = vec![
        step("write_haiku", true),
        // Errored inside a plan that recovered around it.
        step("write_limerick", false),
    ];

    close(
        &Goal::new("a haiku and a limerick"),
        "ep-compose",
        1,
        &Approach::Authored {
            why: "compose the two writers".into(),
            fingerprint: "abc1234".into(),
        },
        &composed(),
        &finished,
        &Budget::default(),
        &ledger,
        &caps_with(llm),
        None,
        "2026-01-01T00:00:00Z",
    )
    .await
    .expect("closes");

    let haiku = ledger.workflow_score("haiku-writer").await.expect("scored");
    assert_eq!(
        (haiku.applied, haiku.helped),
        (1, 1),
        "it ran and the attempt was satisfied — the standard a selection is held to"
    );

    let limerick = ledger
        .workflow_score("limerick-writer")
        .await
        .expect("scored");
    assert_eq!(
        (limerick.applied, limerick.helped),
        (1, 0),
        "a child that errored inside a satisfied plan was exercised, not vindicated"
    );

    let never = ledger.workflow_score("epic-writer").await.expect("scored");
    assert_eq!(
        (never.applied, never.helped),
        (0, 0),
        "a call the run never reached is not evidence of anything"
    );
}

#[tokio::test]
async fn a_called_workflow_earns_nothing_from_an_attempt_that_fell_short() {
    // The counters must stay readable as evidence: an unsatisfied episode
    // gives a component `applied` and no more, exactly as it would a chosen
    // workflow that failed.
    let llm = Scripted::new(vec![json!({
        "satisfied": false, "blocker": "goal_not_met",
        "gap": "the document is missing the limerick", "advanced": false
    })]);
    let ledger = MemoryLedger::new();
    let diagnosis = Diagnosis::default();
    let outcome = RunOutcome {
        output: json!({}),
        pending_approvals: Vec::new(),
        cancelled: false,
    };
    let mut finished = ran(&outcome, &diagnosis, "wrote half a document");
    finished.steps = vec![step("write_haiku", true)];

    close(
        &Goal::new("a haiku and a limerick"),
        "ep-short",
        1,
        &Approach::Authored {
            why: "compose the two writers".into(),
            fingerprint: "abc1234".into(),
        },
        &composed(),
        &finished,
        &Budget::default(),
        &ledger,
        &caps_with(llm),
        None,
        "2026-01-01T00:00:00Z",
    )
    .await
    .expect("closes");

    let haiku = ledger.workflow_score("haiku-writer").await.expect("scored");
    assert_eq!(
        (haiku.applied, haiku.helped),
        (1, 0),
        "the child ran cleanly, but nothing it was part of was satisfied"
    );
}

#[tokio::test]
async fn every_activation_of_a_looped_call_is_scored_not_just_the_first() {
    // A node inside a loop produces one `StepRecord` per iteration. Reading
    // only the first record credits the workflow once for work it did three
    // times — and, worse, lets an early success hide a later error, so a child
    // that failed a pass reads as clean. The counters are the only evidence the
    // chooser and the promotion gate have; they have to count what happened.
    let llm = Scripted::new(vec![json!({
        "satisfied": true, "blocker": "none", "gap": "", "advanced": true
    })]);
    let ledger = MemoryLedger::new();
    let diagnosis = Diagnosis::default();
    let outcome = RunOutcome {
        output: json!({}),
        pending_approvals: Vec::new(),
        cancelled: false,
    };
    let mut finished = ran(&outcome, &diagnosis, "wrote three haiku");
    // One node, three passes, mixed outcomes — the first one succeeding is
    // exactly the arrangement that made the old reading look correct.
    finished.steps = vec![
        step("write_haiku", true),
        step("write_haiku", false),
        step("write_haiku", true),
    ];

    close(
        &Goal::new("three haiku"),
        "ep-loop",
        1,
        &Approach::Authored {
            why: "call the writer once per subject".into(),
            fingerprint: "abc1234".into(),
        },
        &composed(),
        &finished,
        &Budget::default(),
        &ledger,
        &caps_with(llm),
        None,
        "2026-01-01T00:00:00Z",
    )
    .await
    .expect("closes");

    let haiku = ledger.workflow_score("haiku-writer").await.expect("scored");
    assert_eq!(
        (haiku.applied, haiku.helped),
        (3, 2),
        "three activations, and the one that errored is not vindicated by the \
         two that did not"
    );
}

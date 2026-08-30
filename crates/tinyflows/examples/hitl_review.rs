#![forbid(unsafe_code)]

//! Human review as a **step in the graph**: an `approval` node hands a URL to a
//! host-implemented review surface, the run pauses while nobody has answered,
//! and the branch it takes afterwards depends on what the human said.
//!
//! `DeskReview` below stands in for whatever real surface a host has — a Slack
//! card, an inbox row, a web queue. It shows the two things the
//! [`ApprovalProvider`](tinyflows::caps::ApprovalProvider) contract asks for:
//! **create-or-fetch** on `request_id`, so re-asking never notifies the reviewer
//! twice, and a decision that can carry the human's own edit.
//!
//! Run:  cargo run --example hitl_review --features mock
#[cfg(feature = "mock")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tinyflows::caps::mock::mock_capabilities;
    use tinyflows::caps::{
        ApprovalDecision, ApprovalOutcome, ApprovalProvider, ApprovalRequest, Capabilities,
    };
    use tinyflows::compiler::compile;
    use tinyflows::engine::{RunInput, resume, run};
    use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

    /// A host's review desk: one row per `request_id`, holding the verdict once
    /// a human has left one.
    #[derive(Default)]
    struct DeskReview {
        rows: Mutex<HashMap<String, Option<ApprovalDecision>>>,
    }

    impl DeskReview {
        /// What a human does later, from the host's own UI.
        fn answer(&self, request_id: &str, decision: ApprovalDecision) {
            self.rows
                .lock()
                .expect("lock")
                .insert(request_id.to_string(), Some(decision));
        }

        /// Every review this desk has been asked to run, decided or not.
        fn queue(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.rows.lock().expect("lock").keys().cloned().collect();
            ids.sort();
            ids
        }
    }

    #[async_trait]
    impl ApprovalProvider for DeskReview {
        async fn decide(
            &self,
            request: &ApprovalRequest,
        ) -> tinyflows::error::Result<ApprovalOutcome> {
            let mut rows = self.rows.lock().expect("lock");
            // Create-or-fetch: the row is keyed on `request_id`, so the run
            // asking again after a resume finds THIS review rather than opening
            // a second one and pinging the reviewer twice.
            let row = rows.entry(request.request_id.clone()).or_insert_with(|| {
                println!(
                    "[desk] new review {:?}: {} -> {}",
                    request.request_id,
                    request.title.as_deref().unwrap_or("(untitled)"),
                    request.subject.value
                );
                None
            });
            Ok(match row.clone() {
                Some(decision) => ApprovalOutcome::Decided(decision),
                None => ApprovalOutcome::Pending,
            })
        }
    }

    fn node(id: &str, kind: NodeKind, config: Value) -> Node {
        Node {
            id: id.into(),
            kind,
            type_version: 1,
            name: id.into(),
            config,
            ports: vec![],
            position: None,
        }
    }
    fn edge(from: &str, port: &str, to: &str) -> Edge {
        Edge {
            from_node: from.into(),
            from_port: port.into(),
            to_node: to.into(),
            to_port: "main".into(),
        }
    }

    // trigger -> review -> publish (on `approved`) / revise (on `rejected`).
    let graph = WorkflowGraph {
        nodes: vec![
            node("trigger", NodeKind::Trigger, Value::Null),
            node(
                "review",
                NodeKind::Approval,
                json!({
                    // A real host would key this on the run id (e.g.
                    // `"=run.id"`) rather than a literal, so two runs of this
                    // graph never collide on the same review.
                    "title": "Publish this post?",
                    "prompt": "Approving publishes it to the public feed.",
                    "subject_kind": "url",
                    "subject": "=item.url",
                    "assignees": ["editor@example.com"],
                }),
            ),
            node(
                "publish",
                NodeKind::Transform,
                json!({ "set": { "published": "=item.subject" } }),
            ),
            node(
                "revise",
                NodeKind::Transform,
                json!({ "set": { "revise_because": "=item.comment" } }),
            ),
        ],
        edges: vec![
            edge("trigger", "main", "review"),
            edge("review", "approved", "publish"),
            edge("review", "rejected", "revise"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let desk = Arc::new(DeskReview::default());
    let caps = Capabilities {
        approvals: Some(desk.clone()),
        ..mock_capabilities()
    };
    // The host names the run. This is what gives the review a stable identity
    // (`request_id` defaults to "<run id>:<node id>"), so the resume below
    // resolves the card already in front of a person instead of opening a
    // second one. It is seeded outside the trigger payload on purpose: a
    // caller-supplied value here would hand an attacker the de-duplication key.
    let run_id = "run-7f3a";
    let trigger = json!({ "url": "https://example.com/drafts/42" });
    let input = || RunInput::new(trigger.clone()).with_run_id(run_id);

    // 1) Nobody has answered, so the run suspends at the review. Nothing is
    //    burned while the card sits in someone's queue.
    let paused = run(&compiled, input(), &caps).await.expect("run");
    println!("--- before the human answers ---");
    println!("pending_approvals: {:?}", paused.pending_approvals);
    println!("desk queue:        {:?}", desk.queue());

    // 2) The human approves — and edits the URL on the way through, which the
    //    host reports as the decision's payload.
    let request_id = desk.queue().first().cloned().expect("one open review");
    desk.answer(
        &request_id,
        ApprovalDecision {
            approved: true,
            decided_by: Some("editor@example.com".into()),
            comment: Some("fixed the slug".into()),
            payload: Some(json!("https://example.com/drafts/42?utm=newsletter")),
        },
    );

    // 3) Resuming re-asks the desk, which now has the verdict. Note the review
    //    id is unchanged, so the reviewer is never asked a second time.
    let done = resume(&compiled, input(), vec![], &caps)
        .await
        .expect("resume");
    println!("--- after the human answers ---");
    println!("pending_approvals: {:?}", done.pending_approvals);
    println!(
        "review port:       {}",
        done.output["nodes"]["review"]["port"]
    );
    println!(
        "published:         {}",
        done.output["nodes"]["publish"]["items"][0]["json"]["published"]
    );
    println!("desk queue:        {:?}", desk.queue());
}

#[cfg(not(feature = "mock"))]
fn main() {
    eprintln!(
        "this example needs the mock capabilities: cargo run --example hitl_review --features mock"
    );
}

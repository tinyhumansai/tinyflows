// ---- transcripts: what the harness did inside the node ------------------
//
// Split out of part 02, which the repo's 500-line rule would otherwise have
// pushed over.

mod transcript {
    use super::agent_node;
    use crate::caps::mock::{MockAgentHarness, MockAgentRunner, mock_capabilities_with_agent};
    use crate::caps::AgentRunner;
    use crate::data::Item;
    use crate::model::AgentDefinition;
    use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};
    use crate::observability::{NoopObserver, RunObserver};
    use crate::transcript::TranscriptEntry;
    use serde_json::{Value, json};
    use std::sync::Arc;

    async fn execute(
        runner: Arc<dyn AgentRunner>,
        config: Value,
        input: Vec<Item>,
        observer: &dyn RunObserver,
    ) -> NodeOutput {
        let node = agent_node(config);
        let caps = mock_capabilities_with_agent_arc(runner);
        let agents: &[AgentDefinition] = &[];
        let run_meta = json!({ "run_id": "run_t", "sub_workflow_depth": 0 });
        super::super::AgentNode
            .execute(NodeContext {
                node: &node,
                input: &input,
                run: &run_meta,
                nodes: &Value::Null,
                caps: &caps,
                agents,
                observer,
                token: crate::engine::CancellationToken::new(),
                lane: None,
                resume: None,
                step: 0,
            })
            .await
            .expect("execute")
    }

    /// `mock_capabilities_with_agent` takes a concrete runner; these tests need
    /// to swap two different ones through the same helper.
    fn mock_capabilities_with_agent_arc(
        runner: Arc<dyn AgentRunner>,
    ) -> crate::caps::Capabilities {
        let mut caps = mock_capabilities_with_agent(MockAgentRunner);
        caps.agent = Some(runner);
        caps
    }

    fn harness() -> Arc<dyn AgentRunner> {
        Arc::new(MockAgentHarness::new())
    }

    #[tokio::test]
    async fn a_harness_transcript_reaches_the_node_output() {
        // The settled half: what the host reported on its `AgentRunOutcome`
        // rides the `NodeOutput`, which is what the engine copies onto the step.
        let out = execute(
            harness(),
            json!({ "agent_ref": "triager" }),
            vec![Item::new(json!({ "seed": 1 }))],
            &NoopObserver,
        )
        .await;

        assert_eq!(
            out.transcript
                .iter()
                .map(|e| e.kind.as_str())
                .collect::<Vec<_>>(),
            ["agent_thinking", "agent_message"],
            "MockAgentHarness reports two entries; both must survive to the output"
        );
    }

    #[tokio::test]
    async fn per_item_turns_accumulate_into_one_transcript() {
        // Why the accumulator is shared rather than returned: a per-item node
        // runs one turn per input and reports ONE step. Without it, every turn
        // but the last would be dropped.
        let out = execute(
            harness(),
            json!({ "agent_ref": "triager", "execution": "per_item" }),
            vec![
                Item::new(json!({ "seed": 1 })),
                Item::new(json!({ "seed": 2 })),
                Item::new(json!({ "seed": 3 })),
            ],
            &NoopObserver,
        )
        .await;

        assert_eq!(out.items.len(), 3, "one turn per input item");
        assert_eq!(
            out.transcript.len(),
            6,
            "two entries per turn, all three turns kept"
        );
    }

    #[tokio::test]
    async fn a_legacy_host_reports_no_transcript() {
        // THE non-breaking guarantee. `MockAgentRunner` implements only the
        // legacy `run_agent`, so the default `run` wraps its return in a
        // `finished` outcome with no transcript. A host that never heard of this
        // field keeps working and simply has nothing to say.
        let out = execute(
            Arc::new(MockAgentRunner),
            json!({ "agent_ref": "triager" }),
            vec![Item::new(json!({ "seed": 1 }))],
            &NoopObserver,
        )
        .await;
        assert!(out.transcript.is_empty());
        assert_eq!(out.items.len(), 1, "the turn still ran and still emitted");
    }

    #[tokio::test]
    async fn a_node_with_no_harness_reports_no_transcript() {
        // The degraded path: no `agent_ref`, so the node falls back to
        // `LlmProvider` and there is no harness to have a transcript. Empty is
        // the honest answer, and must not be an error.
        let out = execute(
            harness(),
            json!({ "prompt": "hi" }),
            vec![Item::new(json!({}))],
            &NoopObserver,
        )
        .await;
        assert!(out.transcript.is_empty());
    }

    /// A runner that names its item and finishes in reverse order.
    ///
    /// Both halves matter. Naming the item is what lets the assertion tell
    /// input order from completion order at all — four identical turns cannot.
    /// Finishing in reverse is what makes the two orders actually disagree, so
    /// a completion-ordered accumulator fails this test rather than passing it
    /// by luck.
    struct ReverseOrderHarness;

    #[async_trait::async_trait]
    impl AgentRunner for ReverseOrderHarness {
        async fn run_agent(
            &self,
            _agent_ref: &str,
            _request: Value,
            _conn: Option<&str>,
        ) -> crate::error::Result<Value> {
            unreachable!("the typed `run` is overridden")
        }

        async fn run(
            &self,
            request: crate::caps::AgentRunRequest,
        ) -> crate::error::Result<crate::caps::AgentRunOutcome> {
            // `=item.seed` is resolved against this item before the request is
            // assembled, so the config carries which item this turn is for.
            let seed = request
                .config
                .get("prompt")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            // Later items return first.
            let delay = 40u64.saturating_sub(seed * 10);
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            Ok(crate::caps::AgentRunOutcome::finished(json!({
                "text": format!("answered {seed}")
            }))
            .with_transcript(vec![TranscriptEntry::bounded(
                0,
                "agent_message",
                format!("item {seed}"),
            )]))
        }
    }

    #[tokio::test]
    async fn per_item_transcripts_come_back_in_item_order() {
        // `map_items` restores its OUTPUTS to input order, so the transcript
        // describing them has to match. The sink is keyed by item index for
        // exactly this: with `concurrency > 1` the turns finish in whatever
        // order they finish, and appending on completion would make one run's
        // transcript differ from the next for identical input.
        let out = execute(
            Arc::new(ReverseOrderHarness),
            json!({
                "agent_ref": "triager",
                "execution": "per_item",
                "concurrency": 4,
                "prompt": "=item.seed",
            }),
            (0..4u64).map(|n| Item::new(json!({ "seed": n }))).collect(),
            &NoopObserver,
        )
        .await;

        assert_eq!(out.items.len(), 4, "one turn per input item");
        assert_eq!(
            out.transcript
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            ["item 0", "item 1", "item 2", "item 3"],
            "input order, not completion order"
        );
    }
}

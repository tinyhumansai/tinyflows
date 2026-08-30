/// The frame can resolve a node's bindings without executing it — the
/// inspection a breakpoint needs, and the thing that turns "it produced null"
/// into a pointer at the binding that did.
#[tokio::test]
async fn a_frame_resolves_bindings_without_executing() {
    struct Capture(Mutex<Vec<(String, String)>>);

    #[async_trait]
    impl StepInterceptor for Capture {
        async fn intercept(&self, frame: StepFrame<'_>) -> StepAction {
            if frame.phase == StepPhase::Before && frame.node.id == "call" {
                let (_resolved, nulls) = frame.resolved_config();
                let mut seen = self.0.lock().expect("capture lock");
                for null in nulls {
                    seen.push((null.location, null.expression));
                }
            }
            StepAction::Continue { state_patch: None }
        }
    }

    let mut graph = graph();
    // A binding onto a field no upstream node produces: legal, resolves to
    // null, and does nothing at run time. Exactly the failure a green run hides.
    graph.nodes[1].config = json!({
        "slug": "svc.do",
        "args": { "to": "=nodes.t.item.missing_field" }
    });
    let compiled = compile(&graph).expect("compile");
    let hook = Arc::new(Capture(Mutex::new(Vec::new())));

    run_intercepted(
        &compiled,
        json!({}),
        &mock_capabilities(),
        &(Arc::new(NoopObserver) as Arc<dyn RunObserver>),
        CancellationToken::new(),
        hook.clone(),
    )
    .await
    .expect("run");

    let seen = hook.0.lock().expect("capture lock").clone();
    assert_eq!(
        seen,
        vec![(
            "args.to".to_string(),
            "=nodes.t.item.missing_field".to_string()
        )],
        "the frame should report the null binding and where it was written"
    );
}

/// A node that failed once and then succeeded must not be reported as failed.
///
/// Regression: the engine's retry loop keeps the last failed attempt's error
/// even after a later attempt succeeds, so an `After` frame that surfaced
/// `last_err` unconditionally showed a recovered node as a failed one — and
/// would fire every on-error breakpoint on it.
#[tokio::test]
async fn a_recovered_retry_reports_no_error_to_the_interceptor() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fails the first call, succeeds thereafter.
    struct Flaky(AtomicUsize);

    #[async_trait]
    impl tinyflows::caps::ToolInvoker for Flaky {
        async fn invoke(
            &self,
            _slug: &str,
            _args: Value,
            _conn: Option<&str>,
        ) -> tinyflows::error::Result<Value> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(tinyflows::error::EngineError::Capability(
                    "transient".into(),
                ))
            } else {
                Ok(json!({ "ok": true }))
            }
        }
    }

    /// Records whether the after-frame carried an error, per node.
    struct SawError(Mutex<Vec<(String, bool)>>);

    #[async_trait]
    impl StepInterceptor for SawError {
        async fn intercept(&self, frame: StepFrame<'_>) -> StepAction {
            if frame.phase == StepPhase::After {
                self.0
                    .lock()
                    .expect("lock")
                    .push((frame.node.id.clone(), frame.error.is_some()));
            }
            StepAction::Continue { state_patch: None }
        }
    }

    let mut graph = graph();
    graph.nodes[1].config = json!({ "slug": "svc.do", "retry": { "max_attempts": 2 } });
    let compiled = compile(&graph).expect("compile");

    let mut caps = mock_capabilities();
    caps.tools = Arc::new(Flaky(AtomicUsize::new(0)));
    let hook = Arc::new(SawError(Mutex::new(Vec::new())));

    run_intercepted(
        &compiled,
        json!({}),
        &caps,
        &(Arc::new(NoopObserver) as Arc<dyn RunObserver>),
        CancellationToken::new(),
        hook.clone(),
    )
    .await
    .expect("the retry recovers, so the run completes");

    let seen = hook.0.lock().expect("lock").clone();
    let call = seen
        .iter()
        .find(|(id, _)| id == "call")
        .expect("the retrying node reports an after-frame");
    assert!(
        !call.1,
        "a node that recovered on retry must not surface an error to the interceptor"
    );
}

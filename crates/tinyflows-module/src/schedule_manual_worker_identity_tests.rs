use super::*;

#[tokio::test]
async fn config_failure_keeps_the_supplied_run_id_in_the_terminal_outcome() {
    use tinyflows_bus::schedule::{
        ManualDelivery, ManualRunOutcome, ManualRunRequest, ManualRunState, PrepareManualRun,
        StartManualRun, WaitManualRun,
    };

    let (_server, host, _) = connections().await;
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    let handle: tinyflows_bus::schedule::ManualRunHandle = proxy
        .call(
            "PrepareManualRun",
            &(PrepareManualRun {
                scope: "manual-run-identity".into(),
                request_id: "bad-config".into(),
            },),
        )
        .await
        .unwrap();
    let job: CronJob = serde_json::from_value(json!({
        "id":"identity-job","expression":"","schedule":{"kind":"every","every_ms":60000},
        "command":"echo identity","prompt":null,"name":null,"job_type":"shell",
        "session_target":"isolated","model":null,"agent_id":null,"enabled":true,
        "delivery":{"mode":"none"},"delete_after_run":false,
        "created_at":"2026-01-01T00:00:00Z","next_run":"2026-01-01T00:00:00Z"
    }))
    .unwrap();
    let run_id = "stable-config-error-run";
    proxy
        .call::<ManualRunState>(
            "StartManualRun",
            &(StartManualRun {
                handle: handle.clone(),
                request: ManualRunRequest {
                    callback: callback(),
                    config: json!("not a scheduler config"),
                    storage: serde_json::from_value(json!({
                        "backend":{"kind":"documents","callback":{
                            "destination":"test.ScheduleHost","object_path":"/test/Documents"
                        }}
                    }))
                    .unwrap(),
                    job,
                    run_id: run_id.into(),
                    delivery: ManualDelivery::NotRequested,
                },
            },),
        )
        .await
        .unwrap();

    let mut terminal = None;
    for _ in 0..10_000 {
        let current: ManualRunState = proxy
            .call::<Option<ManualRunState>>(
                "WaitManualRun",
                &(WaitManualRun {
                    handle: handle.clone(),
                },),
            )
            .await
            .unwrap()
            .expect("the prepared handle remains observable");
        if matches!(current, ManualRunState::Terminal { .. }) {
            terminal = Some(current);
            break;
        }
        tokio::task::yield_now().await;
    }

    assert!(matches!(
        terminal,
        Some(ManualRunState::Terminal {
            outcome: ManualRunOutcome::Failed {
                run_id: Some(ref actual),
                ..
            }
        }) if actual == run_id
    ));
}

#[tokio::test]
async fn config_failure_keeps_the_generated_run_id_in_the_terminal_outcome() {
    use tinyflows_bus::schedule::{
        ManualDelivery, ManualRunOutcome, ManualRunRequest, ManualRunState, PrepareManualRun,
        StartManualRun, WaitManualRun,
    };

    let (_server, host, _) = connections().await;
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    let handle: tinyflows_bus::schedule::ManualRunHandle = proxy
        .call(
            "PrepareManualRun",
            &(PrepareManualRun {
                scope: "manual-generated-run-identity".into(),
                request_id: "bad-config-generated".into(),
            },),
        )
        .await
        .unwrap();
    let job: CronJob = serde_json::from_value(json!({
        "id":"generated-identity-job","expression":"","schedule":{"kind":"every","every_ms":60000},
        "command":"echo identity","prompt":null,"name":null,"job_type":"shell",
        "session_target":"isolated","model":null,"agent_id":null,"enabled":true,
        "delivery":{"mode":"none"},"delete_after_run":false,
        "created_at":"2026-01-01T00:00:00Z","next_run":"2026-01-01T00:00:00Z"
    }))
    .unwrap();
    proxy
        .call::<ManualRunState>(
            "StartManualRun",
            &(StartManualRun {
                handle: handle.clone(),
                request: ManualRunRequest {
                    callback: callback(),
                    config: json!("not a scheduler config"),
                    storage: serde_json::from_value(json!({
                        "backend":{"kind":"documents","callback":{
                            "destination":"test.ScheduleHost","object_path":"/test/Documents"
                        }}
                    }))
                    .unwrap(),
                    job,
                    run_id: String::new(),
                    delivery: ManualDelivery::NotRequested,
                },
            },),
        )
        .await
        .unwrap();

    let mut terminal = None;
    for _ in 0..10_000 {
        let current: ManualRunState = proxy
            .call::<Option<ManualRunState>>(
                "WaitManualRun",
                &(WaitManualRun {
                    handle: handle.clone(),
                },),
            )
            .await
            .unwrap()
            .expect("the prepared handle remains observable");
        if matches!(current, ManualRunState::Terminal { .. }) {
            terminal = Some(current);
            break;
        }
        tokio::task::yield_now().await;
    }

    assert!(matches!(
        terminal,
        Some(ManualRunState::Terminal {
            outcome: ManualRunOutcome::Failed {
                run_id: Some(ref actual),
                ..
            }
        }) if !actual.is_empty()
    ));
}

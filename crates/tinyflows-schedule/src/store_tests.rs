use super::*;

#[test]
fn short_output_is_kept_and_long_output_is_cut_at_a_char_boundary() {
    assert_eq!(truncate_cron_output("ok"), "ok");
    let exact = "a".repeat(MAX_CRON_OUTPUT_BYTES);
    assert_eq!(truncate_cron_output(&exact), exact);
    let long = "é".repeat(MAX_CRON_OUTPUT_BYTES);
    let cut = truncate_cron_output(&long);
    assert!(cut.len() <= MAX_CRON_OUTPUT_BYTES);
    assert!(cut.ends_with(TRUNCATED_OUTPUT_MARKER));
}

#[test]
fn a_new_spec_is_an_enabled_isolated_job() {
    let spec = AgentJobSpec::new(
        Schedule::Every {
            every_ms: 3_600_000,
        },
        "hi",
    );
    assert!(spec.enabled);
    assert_eq!(spec.session_target, SessionTarget::default());
    assert!(spec.origin.is_none() && spec.delivery.is_none());
}

#[test]
fn only_a_flow_jobs_command_is_pinned() {
    let mut job = CronJob {
        policy: Default::default(),
        id: "flow:a".into(),
        expression: "0 9 * * *".into(),
        schedule: Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: None,
            active_hours: None,
        },
        command: "a".into(),
        prompt: None,
        name: None,
        job_type: crate::JobType::Flow,
        session_target: SessionTarget::default(),
        model: None,
        agent_id: None,
        enabled: true,
        delivery: DeliveryConfig::default(),
        delete_after_run: false,
        created_at: chrono::Utc::now(),
        next_run: chrono::Utc::now(),
        last_run: None,
        last_status: None,
        last_output: None,
        origin: None,
    };
    let retarget = CronJobPatch {
        command: Some("b".into()),
        ..CronJobPatch::default()
    };
    assert!(check_patch(&job, &retarget).is_err());
    let same = CronJobPatch {
        command: Some("a".into()),
        ..CronJobPatch::default()
    };
    assert!(check_patch(&job, &same).is_ok());
    assert!(check_patch(&job, &CronJobPatch::default()).is_ok());
    job.job_type = crate::JobType::Shell;
    assert!(
        check_patch(&job, &retarget).is_ok(),
        "a shell job's command is editable"
    );
}

//! Retry and persistence independent of providers, policy, delivery transports.
use super::*;
use crate::{DeliveryStatus, JobType, Schedule};
use anyhow::Result;

/// Run with one stable id across retries and delivery. No run claim is taken;
/// manual entry points must acquire `RunClaim` before calling this function.
pub async fn execute_with_retry(
    ports: &RuntimePorts,
    config: &SchedulerConfig,
    job: &CronJob,
    run_id: &str,
) -> Result<Attempt> {
    let retries = job.policy.retries.unwrap_or(config.retries);
    let mut backoff_ms = config.backoff_ms.max(200);
    for attempt in 0..=retries {
        let result = if matches!(job.job_type, JobType::Flow) {
            ports.triggers.schedule(&job.command, run_id).await
        } else {
            ports.executor.execute(job, run_id).await
        };
        let result = result.unwrap_or_else(|error| Attempt {
            success: false,
            output: error.to_string(),
            retryable: true,
        });
        if result.success {
            return Ok(result);
        }
        if !result.retryable || attempt == retries {
            ports.executor.failed(job, run_id, &result).await;
            return Ok(result);
        }
        let jitter_ms = u64::from(ports.clock.now().timestamp_subsec_millis() % 250);
        ports
            .clock
            .sleep(Duration::from_millis(backoff_ms.saturating_add(jitter_ms)))
            .await;
        backoff_ms = backoff_ms.saturating_mul(2).min(30_000);
    }
    unreachable!("inclusive attempts always execute at least once")
}

pub(super) async fn execute_and_persist(
    ports: &RuntimePorts,
    config: &SchedulerConfig,
    job: &CronJob,
) -> Result<()> {
    let run_id = uuid::Uuid::new_v4().to_string();
    let started_at = ports.clock.now();
    let attempt = execute_with_retry(ports, config, job, &run_id).await?;
    let finished_at = ports.clock.now();
    let mut success = attempt.success;
    let delivery_status = match ports.delivery.deliver(job, &run_id, &attempt).await {
        Ok(status) => status,
        Err(error) => {
            if !job.delivery.best_effort {
                success = false;
            }
            tracing::warn!(job_id = %job.id, %error, "scheduled delivery failed");
            DeliveryStatus::Failed
        }
    };
    if delivery_status == DeliveryStatus::Failed && !job.delivery.best_effort {
        success = false;
    }
    ports
        .store
        .record_run(RunRecord {
            run_id,
            job_id: job.id.clone(),
            job_created_at: job.created_at,
            started_at,
            finished_at,
            status: if success { "ok" } else { "error" }.into(),
            output: attempt.output.clone(),
            delivery_status: Some(delivery_status),
        })
        .await?;
    let one_shot = matches!(job.schedule, Schedule::At { .. });
    let remove = one_shot && job.delete_after_run && success;
    let next_run = if one_shot {
        None
    } else {
        Some(next_run_for_schedule(&job.schedule, ports.clock.now())?)
    };
    ports
        .store
        .complete(
            job,
            Completion {
                remove,
                disable: one_shot && !remove,
                next_run,
                finished_at,
                success,
                output: attempt.output,
            },
        )
        .await
}

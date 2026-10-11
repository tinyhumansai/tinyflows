use super::*;
use async_trait::async_trait;
use tinyflows_schedule::runtime::*;
use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

struct Replacement(CronDocuments);
#[async_trait]
impl JobExecutor for Replacement {
    async fn execute(&self, job: &CronJob, _: &str) -> Result<Attempt> {
        self.0.remove_job(&job.id).await?;
        let replacement = self
            .0
            .add_flow_schedule_job("history-fence", job.schedule.clone())
            .await?;
        assert_eq!(job.id, replacement.id);
        assert_ne!(job.created_at, replacement.created_at);
        Ok(Attempt {
            success: true,
            output: "old incarnation".into(),
            retryable: false,
        })
    }
}
#[async_trait]
impl RunDelivery for Replacement {
    async fn deliver(&self, _: &CronJob, _: &str, _: &Attempt) -> Result<DeliveryStatus> {
        Ok(DeliveryStatus::NotRequested)
    }
}
#[async_trait]
impl TriggerSink for Replacement {
    async fn schedule(&self, flow_id: &str, run_id: &str) -> Result<Attempt> {
        let job = self.0.find_flow_schedule_job(flow_id).await?.unwrap();
        self.execute(&job, run_id).await
    }
}
#[tokio::test]
async fn old_run_history_does_not_attach_to_a_recreated_flow_job() {
    let backend = MemoryStorage::new();
    let scoped = backend
        .for_scope(&Scope::new("history-fence").unwrap())
        .unwrap();
    let documents = CronDocuments::new(scoped.documents().clone());
    let job = documents
        .add_flow_schedule_job("history-fence", Schedule::Every { every_ms: 60_000 })
        .await
        .unwrap();
    let io = Arc::new(Replacement(documents.clone()));
    let ports = RuntimePorts {
        store: Arc::new(Store::Documents(documents.clone())),
        executor: io.clone(),
        delivery: io.clone(),
        triggers: io,
        clock: Arc::new(SystemClock),
    };
    let mut scheduler = Scheduler::new("history-fence", SchedulerConfig::default(), ports);
    scheduler.dispatch(vec![job.clone()]).await.unwrap();
    let error = scheduler.drain().await.unwrap_err();
    assert!(
        error.to_string().contains("removed job creation"),
        "{error:#}"
    );
    assert!(
        documents.list_runs(&job.id, 10).await.unwrap().is_empty(),
        "old execution contaminated the replacement job history"
    );
}

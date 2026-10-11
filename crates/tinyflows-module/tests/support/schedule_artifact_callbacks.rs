// Shared fixture for the compiled module test; client uses only the bus wire vocabulary.
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tinybus::Result;
#[derive(Default)]
pub struct State {
    pub records: Mutex<Vec<Value>>,
    pub completions: Mutex<Vec<Value>>,
    pub triggers: Mutex<Vec<String>>,
    pub defer_first: std::sync::atomic::AtomicBool,
    pub started: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    pub exclusions: Mutex<Vec<Vec<String>>>,
}
pub struct Callbacks(pub Arc<State>);
#[tinybus::interface(name = "ai.tinyhumans.tinyflows.ScheduleHost")]
impl Callbacks {
    async fn due_jobs(&self, _now: String) -> Result<Vec<Value>> {
        if !self.0.defer_first.load(std::sync::atomic::Ordering::SeqCst)
            && !self.0.completions.lock().unwrap().is_empty()
        {
            return Ok(vec![]);
        }
        Ok(vec![
            json!({"id":"artifact-flow-job", "expression":"", "schedule":{"kind":"at", "at":"2026-01-01T00:00:00Z"}, "command":"artifact-flow", "job_type":"flow", "session_target":"isolated", "enabled":true, "delivery":{"mode":"none"}, "delete_after_run":false, "created_at":"2026-01-01T00:00:00Z", "next_run":"2026-01-01T00:00:00Z", "prompt":null, "name":null, "model":null, "agent_id":null, "last_run":null, "last_status":null, "last_output":null}),
        ])
    }
    async fn due_jobs_excluding(&self, now: String, excluded: Vec<String>) -> Result<Vec<Value>> {
        self.0.exclusions.lock().unwrap().push(excluded.clone());
        let mut jobs = self.due_jobs(now).await?;
        if self.0.defer_first.load(std::sync::atomic::Ordering::SeqCst) {
            let mut later = jobs[0].clone();
            later["id"] = json!("later-flow-job");
            later["command"] = json!("later-flow");
            jobs.push(later);
        }
        jobs.retain(|job| !excluded.iter().any(|id| job["id"] == *id));
        jobs.truncate(1);
        Ok(jobs)
    }
    async fn claim_slot(&self, _job: Value, _next_run: String) -> Result<()> {
        Ok(())
    }
    async fn record_run(&self, record: Value) -> Result<()> {
        self.0.records.lock().unwrap().push(record);
        Ok(())
    }
    async fn complete(&self, _job: Value, completion: Value) -> Result<()> {
        self.0.completions.lock().unwrap().push(completion);
        Ok(())
    }
    async fn execute(&self, _job: Value, _run_id: String) -> Result<Value> {
        Err(tinybus::Error::failed("a flow must use ScheduleTrigger"))
    }
    async fn failed(&self, _job: Value, _run_id: String, _attempt: Value) -> Result<()> {
        Ok(())
    }
    async fn deliver(&self, _job: Value, _run_id: String, _attempt: Value) -> Result<String> {
        Ok("not_requested".into())
    }
    async fn schedule_trigger(&self, flow_id: String, _run_id: String) -> Result<Value> {
        self.0.triggers.lock().unwrap().push(flow_id.clone());
        if flow_id == "artifact-flow"
            && self.0.defer_first.load(std::sync::atomic::Ordering::SeqCst)
        {
            self.0.started.notify_one();
            self.0.release.notified().await;
        }
        Ok(json!({"success":true,"output":"scheduled","retryable":false}))
    }
}

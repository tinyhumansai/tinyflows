//! Non-blocking observer and durable event writer for one flow run.

use std::sync::{Arc, Mutex};

use crate::caps::RunProgressSink;
use anyhow::Result;
use serde_json::{Value, json};
use tinyflows::engine::{GraphEventJournal, GraphObservation, InMemoryGraphEventJournal};
use tinyflows::observability::{ExecutionStep, Run, RunObserver, RunStatus, StepStatus};
use tinyflows_bus::runs::{RunEvent, RunFinished};
use tinyflows_catalog::FlowRunStep;
use tinyflows_drivers::catalog::FlowCatalogDocuments;
use tokio::sync::{mpsc, oneshot};

/// Construct the terminal wire payload using the owning engine's status
/// classification. Callers cannot provide an independent success flag.
pub(super) fn finished_payload(
    status: &str,
    error: Option<String>,
    graph_run_id: Option<String>,
) -> RunFinished {
    RunFinished {
        status: status.to_owned(),
        successful: tinyflows::nodes::control_flow::dedup_settle::is_success_status(status),
        error,
        graph_run_id,
    }
}

async fn persist_event(
    catalog: &FlowCatalogDocuments,
    run_id: &str,
    seq: &mut u64,
    kind: String,
    data: Value,
    graph_run_id: Option<String>,
    last_error: &mut Option<String>,
) -> Result<u64> {
    let event = RunEvent {
        seq: seq.saturating_add(1),
        kind,
        data,
        graph_run_id,
    };
    match catalog
        .append_flow_run_event(run_id, serde_json::to_value(&event)?)
        .await
    {
        Ok(()) => {
            *seq = event.seq;
            Ok(event.seq)
        }
        Err(error) => {
            let message = error.to_string();
            *last_error = Some(message.clone());
            Err(anyhow::anyhow!(message))
        }
    }
}

enum WriteCommand {
    Step {
        step: FlowRunStep,
        data: Value,
    },
    Event {
        kind: String,
        data: Value,
        graph_run_id: Option<String>,
        ack: Option<oneshot::Sender<Result<u64>>>,
    },
    Barrier(oneshot::Sender<Option<String>>),
}

#[derive(Clone)]
pub(super) struct EventWriter {
    sender: mpsc::UnboundedSender<WriteCommand>,
}

impl EventWriter {
    pub fn new(catalog: FlowCatalogDocuments, run_id: String, initial_seq: u64) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut seq = initial_seq;
            let mut last_error = None;
            let mut terminal = false;
            while let Some(command) = receiver.recv().await {
                match command {
                    WriteCommand::Step { step, data } => {
                        if terminal {
                            continue;
                        }
                        if let Err(error) = catalog.upsert_flow_run_step(&run_id, &step).await {
                            last_error = Some(format!("persisting run step: {error}"));
                        }
                        let _ = persist_event(
                            &catalog,
                            &run_id,
                            &mut seq,
                            "step".into(),
                            data,
                            None,
                            &mut last_error,
                        )
                        .await;
                    }
                    WriteCommand::Event {
                        kind,
                        data,
                        graph_run_id,
                        ack,
                    } => {
                        if terminal {
                            if let Some(ack) = ack {
                                let _ =
                                    ack.send(Err(anyhow::anyhow!("run event stream is terminal")));
                            }
                            continue;
                        }
                        let seals_stream = kind == "finished";
                        let ack_result = persist_event(
                            &catalog,
                            &run_id,
                            &mut seq,
                            kind,
                            data,
                            graph_run_id,
                            &mut last_error,
                        )
                        .await;
                        if seals_stream && ack_result.is_ok() {
                            terminal = true;
                        }
                        if let Some(ack) = ack {
                            let _ = ack.send(ack_result);
                        }
                    }
                    WriteCommand::Barrier(ack) => {
                        let _ = ack.send(last_error.take());
                    }
                }
            }
        });
        Self { sender }
    }

    pub fn emit(&self, kind: impl Into<String>, data: Value) {
        let _ = self.sender.send(WriteCommand::Event {
            kind: kind.into(),
            data,
            graph_run_id: None,
            ack: None,
        });
    }

    pub fn graph_observation(&self, observation: &tinyflows::engine::GraphObservation) {
        let _ = self.sender.send(WriteCommand::Event {
            kind: "graph_observation".into(),
            data: serde_json::to_value(observation).unwrap_or(Value::Null),
            graph_run_id: Some(observation.run_id.as_str().to_owned()),
            ack: None,
        });
    }

    pub async fn emit_terminal(&self, finished: RunFinished) -> Result<u64> {
        let data = serde_json::to_value(finished)?;
        let (ack, result) = oneshot::channel();
        self.sender
            .send(WriteCommand::Event {
                kind: "finished".to_owned(),
                data,
                graph_run_id: None,
                ack: Some(ack),
            })
            .map_err(|_| anyhow::anyhow!("run event writer stopped"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("run event writer stopped"))?
    }

    pub async fn flush(&self) -> Option<String> {
        let (ack, result) = oneshot::channel();
        if self.sender.send(WriteCommand::Barrier(ack)).is_err() {
            return Some("run event writer stopped".into());
        }
        result
            .await
            .unwrap_or_else(|_| Some("run event writer stopped".into()))
    }

    fn step(&self, step: FlowRunStep) {
        let data = serde_json::to_value(&step).unwrap_or(Value::Null);
        let _ = self.sender.send(WriteCommand::Step { step, data });
    }
}

pub(super) struct Observer {
    writer: EventWriter,
    steps: Arc<Mutex<Vec<FlowRunStep>>>,
}

impl Observer {
    pub fn new(writer: EventWriter, initial_steps: Vec<FlowRunStep>) -> Self {
        Self {
            writer,
            steps: Arc::new(Mutex::new(initial_steps)),
        }
    }

    pub fn steps(&self) -> Vec<FlowRunStep> {
        self.steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn progress_sink(&self) -> RunProgressSink {
        let writer = self.writer.clone();
        Arc::new(move |kind, data| writer.emit(kind, data))
    }

    pub fn graph_journal(&self) -> Arc<dyn GraphEventJournal> {
        Arc::new(RunGraphJournal {
            inner: Arc::new(InMemoryGraphEventJournal::new()),
            writer: self.writer.clone(),
        })
    }
}

struct RunGraphJournal {
    inner: Arc<InMemoryGraphEventJournal>,
    writer: EventWriter,
}

#[async_trait::async_trait]
impl GraphEventJournal for RunGraphJournal {
    async fn append(&self, observation: GraphObservation) -> tinyflows::graph::Result<u64> {
        let offset = self.inner.append(observation.clone()).await?;
        self.writer.graph_observation(&observation);
        Ok(offset)
    }

    async fn read_from(
        &self,
        run_id: &str,
        offset: u64,
    ) -> tinyflows::graph::Result<Vec<GraphObservation>> {
        self.inner.read_from(run_id, offset).await
    }
}

impl RunObserver for Observer {
    fn on_run_start(&self, run_id: &str) {
        self.writer
            .emit("engine_started", json!({"engine_run_id": run_id}));
    }

    fn on_step_start(&self, node_id: &str) {
        self.writer
            .emit("step_started", json!({"node_id": node_id}));
    }

    fn on_step_finish(&self, step: &ExecutionStep) {
        let status = match &step.status {
            StepStatus::Success => "success",
            StepStatus::Error => "error",
        };
        let step = FlowRunStep {
            node_id: step.node_id.clone(),
            output: step.output.clone(),
            port: None,
            status: Some(status.into()),
            duration_ms: Some(u64::try_from(step.duration_ms).unwrap_or(u64::MAX)),
            diagnostics: step
                .diagnostics
                .iter()
                .map(|diagnostic| serde_json::to_value(diagnostic).unwrap_or(Value::Null))
                .collect(),
        };
        record_step(&self.steps, step.clone());
        self.writer.step(step);
    }

    fn on_run_finish(&self, run: &Run) {
        let status = match &run.status {
            RunStatus::Running => "running",
            RunStatus::Completed => "completed",
            RunStatus::CompletedWithErrors => "completed_with_errors",
            RunStatus::Failed => "failed",
        };
        self.writer.emit(
            "engine_settled",
            json!({"engine_run_id": run.id,"status":status}),
        );
    }
}

fn record_step(steps: &Mutex<Vec<FlowRunStep>>, step: FlowRunStep) {
    let mut steps = steps
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = steps
        .iter_mut()
        .find(|existing| existing.node_id == step.node_id)
    {
        *existing = step;
    } else {
        steps.push(step);
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;

//! A worked host: the loop on a server, the engine on a "device", a relay
//! between them.
//!
//! Run it:
//!
//! ```text
//! cargo run -p tinyflows-adaptive --example service
//! ```
//!
//! Everything here is the real crate driving real serialization — the only
//! stand-ins are the transport (tokio channels where production has a socket)
//! and the model (a script that routes on the `tier` field, where production
//! has an HTTP client). Every seam a production host implements is marked
//! `HOST:`.
//!
//! What it demonstrates, in order:
//!
//! 1. building the tenant handles once and the `Loop` per goal run;
//! 2. a [`Relay`] that serializes a [`RunRequest`], registers a waiter under a
//!    unique wire id, sends the frame, and awaits the report with a deadline —
//!    the exact shape a Socket.IO handler pair implements;
//! 3. the device side: one call to [`serve`] between deserialize and reply;
//! 4. the STORAGE relay — the device-master arrangement over a wire. The
//!    device's own store is the durable workflow home; the service holds no
//!    workflow durably. [`DeviceVault`] is a stateless adapter implementing
//!    [`Vault`]: `load` fetches the device's catalogue at episode start,
//!    and the success-gated flush relays each kept record back as a put the
//!    device writes into its store — where its own surfaces list it;
//! 5. the success gate: the learned workflow reaches the DEVICE only because
//!    the goal run satisfied;
//! 6. the second goal run selecting what the first one learned — read back
//!    from the device, not from anything the service kept.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{Capabilities, LlmProvider};
use tinyflows::error::Result as EngineResult;
use tinyflows::store::types::{WorkflowError, WorkflowRecord};
use tinyflows::store::{HostPolicy, WorkflowStore};
use tinyflows_adaptive::contracts::{Budget, Goal};
use tinyflows_adaptive::driver::{Clock, Loop};
use tinyflows_adaptive::execute::{Relay, Remote, RunReport, RunRequest, Unobserved, serve};
use tinyflows_adaptive::host::HostFacts;
use tinyflows_adaptive::inventory;
use tinyflows_adaptive::ledger::memory::MemoryLedger;
use tinyflows_adaptive::ledger::{EpisodeStatus, Ledger};
use tinyflows_adaptive::workflows::memory::MemoryVault;
use tinyflows_adaptive::workflows::{Snapshot, Vault};
use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------------------
// The relay — the piece this example exists to show.
// ---------------------------------------------------------------------------

/// Carries a [`RunRequest`] to wherever the engine is and correlates the
/// [`RunReport`] that comes back.
///
/// The pattern, independent of transport:
///
/// * **dispatch**: mint a unique wire id, register a oneshot waiter under it,
///   serialize, send, await with a deadline. The wire id is minted *here*
///   rather than trusting the request's own `attempt_id`, so two concurrent
///   episodes — or a retry racing a late reply — can never resolve each
///   other's waiters.
/// * **deliver**: parse the frame, look up the waiter by the echoed id,
///   resolve it. In production this body *is* your socket receive handler.
/// * **deadline**: return `Err` with a readable reason. [`Remote`] turns that
///   into a judgeable attempt rather than a crash — a device asleep is a fact
///   about the run, not an exception.
struct ChannelRelay {
    /// HOST: `socket.emit("tinyflows:flow_run", frame)`.
    to_device: mpsc::Sender<String>,
    waiting: Mutex<HashMap<String, oneshot::Sender<RunReport>>>,
    sequence: AtomicU64,
    deadline: Duration,
}

impl ChannelRelay {
    fn new(to_device: mpsc::Sender<String>, deadline: Duration) -> Arc<Self> {
        Arc::new(Self {
            to_device,
            waiting: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(0),
            deadline,
        })
    }

    /// HOST: the body of your `socket.on("tinyflows:flow_result", …)` handler.
    fn deliver(&self, frame: &str) {
        let Ok(report) = serde_json::from_str::<RunReport>(frame) else {
            eprintln!("   ! dropped an unparseable report frame");
            return;
        };
        let waiter = self
            .waiting
            .lock()
            .expect("waiter lock")
            .remove(&report.attempt_id);
        match waiter {
            Some(tx) => {
                let _ = tx.send(report);
            }
            // A reply after its deadline, or a duplicate. Log and drop — the
            // dispatch side already synthesized an unreported attempt.
            None => eprintln!("   ! late or unknown report `{}`", report.attempt_id),
        }
    }
}

#[async_trait]
impl Relay for ChannelRelay {
    async fn dispatch(&self, request: &RunRequest) -> Result<RunReport, String> {
        // A unique wire id per dispatch. The loop's own attempt_id is not
        // unique enough: attempts within an episode share it, and a late
        // report from attempt 1 must not resolve attempt 2's waiter.
        let wire_id = format!(
            "{}#{}",
            request.attempt_id,
            self.sequence.fetch_add(1, Ordering::Relaxed)
        );
        let mut framed = request.clone();
        framed.attempt_id = wire_id.clone();
        let frame = serde_json::to_string(&framed).map_err(|e| e.to_string())?;
        println!("   → RunRequest  {}", peek(&frame));

        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .expect("waiter lock")
            .insert(wire_id.clone(), tx);

        if self.to_device.send(frame).await.is_err() {
            self.waiting.lock().expect("waiter lock").remove(&wire_id);
            return Err("no device connected".to_string());
        }

        match tokio::time::timeout(self.deadline, rx).await {
            Ok(Ok(mut report)) => {
                println!(
                    "   ← RunReport   {} steps, failed: {:?}",
                    report.steps.len(),
                    report.failed
                );
                // Hand the loop back its own id; the wire salt was ours.
                report.attempt_id = request.attempt_id.clone();
                Ok(report)
            }
            Ok(Err(_)) => Err("the delivery side dropped the waiter".to_string()),
            Err(_) => {
                self.waiting.lock().expect("waiter lock").remove(&wire_id);
                Err(format!("no report within {:?}", self.deadline))
            }
        }
    }
}

fn peek(frame: &str) -> String {
    let head: String = frame.chars().take(88).collect();
    format!("{head}… ({} bytes)", frame.len())
}

// ---------------------------------------------------------------------------
// The device. In production this is medulla behind the socket.
// ---------------------------------------------------------------------------

/// Deserialize, [`serve`], serialize. That is the whole device obligation.
fn spawn_device(mut from_server: mpsc::Receiver<String>, to_server: mpsc::Sender<String>) {
    tokio::spawn(async move {
        // HOST: the device's real Capabilities — its harness behind
        // `AgentRunner`, its HTTP client, its sandboxed code runner. The mock
        // bundle keeps this example self-contained.
        let caps = mock_capabilities();
        while let Some(frame) = from_server.recv().await {
            let Ok(request) = serde_json::from_str::<RunRequest>(&frame) else {
                continue;
            };
            // HOST: a real Workspace here (git mark / git diff) is what fills
            // the `changed` evidence the judge reads.
            let report = serve(&request, &caps, &Unobserved).await;
            let Ok(reply) = serde_json::to_string(&report) else {
                continue;
            };
            let _ = to_server.send(reply).await;
        }
    });
}

include!("service/runtime.rs");

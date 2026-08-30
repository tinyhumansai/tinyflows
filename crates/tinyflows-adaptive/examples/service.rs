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

// ---------------------------------------------------------------------------
// Inference. In production: an HTTP client routing `tier` → model.
// ---------------------------------------------------------------------------

/// A script standing where the model client goes. The one production-relevant
/// thing about it is the match: every request carries `tier`, and routing on
/// it — select to a cheap model, judge to a strong one — is host config, not
/// crate code.
struct TierRouter;

#[async_trait]
impl LlmProvider for TierRouter {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> EngineResult<Value> {
        let shown = request["messages"][1]["content"]
            .as_str()
            .unwrap_or_default();
        Ok(match request["tier"].as_str().unwrap_or_default() {
            // Reads the candidate listing it was shown, like a real selector.
            "select" => {
                let first = shown
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("- id: "));
                json!({
                    "workflow_id": first,
                    "why": "matches the goal",
                    "inputs": { "repo": "acme/rust-lib" },
                })
            }
            // The recipe surface: steps in, never graph syntax — the
            // lowering writes the graph. The ask names no concrete repo;
            // the declared input carries it, which is what lets `keep` file
            // the plan for the next repository.
            "author" => json!({
                "why": "nothing stored fits yet",
                "declared": [
                    { "name": "repo", "description": "the repository to review", "required": true }
                ],
                "inputs": { "repo": "acme/thing" },
                "steps": [{
                    "id": "review",
                    "ask": "Review the open pull requests and post a summary."
                }],
            }),
            "judge" => json!({ "satisfied": true, "gap": "" }),
            "generalise" => json!({
                "name": "Review a repository's pull requests",
                "description": "Reviews the open pull requests on a repository \
                                and posts a summary. Takes the repository as an input.",
                "reusable": true,
            }),
            "consolidate" => json!({ "lessons": [], "corroborate": [] }),
            other => json!({ "error": format!("unexpected tier {other}") }),
        })
    }
}

// ---------------------------------------------------------------------------
// The storage relay — device-master over a wire.
// ---------------------------------------------------------------------------

/// One message of the vault wire, either direction.
///
/// The service sends `Load` / `Put` / `Remove`; the device answers `Records`
/// or `Ack` under the echoed wire id. Same discipline as the run relay: ids
/// are minted service-side, so a late reply can never resolve the wrong
/// waiter.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum VaultFrame {
    /// Service → device: send me your whole catalogue.
    Load { wire_id: String },
    /// Device → service: the catalogue.
    Records {
        wire_id: String,
        records: Vec<WorkflowRecord>,
    },
    /// Service → device: write this record into YOUR store.
    ///
    /// Boxed: a record dwarfs every other variant, and the frame travels
    /// through queues sized for the small ones.
    Put {
        wire_id: String,
        record: Box<WorkflowRecord>,
    },
    /// Service → device: remove this id from your store.
    Remove { wire_id: String, id: String },
    /// Device → service: the write or removal settled.
    Ack {
        wire_id: String,
        error: Option<String>,
    },
}

/// What a vault waiter resolves to.
enum VaultReply {
    Records(Vec<WorkflowRecord>),
    Ack(Option<String>),
}

/// The service's view of the DEVICE's workflow store.
///
/// Stateless on purpose: it holds a sender, a deadline and a counter —
/// never a record. During an episode the [`Snapshot`] is the only
/// service-side copy, and it is memory; when the success gate flushes, each
/// kept workflow crosses this wire and comes to rest in the device's own
/// store, which is the one durable home the design allows it.
struct DeviceVault {
    to_device: mpsc::Sender<String>,
    waiting: Mutex<HashMap<String, oneshot::Sender<VaultReply>>>,
    sequence: AtomicU64,
    deadline: Duration,
}

impl DeviceVault {
    fn new(to_device: mpsc::Sender<String>, deadline: Duration) -> Arc<Self> {
        Arc::new(Self {
            to_device,
            waiting: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(0),
            deadline,
        })
    }

    /// HOST: the body of your `socket.on("tinyflows:vault_reply", …)` handler.
    fn deliver(&self, frame: &str) {
        let Ok(frame) = serde_json::from_str::<VaultFrame>(frame) else {
            eprintln!("   ! dropped an unparseable vault frame");
            return;
        };
        let (wire_id, reply) = match frame {
            VaultFrame::Records { wire_id, records } => (wire_id, VaultReply::Records(records)),
            VaultFrame::Ack { wire_id, error } => (wire_id, VaultReply::Ack(error)),
            // Requests only travel the other way.
            _ => return,
        };
        if let Some(tx) = self.waiting.lock().expect("vault waiters").remove(&wire_id) {
            let _ = tx.send(reply);
        } else {
            eprintln!("   ! late or unknown vault reply `{wire_id}`");
        }
    }

    async fn exchange(&self, mut frame: VaultFrame) -> Result<VaultReply, WorkflowError> {
        let wire_id = format!("vault#{}", self.sequence.fetch_add(1, Ordering::Relaxed));
        match &mut frame {
            VaultFrame::Load { wire_id: id }
            | VaultFrame::Put { wire_id: id, .. }
            | VaultFrame::Remove { wire_id: id, .. } => *id = wire_id.clone(),
            _ => unreachable!("the service only sends requests"),
        }
        let payload = serde_json::to_string(&frame)
            .map_err(|e| WorkflowError::Engine(format!("vault frame: {e}")))?;

        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .expect("vault waiters")
            .insert(wire_id.clone(), tx);

        if self.to_device.send(payload).await.is_err() {
            self.waiting.lock().expect("vault waiters").remove(&wire_id);
            return Err(WorkflowError::Engine("no device connected".to_string()));
        }
        match tokio::time::timeout(self.deadline, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(WorkflowError::Engine(
                "the delivery side dropped the vault waiter".to_string(),
            )),
            Err(_) => {
                self.waiting.lock().expect("vault waiters").remove(&wire_id);
                // Said with the consequence: a flush that cannot reach the
                // device is a REPORTED failure — the learnings ledger is
                // untouched, and nothing pretends the workflow landed.
                Err(WorkflowError::Engine(format!(
                    "the device did not answer within {:?} — the record was NOT stored",
                    self.deadline
                )))
            }
        }
    }
}

#[async_trait]
impl Vault for DeviceVault {
    async fn load(&self) -> Result<Vec<WorkflowRecord>, WorkflowError> {
        match self
            .exchange(VaultFrame::Load {
                wire_id: String::new(),
            })
            .await?
        {
            VaultReply::Records(records) => Ok(records),
            VaultReply::Ack(_) => Err(WorkflowError::Engine(
                "the device answered a load with an ack".to_string(),
            )),
        }
    }

    async fn put(&self, record: &WorkflowRecord) -> Result<(), WorkflowError> {
        println!("   → VaultPut    {}", record.id);
        match self
            .exchange(VaultFrame::Put {
                wire_id: String::new(),
                record: Box::new(record.clone()),
            })
            .await?
        {
            VaultReply::Ack(None) => Ok(()),
            VaultReply::Ack(Some(error)) => Err(WorkflowError::Engine(error)),
            VaultReply::Records(_) => Err(WorkflowError::Engine(
                "the device answered a put with records".to_string(),
            )),
        }
    }

    async fn remove(&self, id: &str) -> Result<(), WorkflowError> {
        match self
            .exchange(VaultFrame::Remove {
                wire_id: String::new(),
                id: id.to_string(),
            })
            .await?
        {
            VaultReply::Ack(None) => Ok(()),
            VaultReply::Ack(Some(error)) => Err(WorkflowError::Engine(error)),
            VaultReply::Records(_) => Err(WorkflowError::Engine(
                "the device answered a remove with records".to_string(),
            )),
        }
    }
}

/// The device's side of the storage relay.
///
/// The whole obligation: deserialize, apply to the DEVICE's own store, reply.
/// In production the store is the device's real tinyflows store — the one its
/// workflow surfaces already list — and this task is your socket handler.
fn spawn_device_store(
    store: Arc<MemoryVault>,
    mut from_server: mpsc::Receiver<String>,
    to_server: mpsc::Sender<String>,
) {
    tokio::spawn(async move {
        while let Some(frame) = from_server.recv().await {
            let Ok(frame) = serde_json::from_str::<VaultFrame>(&frame) else {
                continue;
            };
            let reply = match frame {
                VaultFrame::Load { wire_id } => match store.load().await {
                    Ok(records) => VaultFrame::Records { wire_id, records },
                    Err(e) => VaultFrame::Ack {
                        wire_id,
                        error: Some(e.to_string()),
                    },
                },
                VaultFrame::Put { wire_id, record } => {
                    println!("   ← device stored `{}` in ITS store", record.id);
                    VaultFrame::Ack {
                        wire_id,
                        error: store.put(&record).await.err().map(|e| e.to_string()),
                    }
                }
                VaultFrame::Remove { wire_id, id } => VaultFrame::Ack {
                    wire_id,
                    error: store.remove(&id).await.err().map(|e| e.to_string()),
                },
                // Replies only travel the other way.
                _ => continue,
            };
            let Ok(payload) = serde_json::to_string(&reply) else {
                continue;
            };
            let _ = to_server.send(payload).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Small host pieces.
// ---------------------------------------------------------------------------

struct WallClock;
impl Clock for WallClock {
    fn now(&self) -> String {
        // Opaque to the crate; a real host writes RFC 3339. Zero-padded so the
        // episode listing's string ordering matches time ordering.
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{secs:020}")
    }
}

fn permissive() -> Arc<dyn HostPolicy> {
    #[derive(Debug, Default)]
    struct Permissive;
    impl HostPolicy for Permissive {}
    Arc::new(Permissive)
}

// ---------------------------------------------------------------------------
// The service.
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // ---- process scope: once, at boot -----------------------------------
    // HOST: MongoLedger::connect / SqliteLedger::at_default_location, a real
    // HTTP-backed LlmProvider, real HostFacts from the device's probe.
    let ledger_root = MemoryLedger::new();
    // The DEVICE's workflow store — the one durable home for workflows in
    // this whole topology. In production: the device's real tinyflows store.
    let device_store = Arc::new(MemoryVault::new());
    let caps = Capabilities {
        llm: Arc::new(TierRouter),
        ..mock_capabilities()
    };
    let facts = HostFacts::unknown();

    // The wire: two channel pairs where production has one socket — one for
    // runs, one for the vault.
    let (to_device_tx, to_device_rx) = mpsc::channel::<String>(16);
    let (to_server_tx, mut to_server_rx) = mpsc::channel::<String>(16);
    let relay = ChannelRelay::new(to_device_tx, Duration::from_secs(30));
    spawn_device(to_device_rx, to_server_tx);
    {
        // HOST: this task is your socket receive handler.
        let relay = Arc::clone(&relay);
        tokio::spawn(async move {
            while let Some(frame) = to_server_rx.recv().await {
                relay.deliver(&frame);
            }
        });
    }
    let (vault_tx, vault_rx) = mpsc::channel::<String>(16);
    let (vault_reply_tx, mut vault_reply_rx) = mpsc::channel::<String>(16);
    let vault = DeviceVault::new(vault_tx, Duration::from_secs(30));
    spawn_device_store(Arc::clone(&device_store), vault_rx, vault_reply_tx);
    {
        // HOST: and this one is your vault-reply handler.
        let vault = Arc::clone(&vault);
        tokio::spawn(async move {
            while let Some(frame) = vault_reply_rx.recv().await {
                vault.deliver(&frame);
            }
        });
    }

    // ---- tenant scope: per request, free --------------------------------
    // The ledger stays tenant-scoped on the service: learnings are the
    // service's property. Workflows are the DEVICE's — the vault handle IS
    // the device, so tenancy is which device you are talking to.
    let tenant = "user-7";
    let ledger = ledger_root.for_tenant(tenant);

    // ---- goal run 1: a cold catalogue, so the loop authors --------------
    println!("── goal run 1 · cold start ──");
    run_goal(
        "ep-1",
        &Goal::new("review the open pull requests on acme/thing"),
        &ledger,
        vault.as_ref(),
        &caps,
        &facts,
        &relay,
    )
    .await;

    // ---- goal run 2: the catalogue now holds what run 1 learned ---------
    // Learned from the DEVICE: the service kept nothing between the runs.
    println!("\n── goal run 2 · the loop reuses what the DEVICE now holds ──");
    run_goal(
        "ep-2",
        &Goal::new("review the open pull requests on acme/rust-lib"),
        &ledger,
        vault.as_ref(),
        &caps,
        &facts,
        &relay,
    )
    .await;

    // ---- what the DEVICE holds, and what the trail says -----------------
    // Listed from the device store directly: proof the records came to rest
    // on the device, not in anything the service kept.
    println!("\n── the DEVICE's store ──");
    for record in device_store.load().await.expect("device load") {
        println!("   {} · {}", record.id, record.name);
    }

    println!("\n── the tenant's shelf, read back over the wire ──");
    let snapshot = Snapshot::load(vault.as_ref(), permissive())
        .await
        .expect("load");
    let store: Arc<dyn WorkflowStore> = Arc::new(snapshot);
    for listing in inventory::shelf(&store, &ledger).await.expect("shelf") {
        println!(
            "   {} · {:?} · run {}× satisfied {}× · learned: {}",
            listing.id,
            listing.standing,
            listing.score.applied,
            listing.score.helped,
            listing.learned
        );
    }

    println!("\n── the trail ──");
    for episode in ["ep-1", "ep-2"] {
        for row in ledger.rows(episode).await.expect("rows") {
            println!(
                "   {episode} attempt {} · [{}] → {}",
                row.attempt, row.approach_sig, row.outcome
            );
        }
    }
}

/// One goal run, end to end: fetch the catalogue, drive the loop over the
/// relay, and persist what was learned only if the goal was achieved.
async fn run_goal(
    episode: &str,
    goal: &Goal,
    ledger: &MemoryLedger,
    vault: &DeviceVault,
    caps: &Capabilities,
    facts: &HostFacts,
    relay: &Arc<ChannelRelay>,
) {
    // Fetched fresh each goal run — FROM THE DEVICE, over the wire: the
    // service holds no catalogue of its own to consult.
    let snapshot = Snapshot::load(vault, permissive()).await.expect("load");
    let store: Arc<dyn WorkflowStore> = Arc::new(snapshot.clone());

    let runner = Remote {
        relay: relay.as_ref(),
        attempt_id: episode.to_string(),
    };
    let engine = Loop {
        ledger,
        store: &store,
        caps,
        facts,
        runner: &runner,
        clock: &WallClock,
        budget: Budget::default(),
        conn: None, // HOST: the tenant's credential reference
    };

    let finished = engine.run(episode, goal).await.expect("the loop ran");
    println!(
        "   {episode}: {:?} after {} attempt(s)",
        finished.status, finished.attempts
    );

    // The success gate: the DEVICE only ever receives workflows from goal
    // runs that succeeded — each pending record crosses the wire as a put
    // the device writes into its own store.
    if finished.status == EpisodeStatus::Satisfied && snapshot.pending() > 0 {
        let landed = snapshot.flush(vault).await.expect("flush");
        println!("   flushed {landed} learned workflow(s) to the DEVICE");
    }
}

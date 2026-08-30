use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::{Either, select};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::caps::Capabilities;
use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::interception::{StepAction, StepFrame, StepInterceptor, StepPhase};
use crate::model::{Node, NodeKind};

use super::breakpoint::{Breakpoint, BreakpointId, BreakpointSpec, PauseMode};
use crate::testkit::mocks::MockCaps;

/// How long a pause parks before releasing itself, unless configured otherwise.
///
/// Long enough for a person to think, short enough that a forgotten session
/// does not hold a run task for the life of the process.
pub const DEFAULT_PAUSE_TIMEOUT: Duration = Duration::from_secs(300);

/// A parked activation: everything an inspector can see about where the run
/// stopped.
///
/// `Serialize` because this is exactly what a `flow_debug.inspect` tool call
/// hands back — the JSON an agent reads is this struct, not a projection of it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PauseSnapshot {
    /// Identifies this pause. Required to release it.
    pub pause_id: u64,
    /// The breakpoint that fired, or `None` for a `Step` stop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakpoint: Option<BreakpointId>,
    /// Whether the node is about to run, or has just finished.
    pub phase: &'static str,
    /// The paused node.
    pub node_id: String,
    /// Its kind.
    pub node_kind: NodeKind,
    /// The super-step driving it.
    pub step: usize,
    /// Which activation of this node this is, counting from 1.
    pub activation: u32,
    /// Execution attempts consumed. `0` before the node runs.
    pub attempts: u32,
    /// The parallel lane, when a `scatter` opened one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// The node's resolved input items.
    pub input: Vec<Item>,
    /// The whole run state.
    pub state: Value,
    /// The node's config with every `=`-binding resolved.
    pub resolved_config: Value,
    /// Every binding that resolved to `null`, as `(location, expression)`.
    ///
    /// The first thing to look at on a node that "worked" and did nothing.
    pub null_bindings: Vec<(String, String)>,
    /// What the node produced, at the `after` phase on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<Item>>,
    /// Why it failed, at the `after` phase after exhausted attempts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What to do with a parked activation.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DebugCommand {
    /// Release it and run on.
    Continue,
    /// Release it and break at the very next activation, whatever node that is.
    Step,
    /// Merge `patch` into the run state, then continue. `Before` only.
    Patch(Value),
    /// Emit these items instead of executing / instead of what ran.
    Override {
        /// The items to emit.
        items: Vec<Item>,
        /// The port, or `None` for the default.
        port: Option<String>,
    },
    /// Emit nothing; downstream still runs.
    Skip,
    /// Fail the node, entering its own `on_error` policy.
    Fail(String),
    /// Release it, clear every breakpoint, and let the run finish at speed.
    Detach,
}

/// The stream of pauses as they park.
pub struct PauseStream(mpsc::UnboundedReceiver<PauseSnapshot>);

impl PauseStream {
    /// The next activation to park, or `None` once the controller is gone.
    pub async fn next(&mut self) -> Option<PauseSnapshot> {
        self.0.recv().await
    }
}

#[derive(Default)]
struct Inner {
    breakpoints: Vec<Breakpoint>,
    next_breakpoint: u64,
    next_pause: u64,
    /// Parked activations, keyed by pause id.
    ///
    /// A map rather than a single slot: a `scatter` into eight lanes hitting
    /// one breakpoint parks eight activations at once, and a single slot would
    /// either serialize the super-step invisibly or deadlock it.
    pauses: HashMap<u64, PauseSnapshot>,
    releases: HashMap<u64, oneshot::Sender<DebugCommand>>,
    /// One-shot "break at the next activation", set by `Step`.
    ///
    /// A flag rather than a breakpoint, so stepping does not leave a breakpoint
    /// behind that has to be cleaned up.
    step_next: bool,
    detached: bool,
    /// How many times each node has been activated, for `Condition::Activation`.
    activations: HashMap<String, u32>,
}

/// The breakpoint table and the parked pauses — what the engine talks to, and
/// what another task drives.
///
/// # Nothing can wedge
///
/// A parked activation holds a run task, so every way of getting stuck has to
/// have a way out. There are four, and any one of them frees the run:
///
/// 1. **A pause timeout** ([`DEFAULT_PAUSE_TIMEOUT`], configurable). On expiry
///    the pause releases itself as `Continue` and logs a warning. This is what
///    makes a crashed or forgetful inspector degrade a run to an ordinary run
///    rather than a hung one.
/// 2. **[`detach`](Self::detach)** — clears every breakpoint and releases every
///    parked activation.
/// 3. **A dropped sender** — if the controller's release channel is dropped
///    without a command, the waiting side resolves to `Continue` rather than
///    waiting forever.
/// 4. **Dropping the [`DebugSession`](super::DebugSession)**, which detaches and
///    cancels before it aborts.
///
/// # Why a `std::sync::Mutex`
///
/// Deliberately not a `tokio::sync::Mutex`. Its guard is `!Send`, so *holding
/// the lock across the pause `await` fails to compile* in the engine's `Send`
/// future. The one invariant that would otherwise deadlock every other node in
/// the run is enforced by the compiler rather than by review.
pub struct DebugController {
    inner: Mutex<Inner>,
    pauses_tx: mpsc::UnboundedSender<PauseSnapshot>,
    pause_timeout: Mutex<Option<Duration>>,
    /// Mocks to scope per node, when the session is running against them.
    mocks: Mutex<Option<std::sync::Arc<MockCaps>>>,
}

impl DebugController {
    /// A controller with no breakpoints, and the stream of pauses it will
    /// produce.
    #[must_use]
    pub fn new() -> (std::sync::Arc<Self>, PauseStream) {
        let (pauses_tx, rx) = mpsc::unbounded_channel();
        let controller = std::sync::Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            pauses_tx,
            pause_timeout: Mutex::new(Some(DEFAULT_PAUSE_TIMEOUT)),
            mocks: Mutex::new(None),
        });
        (controller, PauseStream(rx))
    }

    /// Scope capability calls per node against `mocks`, so a paused run's calls
    /// are attributed to the node that made them.
    pub fn use_mocks(&self, mocks: std::sync::Arc<MockCaps>) {
        *self.mocks.lock().expect("mocks lock") = Some(mocks);
    }

    /// Register a breakpoint.
    ///
    /// # Errors
    /// Returns [`EngineError::Capability`] for a [`PauseMode::Durable`]
    /// breakpoint that also breaks *after* a node: a durable pause is a real
    /// interrupt, and the runtime re-runs an interrupted node from the top on
    /// resume, so breaking after one would execute its side effects twice.
    /// Refused at registration rather than surprising someone at run time.
    pub fn set_breakpoint(&self, spec: BreakpointSpec) -> Result<BreakpointId> {
        if spec.mode == PauseMode::Durable && spec.after {
            return Err(EngineError::Capability(
                "a durable breakpoint cannot break *after* a node: resuming re-runs the \
                 interrupted node from the top, which would repeat its side effects. Use \
                 PauseMode::Live for an after-breakpoint."
                    .to_string(),
            ));
        }
        if !spec.before && !spec.after {
            return Err(EngineError::Capability(
                "a breakpoint must break before a node, after it, or both".to_string(),
            ));
        }
        let mut inner = self.inner.lock().expect("controller poisoned");
        let id = BreakpointId(inner.next_breakpoint);
        inner.next_breakpoint += 1;
        inner.breakpoints.push(Breakpoint {
            id,
            spec,
            hits: 0,
            enabled: true,
        });
        Ok(id)
    }

    /// Remove a breakpoint. `false` if it was not registered.
    pub fn clear_breakpoint(&self, id: BreakpointId) -> bool {
        let mut inner = self.inner.lock().expect("controller poisoned");
        let before = inner.breakpoints.len();
        inner.breakpoints.retain(|b| b.id != id);
        inner.breakpoints.len() != before
    }

    /// Every registered breakpoint and its hit count.
    #[must_use]
    pub fn breakpoints(&self) -> Vec<Breakpoint> {
        self.inner
            .lock()
            .expect("controller poisoned")
            .breakpoints
            .clone()
    }

    /// How long a pause parks before releasing itself as `Continue`.
    ///
    /// `None` disables the timeout — the only configuration that can hang a
    /// run, which is why it has to be asked for explicitly rather than being
    /// the default.
    pub fn set_pause_timeout(&self, timeout: Option<Duration>) {
        *self.pause_timeout.lock().expect("timeout lock") = timeout;
    }

    /// Every activation parked right now.
    #[must_use]
    pub fn pauses(&self) -> Vec<PauseSnapshot> {
        let mut pauses: Vec<PauseSnapshot> = self
            .inner
            .lock()
            .expect("controller poisoned")
            .pauses
            .values()
            .cloned()
            .collect();
        pauses.sort_by_key(|p| p.pause_id);
        pauses
    }

    /// One parked activation by id.
    #[must_use]
    pub fn pause(&self, pause_id: u64) -> Option<PauseSnapshot> {
        self.inner
            .lock()
            .expect("controller poisoned")
            .pauses
            .get(&pause_id)
            .cloned()
    }

    /// Release a parked activation.
    ///
    /// # Errors
    /// Returns [`EngineError::Capability`] if `pause_id` names nothing parked —
    /// a stale command from a confused caller, refused rather than silently
    /// dropped so the caller learns its view is out of date.
    pub fn release(&self, pause_id: u64, command: DebugCommand) -> Result<()> {
        let sender = {
            let mut inner = self.inner.lock().expect("controller poisoned");
            if matches!(command, DebugCommand::Step) {
                inner.step_next = true;
            }
            inner.pauses.remove(&pause_id);
            inner.releases.remove(&pause_id)
        };
        match sender {
            // A closed receiver means the activation already gave up (its
            // timeout fired). Report it rather than pretending the command
            // landed.
            Some(sender) => sender.send(command).map_err(|_| {
                EngineError::Capability(format!("pause {pause_id} was already released"))
            }),
            None => Err(EngineError::Capability(format!(
                "no activation is paused as {pause_id}"
            ))),
        }
    }

    /// Clear every breakpoint and release every parked activation.
    ///
    /// After this the controller is inert: further interceptions return
    /// immediately, so the run finishes at full speed with the debugger still
    /// attached but out of the way.
    pub fn detach(&self) {
        let releases = {
            let mut inner = self.inner.lock().expect("controller poisoned");
            inner.detached = true;
            inner.breakpoints.clear();
            inner.step_next = false;
            inner.pauses.clear();
            std::mem::take(&mut inner.releases)
        };
        for (_, sender) in releases {
            // A receiver that already gave up is fine; the activation is
            // running either way.
            let _ = sender.send(DebugCommand::Continue);
        }
    }

    /// Whether the controller has been detached.
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.inner.lock().expect("controller poisoned").detached
    }

    /// Decide whether to break, and register the pause if so.
    ///
    /// Returns the receiver to await on, or `None` to carry on. The lock is
    /// released before the caller awaits — see the type-level note on why that
    /// is enforced by the choice of mutex.
    fn arm(&self, frame: &StepFrame<'_>) -> Option<(u64, oneshot::Receiver<DebugCommand>)> {
        let mut inner = self.inner.lock().expect("controller poisoned");
        if inner.detached {
            return None;
        }

        // Count activations at `Before` only, so a node is not counted twice
        // per run through.
        let activation = {
            let counter = inner.activations.entry(frame.node.id.clone()).or_insert(0);
            if frame.phase == StepPhase::Before {
                *counter += 1;
            }
            *counter
        };

        // A pending `Step` breaks at the next activation regardless of
        // breakpoints, and is consumed whether or not one also matched.
        let stepping = inner.step_next && frame.phase == StepPhase::Before;
        if stepping {
            inner.step_next = false;
        }

        let hit = inner
            .breakpoints
            .iter_mut()
            .filter(|b| b.enabled)
            .find(|b| b.spec.matches(frame, activation))
            .map(|b| {
                b.hits += 1;
                if b.spec.max_hits.is_some_and(|max| b.hits >= max) {
                    b.enabled = false;
                }
                b.id
            });

        if hit.is_none() && !stepping {
            return None;
        }

        let (resolved_config, nulls) = frame.resolved_config();
        let pause_id = inner.next_pause;
        inner.next_pause += 1;
        let snapshot = PauseSnapshot {
            pause_id,
            breakpoint: hit,
            phase: match frame.phase {
                StepPhase::Before => "before",
                StepPhase::After => "after",
            },
            node_id: frame.node.id.clone(),
            node_kind: frame.node.kind.clone(),
            step: frame.step,
            activation,
            attempts: frame.attempts,
            lane: frame.lane.map(|l| l.id.clone()),
            input: frame.input.to_vec(),
            state: frame.state.clone(),
            resolved_config,
            null_bindings: nulls
                .into_iter()
                .map(|n| (n.location, n.expression))
                .collect(),
            output: frame.output.map(|o| o.items.clone()),
            error: frame.error.map(ToString::to_string),
        };

        let (tx, rx) = oneshot::channel();
        inner.pauses.insert(pause_id, snapshot.clone());
        inner.releases.insert(pause_id, tx);
        drop(inner);

        // Best-effort: a session that dropped its stream still gets its pause
        // through `pauses()`, so a failed send is not a reason to refuse to
        // park.
        let _ = self.pauses_tx.send(snapshot);
        Some((pause_id, rx))
    }

    /// Await a decision for a parked activation, fail-open on every path.
    async fn wait(&self, pause_id: u64, rx: oneshot::Receiver<DebugCommand>) -> DebugCommand {
        let timeout = *self.pause_timeout.lock().expect("timeout lock");
        let Some(timeout) = timeout else {
            // No timeout configured: wait indefinitely, but still treat a
            // dropped sender as `Continue` rather than hanging on a channel
            // nobody will ever answer.
            return rx.await.unwrap_or(DebugCommand::Continue);
        };
        let timer = futures_timer::Delay::new(timeout);
        match select(std::pin::pin!(rx), std::pin::pin!(timer)).await {
            Either::Left((decided, _)) => decided.unwrap_or(DebugCommand::Continue),
            Either::Right(((), _)) => {
                tracing::warn!(
                    pause_id,
                    timeout_secs = timeout.as_secs(),
                    "debug pause timed out; continuing the run"
                );
                let mut inner = self.inner.lock().expect("controller poisoned");
                inner.pauses.remove(&pause_id);
                inner.releases.remove(&pause_id);
                DebugCommand::Continue
            }
        }
    }
}

/// Turn a released command into the action the engine obeys.
fn action_for(command: DebugCommand, phase: StepPhase) -> StepAction {
    match command {
        // `Step` has already armed the next stop; this activation just carries
        // on.
        DebugCommand::Continue | DebugCommand::Step | DebugCommand::Detach => {
            StepAction::Continue { state_patch: None }
        }
        DebugCommand::Patch(patch) => match phase {
            StepPhase::Before => StepAction::Continue {
                state_patch: Some(patch),
            },
            // Patching at `After` would misreport what the node saw, so it is
            // dropped rather than half-applied.
            StepPhase::After => StepAction::Continue { state_patch: None },
        },
        DebugCommand::Override { items, port } => StepAction::Replace { items, port },
        DebugCommand::Skip => StepAction::Skip,
        DebugCommand::Fail(message) => StepAction::Fail { message },
    }
}

#[async_trait]
impl StepInterceptor for DebugController {
    async fn intercept(&self, frame: StepFrame<'_>) -> StepAction {
        let phase = frame.phase;
        let Some((pause_id, rx)) = self.arm(&frame) else {
            return StepAction::Continue { state_patch: None };
        };
        let command = self.wait(pause_id, rx).await;
        // Detaching frees every *other* parked activation too, not only this one.
        if matches!(command, DebugCommand::Detach) {
            self.detach();
        }
        action_for(command, phase)
    }

    fn capabilities_for(&self, node: &Node, capabilities: &Capabilities) -> Option<Capabilities> {
        let _ = capabilities;
        self.mocks
            .lock()
            .expect("mocks lock")
            .as_ref()
            .map(|mocks| mocks.capabilities_for_node(&node.id))
    }
}

#[cfg(test)]
#[path = "controller_tests.rs"]
mod tests;

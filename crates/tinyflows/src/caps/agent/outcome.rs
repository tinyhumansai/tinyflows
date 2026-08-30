//! What a host-owned agent loop produced, and why it stopped.
//!
//! Split from `agent.rs`, which owns the *request* half: that file was pushed
//! past the repository's 500-line limit by the constructors below, and the two
//! halves divide cleanly — everything here describes an outcome coming back.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::transcript::TranscriptEntry;

/// Why a host-owned agent loop stopped.
///
/// The reason a typed outcome is worth having. With a bare `Value` return, an
/// agent that finished, one that stopped a step short of the answer, and one
/// waiting on a human are all indistinguishable — and the workflow marches
/// downstream with a partial answer in every case. Keeping the stop reason out
/// of the `Result` channel (rather than reporting a limit or a pause as an
/// error) is the same split [`ShellRunner`](crate::caps::ShellRunner) makes by
/// reporting a non-zero exit through `exit_code` instead of `Err`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stop", rename_all = "snake_case")]
pub enum StopReason {
    /// The agent produced a final answer. The only reason a downstream node
    /// should treat the outcome as complete.
    Finished,
    /// The loop hit a cap and stopped cleanly, keeping what it produced. The
    /// outcome is **partial**: real, usable, and not the whole answer.
    LimitStop {
        /// Host-defined name of the cap that fired (`"max_steps"`,
        /// `"token_budget"`, `"wall_clock"`).
        ///
        /// A free string, not an enum: the engine branches on *whether* a limit
        /// fired, never on which, and an enum here would mint a taxonomy this
        /// crate cannot keep current with any harness's budget model.
        limit: String,
    },
    /// The loop latched a pause and is **resumable, not finished** — the
    /// harness still holds the transcript.
    ///
    /// The engine does not yet route a pause into its checkpoint/resume
    /// machinery: an `agent` node that receives this fails with a clear
    /// [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// naming the node and reason. The variant exists now so a harness can
    /// never *conflate* a pause with a finish, and so no wire type has to
    /// change when resume support lands.
    Paused {
        /// Opaque host handle for the paused run — a session id, a checkpoint
        /// key — which the harness will need echoed back to resume it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// Host-defined reason (`"tool_approval"`, `"clarifying_question"`),
        /// surfaced to whoever is being asked.
        reason: String,
        /// What the human must decide on. Opaque to the engine.
        #[serde(default, skip_serializing_if = "Value::is_null")]
        payload: Value,
    },
}

impl StopReason {
    /// The `snake_case` wire name of this reason, as it appears on the item
    /// envelope's `meta.stop`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::LimitStop { .. } => "limit_stop",
            Self::Paused { .. } => "paused",
        }
    }
}

/// Token and step accounting for one agent run, as the harness reports it.
///
/// The engine neither aggregates nor enforces these; it forwards them onto the
/// item envelope's `meta.usage` for cost-aware workflows to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentUsage {
    /// Prompt tokens consumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Completion tokens produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Model↔tool iterations the loop performed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    /// Tool invocations the loop performed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
}

/// What a host-owned agent run produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRunOutcome {
    /// Why the loop stopped. **Read this before the payload.**
    pub stop: StopReason,
    /// The agent's prose answer, when it produced one. Lands at the item
    /// envelope's `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The structured result, when the agent produced one. Lands at the
    /// envelope's `json`, after the `output_parser` sub-port if one is
    /// configured. [`Value::Null`] when the agent answered only in prose.
    #[serde(default)]
    pub json: Value,
    /// The harness's native payload, verbatim. Lands at the envelope's `raw`,
    /// and is the escape hatch for anything this struct does not model.
    #[serde(default)]
    pub raw: Value,
    /// Optional usage figures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsage>,
    /// What the harness did on the way to this outcome, in order.
    ///
    /// The node's payload above says what came *out* of the agent; this says
    /// what happened *inside* it — the thinking, the tool calls, the results.
    /// The engine copies it onto the step's
    /// [`ExecutionStep::transcript`](crate::observability::ExecutionStep::transcript),
    /// which is how it reaches a
    /// [`RunObserver`](crate::observability::RunObserver) — so a host that
    /// fills this in gets a run history that explains itself instead of one
    /// that only reports pass or fail.
    ///
    /// Empty by default, and empty is a normal outcome: a harness with no
    /// event stream to fold has nothing to say here, and every host that
    /// predates this field keeps compiling and behaving identically. Bound each
    /// entry with [`TranscriptEntry::bounded`] — the engine does not truncate.
    ///
    /// **Settled, not live.** These ride the outcome, so they exist only once
    /// the run is over. Reporting entries *during* a run would need a sink on
    /// this capability, which [`AgentRunRequest`](crate::caps::AgentRunRequest) cannot carry — it is
    /// `Serialize` + `PartialEq` — so that is a deliberate follow-up rather
    /// than something to imply here.
    ///
    /// **Known gap:** an outcome the `agent` node turns into an `Err` — today
    /// [`StopReason::Paused`], which the engine cannot yet resume — loses its
    /// transcript, because the engine's error step is built without a
    /// `NodeOutput` to carry one. That is the run whose transcript is most
    /// worth reading, and closing it means giving the error path somewhere to
    /// put one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transcript: Vec<TranscriptEntry>,
}

impl AgentRunOutcome {
    /// A [`Finished`](StopReason::Finished) outcome built from a harness's
    /// native `value`, deriving `json` and `text` the way the engine's envelope
    /// does: `json` is the value when it is an object or array, and `text` is
    /// the value when it is a string, else its `text` field.
    ///
    /// This is what the default [`AgentRunner::run`](crate::caps::AgentRunner::run) wraps a legacy
    /// [`run_agent`](crate::caps::AgentRunner::run_agent) return in, and the shorthand a
    /// simple adapter wants.
    ///
    /// ```
    /// use tinyflows::caps::{AgentRunOutcome, StopReason};
    /// use serde_json::json;
    ///
    /// let outcome = AgentRunOutcome::finished(json!({ "text": "done", "n": 1 }));
    /// assert_eq!(outcome.stop, StopReason::Finished);
    /// assert_eq!(outcome.text.as_deref(), Some("done"));
    /// assert!(outcome.is_finished());
    /// ```
    #[must_use]
    pub fn finished(value: Value) -> Self {
        let text = match &value {
            Value::String(s) => Some(s.clone()),
            Value::Object(map) => map.get("text").and_then(Value::as_str).map(str::to_string),
            _ => None,
        };
        let json = match &value {
            Value::Object(_) | Value::Array(_) => value.clone(),
            _ => Value::Null,
        };
        Self {
            stop: StopReason::Finished,
            text,
            json,
            raw: value,
            usage: None,
            transcript: Vec::new(),
        }
    }

    /// The same outcome, carrying what the harness did to reach it.
    ///
    /// The builder half of [`transcript`](Self::transcript), so a host can fold
    /// its event stream once and attach it without naming every other field:
    ///
    /// ```
    /// use tinyflows::caps::AgentRunOutcome;
    /// use tinyflows::transcript::TranscriptEntry;
    /// use serde_json::json;
    ///
    /// let outcome = AgentRunOutcome::finished(json!("837799"))
    ///     .with_transcript(vec![
    ///         TranscriptEntry::bounded(1, "agent_thinking", "Collatz — memoise."),
    ///         TranscriptEntry::bounded(2, "tool_call", "shell: python3 solve.py"),
    ///     ]);
    /// assert_eq!(outcome.transcript.len(), 2);
    /// ```
    #[must_use]
    pub fn with_transcript(mut self, transcript: Vec<TranscriptEntry>) -> Self {
        self.transcript = transcript;
        self
    }

    /// A [`LimitStop`](StopReason::LimitStop) outcome: real, usable, partial.
    ///
    /// Beside [`finished`](Self::finished) because `LimitStop` and
    /// [`Paused`](StopReason::Paused) carry data and so had no constructor — a
    /// host reporting either had to write the struct literal, which is what
    /// makes adding a field to this type source-breaking. These two exist so
    /// that migration is one line.
    #[must_use]
    pub fn limit_stop(value: Value, limit: impl Into<String>) -> Self {
        Self {
            stop: StopReason::LimitStop {
                limit: limit.into(),
            },
            ..Self::finished(value)
        }
    }

    /// A [`Paused`](StopReason::Paused) outcome: resumable, not finished.
    ///
    /// See [`limit_stop`](Self::limit_stop) for why this exists.
    #[must_use]
    pub fn paused(token: Option<String>, reason: impl Into<String>, payload: Value) -> Self {
        Self {
            stop: StopReason::Paused {
                token,
                reason: reason.into(),
                payload,
            },
            ..Self::finished(Value::Null)
        }
    }

    /// Whether the agent reached a final answer.
    ///
    /// Anything else means the payload is partial or absent — see
    /// [`StopReason`].
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(self.stop, StopReason::Finished)
    }
}

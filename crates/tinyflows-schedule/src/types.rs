use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum JobType {
    #[default]
    Shell,
    Agent,
    /// A `flows::Flow` schedule trigger binding (issue B2). The job's
    /// `command` column carries the bound flow's id (there is no shell
    /// command / agent prompt to run); on fire the scheduler publishes
    /// `DomainEvent::FlowScheduleTick { flow_id }` instead of running
    /// anything itself — `flows::bus::FlowTriggerSubscriber` does the actual
    /// dispatch. Created by `flows::ops::flows_set_enabled` (via
    /// `cron::add_flow_schedule_job`), never via the `cron_add` agent tool.
    Flow,
}

impl JobType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Agent => "agent",
            Self::Flow => "flow",
        }
    }

    pub fn parse(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("agent") {
            Self::Agent
        } else if raw.eq_ignore_ascii_case("flow") {
            Self::Flow
        } else {
            Self::Shell
        }
    }
}

/// Which session an agent job runs in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionTarget {
    /// A fresh, detached session with no conversation bound to it.
    #[default]
    Isolated,
    /// The host's main session.
    Main,
    /// A detached run whose context and result delivery are bound to the
    /// conversation that created the job ([`CronJob::origin`]).
    Current,
}

impl SessionTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::Main => "main",
            Self::Current => "current",
        }
    }

    /// Case-insensitive; anything unrecognised reads as [`Self::Isolated`].
    pub fn parse(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("main") {
            Self::Main
        } else if raw.eq_ignore_ascii_case("current") {
            Self::Current
        } else {
            Self::Isolated
        }
    }
}

/// The conversation a job was created from, captured at creation time so a
/// run can bind its context to it and route its result back there.
///
/// Serializes internally tagged by `kind` (`"web"` or `"channel"`); absent
/// optional fields are omitted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobOrigin {
    /// Desktop/web chat thread.
    Web {
        /// The chat thread the job was created in.
        thread_id: String,
        /// The agent that owned that thread, when it was not the default one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    /// External messaging channel (Telegram, Discord, Slack, ...).
    Channel {
        /// Channel backend name, e.g. `"telegram"`.
        channel: String,
        /// Where a reply is sent on that channel (chat, room or user id).
        reply_target: String,
        /// The host's conversation-history key for that chat.
        history_key: String,
        /// The sender whose message created the job.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        /// Channel-side thread or topic within the chat.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
    },
}

impl JobOrigin {
    /// The serialized `kind` tag: `"web"` or `"channel"`.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Web { .. } => "web",
            Self::Channel { .. } => "channel",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveHours {
    pub start: String,
    pub end: String,
}

/// A cron-job schedule.
///
/// Serializes as an internally-tagged object (`{"kind": "cron", ...}`).
/// Deserializes from **either** that object form **or** a bare cron-expression
/// string like `"0 9 * * 1"` — the bare-string form is treated as
/// `Schedule::Cron { expr, tz: None, active_hours: None }`.
///
/// The bare-string shorthand exists because agents and some older frontend
/// callers pass `schedule: "0 9 * * 1"` directly instead of the structured
/// object.  Accepting it here prevents Sentry issue CORE-RUST-FY
/// ("invalid type: string, expected internally tagged enum Schedule").
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Schedule {
    Cron {
        /// A standard 5-field crontab (`min hour dom mon dow`, weekdays 0–7
        /// with 0 and 7 = Sunday) or a crate-native 6/7-field expression
        /// with a leading seconds column (weekdays 1–7, 1 = Sunday). See
        /// [`crate::schedule::normalize_expression`].
        expr: String,
        #[serde(default)]
        tz: Option<String>,
        #[serde(default)]
        active_hours: Option<ActiveHours>,
    },
    At {
        at: DateTime<Utc>,
    },
    Every {
        every_ms: u64,
    },
}

impl<'de> Deserialize<'de> for Schedule {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor};
        use std::fmt;

        struct ScheduleVisitor;

        impl<'de> Visitor<'de> for ScheduleVisitor {
            type Value = Schedule;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    f,
                    "a cron-schedule object ({{\"kind\":\"cron\",\"expr\":\"...\"}}) \
                     or a bare cron-expression string"
                )
            }

            /// Accept a bare string as `Schedule::Cron { expr, .. }`.
            /// This handles callers that send `schedule: "0 9 * * 1"` directly
            /// instead of the structured form.
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                tracing::debug!(
                    "[cron] Schedule::deserialize: got bare string '{}', \
                     coercing to Cron variant",
                    value
                );
                Ok(Schedule::Cron {
                    expr: value.to_owned(),
                    tz: None,
                    active_hours: None,
                })
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                tracing::debug!(
                    "[cron] Schedule::deserialize: got bare string '{}', \
                     coercing to Cron variant",
                    value
                );
                Ok(Schedule::Cron {
                    expr: value,
                    tz: None,
                    active_hours: None,
                })
            }

            /// Accept the standard internally-tagged object form.
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                // Delegate to the serde-derived tagged-enum logic by
                // deserializing from a collected map value.
                #[derive(Deserialize)]
                #[serde(tag = "kind", rename_all = "lowercase")]
                enum ScheduleTagged {
                    Cron {
                        expr: String,
                        #[serde(default)]
                        tz: Option<String>,
                        #[serde(default)]
                        active_hours: Option<ActiveHours>,
                    },
                    At {
                        at: DateTime<Utc>,
                    },
                    Every {
                        every_ms: u64,
                    },
                }

                let tagged =
                    ScheduleTagged::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(match tagged {
                    ScheduleTagged::Cron {
                        expr,
                        tz,
                        active_hours,
                    } => Schedule::Cron {
                        expr,
                        tz,
                        active_hours,
                    },
                    ScheduleTagged::At { at } => Schedule::At { at },
                    ScheduleTagged::Every { every_ms } => Schedule::Every { every_ms },
                })
            }
        }

        deserializer.deserialize_any(ScheduleVisitor)
    }
}

/// The [`DeliveryConfig::mode`] values a host understands.
pub mod delivery_mode {
    /// Keep the result in the run history only.
    pub const NONE: &str = "none";
    /// Announce the result on [`DeliveryConfig::channel`] / [`DeliveryConfig::to`].
    pub const ANNOUNCE: &str = "announce";
    /// Surface the result as a proactive message.
    pub const PROACTIVE: &str = "proactive";
    /// Commit the result into the job's origin conversation
    /// ([`CronJob::origin`](super::CronJob::origin)) and send it once there.
    pub const ORIGIN: &str = "origin";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryConfig {
    /// One of the [`delivery_mode`] constants. `"origin"` routes the result
    /// to the conversation recorded in [`CronJob::origin`].
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default = "default_true")]
    pub best_effort: bool,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            mode: delivery_mode::NONE.to_string(),
            channel: None,
            to: None,
            best_effort: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub expression: String,
    pub schedule: Schedule,
    pub command: String,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub job_type: JobType,
    pub session_target: SessionTarget,
    pub model: Option<String>,
    /// Optional built-in agent definition ID (e.g. `"welcome"`,
    /// `"morning_briefing"`). When set, the cron scheduler
    /// resolves the agent definition from the registry and runs with the
    /// definition's prompt, tool allowlist, iteration cap, and model hint
    /// instead of the generic `OpenHumanSessionHost::from_config` path.
    pub agent_id: Option<String>,
    pub enabled: bool,
    pub delivery: DeliveryConfig,
    pub delete_after_run: bool,
    pub created_at: DateTime<Utc>,
    pub next_run: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_status: Option<String>,
    pub last_output: Option<String>,
    /// The conversation that created the job, when it was created from one.
    /// Absent on jobs that predate origin capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<JobOrigin>,
}

/// What happened to a run's result after the run finished.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// The result was delivered to its destination.
    Delivered,
    /// Delivery was requested but deliberately skipped (e.g. empty output).
    Suppressed,
    /// Delivery was attempted and failed.
    Failed,
    /// The job's delivery mode asked for no delivery.
    NotRequested,
}

impl DeliveryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Suppressed => "suppressed",
            Self::Failed => "failed",
            Self::NotRequested => "not_requested",
        }
    }

    /// Case-insensitive; `None` for an unknown value.
    pub fn parse(raw: &str) -> Option<Self> {
        [
            Self::Delivered,
            Self::Suppressed,
            Self::Failed,
            Self::NotRequested,
        ]
        .into_iter()
        .find(|status| raw.eq_ignore_ascii_case(status.as_str()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronRun {
    pub id: i64,
    pub job_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: String,
    pub output: Option<String>,
    pub duration_ms: Option<i64>,
    /// Outcome of delivering this run's result; absent on runs recorded
    /// without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_status: Option<DeliveryStatus>,
}

/// Deserialize a nullable patch field with true double-option semantics:
///
/// | wire            | result        | meaning       |
/// | --------------- | ------------- | ------------- |
/// | key absent      | `None`        | no change     |
/// | key present `null` | `Some(None)`  | clear the value |
/// | key present value  | `Some(Some(v))` | set the value |
///
/// A plain `#[derive(Deserialize)]` on `Option<Option<T>>` collapses the absent
/// and the `null` cases *both* to the outer `None`, so "clear over the wire"
/// (for example, `{"agent_id": null}`) silently deserializes as "no change" — a no-op. Used
/// with `#[serde(default, deserialize_with = "deserialize_double_option")]`,
/// this helper restores the distinction: serde only invokes it when the key is
/// *present*, so a present `null` becomes `Some(None)` and a present value
/// becomes `Some(Some(v))`, while an absent key falls back to the `default`
/// (`None`).
fn deserialize_double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CronJobPatch {
    pub schedule: Option<Schedule>,
    pub command: Option<String>,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub delivery: Option<DeliveryConfig>,
    pub model: Option<String>,
    pub session_target: Option<SessionTarget>,
    pub delete_after_run: Option<bool>,
    /// `Option<Option<String>>` distinguishes "no change" (`None`) from
    /// "clear the agent definition" (`Some(None)`). See
    /// [`deserialize_double_option`] for why the custom deserializer is required
    /// to honor a wire `null` as a clear rather than a silent no-op.
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub agent_id: Option<Option<String>>,
    /// Set (`Some(Some(_))`) or clear (`Some(None)`) the job's origin
    /// conversation; `None` leaves it unchanged (and is omitted when
    /// serialized, so a round trip never turns it into a clear).
    #[serde(
        default,
        deserialize_with = "deserialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub origin: Option<Option<JobOrigin>>,
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;

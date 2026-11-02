//! Pure mappers a host uses to publish and export run observability.
//!
//! The host owns the event bus and the exporter transport; the two mappings
//! here are generic over engine types and carry no I/O:
//!
//! - `step_status_str`: a `StepStatus` as the stable string persisted on a
//!   run step and published in progress events.
//! - `to_exporter_observations`: the engine's durable `GraphObservation`s
//!   re-typed for a trace exporter that names its own, structurally identical
//!   observation type.

use serde::de::DeserializeOwned;

use crate::graph::GraphObservation;
use crate::observability::StepStatus;

/// Stable wire string for a [`StepStatus`] (`"success"` / `"error"`).
///
/// [`StepStatus`] does not derive `PartialEq`, so this matches rather than
/// compares.
pub fn step_status_str(status: &StepStatus) -> &'static str {
    match status {
        StepStatus::Success => "success",
        StepStatus::Error => "error",
    }
}

/// Re-types the engine's observations for an exporter that names its own
/// observation type `T` (for example `tinyagents`' `GraphObservation`).
///
/// The two types share a shape down to the event payload, so this goes through
/// their common serde representation instead of a hand-kept field mapping.
/// An observation that fails to convert is **dropped, not fatal**: exporting
/// is best-effort, and losing one span beats losing the trace. A drop is
/// logged at `warn` because the only cause is the two types drifting apart.
pub fn to_exporter_observations<T: DeserializeOwned>(observations: &[GraphObservation]) -> Vec<T> {
    observations
        .iter()
        .filter_map(
            |obs| match serde_json::to_value(obs).and_then(serde_json::from_value::<T>) {
                Ok(converted) => Some(converted),
                Err(err) => {
                    tracing::warn!(
                        target: "flows::langfuse",
                        error = %err,
                        "[flows] dropped an observation the exporter could not read — \
                         tinyflows and the exporter GraphObservation have drifted"
                    );
                    None
                }
            },
        )
        .collect()
}

#[cfg(test)]
#[path = "observe_tests.rs"]
mod tests;

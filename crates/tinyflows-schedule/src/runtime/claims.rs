//! Process-wide scoped run and loop claims, released on drop and task abort.
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

fn claims() -> &'static Mutex<HashSet<(String, String)>> {
    static CLAIMS: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    CLAIMS.get_or_init(Mutex::default)
}

/// A single-flight claim shared by automatic and manual dispatch.
#[must_use]
pub struct RunClaim((String, String));
impl RunClaim {
    /// Acquire a job claim, or return None when the same scoped job is active.
    pub fn acquire(scope: &str, job_id: &str) -> Option<Self> {
        let key = (scope.to_owned(), job_id.to_owned());
        let acquired = claims()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone());
        acquired.then(|| Self(key))
    }
    /// Whether a run currently holds this scoped job claim.
    pub fn is_running(scope: &str, job_id: &str) -> bool {
        claims()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(scope.into(), job_id.into()))
    }
}
impl Drop for RunClaim {
    fn drop(&mut self) {
        claims()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

fn loops() -> &'static Mutex<HashSet<String>> {
    static LOOPS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    LOOPS.get_or_init(Mutex::default)
}
/// One poll loop per scoped store. Released even when its task is aborted.
#[must_use]
pub struct SchedulerSlot(String);
impl SchedulerSlot {
    /// Acquire the scoped poll-loop slot until this guard drops.
    pub fn acquire(scope: &str) -> Option<Self> {
        loops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(scope.into())
            .then(|| Self(scope.into()))
    }
    /// Whether a poll-loop slot is held in this process.
    pub fn is_running(scope: &str) -> bool {
        loops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(scope)
    }
}
impl Drop for SchedulerSlot {
    fn drop(&mut self) {
        loops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

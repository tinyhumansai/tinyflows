//! Process-local serialization for explicit storage migrations.

use tokio::sync::{Mutex, MutexGuard};

static CATALOG: Mutex<()> = Mutex::const_new(());
static SCHEDULE: Mutex<()> = Mutex::const_new(());

/// Migrations are rare startup operations; serialize all catalog scopes.
pub(super) async fn catalog() -> MutexGuard<'static, ()> {
    CATALOG.lock().await
}

/// Migrations are rare startup operations; serialize all schedule scopes.
pub(super) async fn schedule() -> MutexGuard<'static, ()> {
    SCHEDULE.lock().await
}

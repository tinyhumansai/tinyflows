//! Tests for the out-of-process script runner.
//!
//! These spawn real interpreters, which is the point: the bug this module was
//! written to fix was a calling convention that was documented one way and
//! implemented another, and only actually running something catches that.
//!
//! `bash` is assumed present on unix, and the `ScriptLanguage::Shell` cases are
//! `#[cfg(unix)]` because `run_script` itself refuses that language on
//! Windows (see its doc comment): there is no portable POSIX shell there to
//! run them in, and emulating one is exactly the per-platform behavior this
//! module exists to avoid. `node` and `python3` are cross-platform but not
//! guaranteed installed, so those cases skip rather than fail on a machine
//! without them.

use super::*;
use serde_json::json;

/// The plain shape most cases here want: an inline script, no declared
/// environment. Cases that exercise `cwd`, a script file, or `env` build a
/// [`ScriptRequest`] themselves.
async fn run(
    language: ScriptLanguage,
    source: &str,
    input: &Value,
    timeout: Duration,
    cwd: Option<&Path>,
) -> Result<ScriptOutput> {
    let env = BTreeMap::new();
    run_script(ScriptRequest {
        language,
        interpreter: None,
        source: ScriptSource::Inline(source),
        input,
        timeout,
        cwd,
        env: &env,
    })
    .await
}

const TIMEOUT: Duration = Duration::from_secs(30);

/// Whether an interpreter is on `PATH`, so a test can skip rather than fail.
fn available(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

include!("script_tests/script_tests_part_01_tests.rs");
include!("script_tests/script_tests_part_02_tests.rs");

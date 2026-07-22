//! CLI smoke tests for the tinyflows binary.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn binary_prints_product_name() {
    let output = Command::new(env!("CARGO_BIN_EXE_tinyflows"))
        .output()
        .expect("run tinyflows binary");

    assert!(
        output.status.success(),
        "binary should exit successfully: {output:?}"
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout utf8"),
        "tinyflows\n"
    );
    assert!(
        output.stderr.is_empty(),
        "binary should not write stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn extension_path_and_pairing_commands_are_scriptable() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let state = std::env::temp_dir().join(format!("tinyflows-cli-pair-{unique}"));
    let path = Command::new(env!("CARGO_BIN_EXE_tinyflows"))
        .args(["extension", "path"])
        .env("TINYFLOWS_HOME", &state)
        .output()
        .expect("print extension path");
    assert!(path.status.success());
    let unpacked = PathBuf::from(String::from_utf8(path.stdout).expect("utf8 path").trim());
    assert_eq!(
        unpacked,
        state.join("extension").join(env!("CARGO_PKG_VERSION"))
    );
    assert!(unpacked.join("manifest.json").is_file());
    assert!(unpacked.join("background.js").is_file());

    let paired = Command::new(env!("CARGO_BIN_EXE_tinyflows"))
        .args(["pair", "--state-dir"])
        .arg(&state)
        .output()
        .expect("create pairing token");
    assert!(paired.status.success(), "{paired:?}");
    let stdout = String::from_utf8(paired.stdout).expect("utf8 output");
    assert!(stdout.contains("relay_url=ws://127.0.0.1:32189/v1/extension"));
    let token = stdout
        .lines()
        .find_map(|line| line.strip_prefix("pairing_token="))
        .expect("pairing token");
    assert_eq!(token.len(), 64);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let secret = state.join("credentials/chrome-extension-relay.secret");
        assert_eq!(
            std::fs::metadata(secret).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::remove_dir_all(state).expect("clean temporary state");
}

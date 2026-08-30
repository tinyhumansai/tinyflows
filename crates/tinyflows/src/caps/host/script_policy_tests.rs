//! Tests for the script-step boundary: which files a step may run, which
//! directories it may run in, and which environment it may declare.
//!
//! Filesystem-only and offline — nothing here spawns a process.

use serde_json::json;

use super::{ScriptPolicy, is_valid_env_name, read_env};

/// A workspace with `scripts/build.sh` and a `project/` directory in it.
fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(root.path().join("scripts")).expect("mkdir");
    std::fs::create_dir(root.path().join("project")).expect("mkdir");
    std::fs::write(root.path().join("scripts/build.sh"), "printf built\n").expect("write");
    root
}

fn policy_at(root: &tempfile::TempDir) -> ScriptPolicy {
    ScriptPolicy::new(&root.path().to_string_lossy())
}

/// A path this platform actually considers absolute.
///
/// `/etc/passwd` is *not* absolute on Windows — it has no drive prefix — so a
/// test hard-coding it would take the traversal branch there and assert against
/// the wrong message.
const fn absolute_path() -> &'static str {
    #[cfg(windows)]
    {
        r"C:\Windows\System32\drivers\etc\hosts"
    }
    #[cfg(not(windows))]
    {
        "/etc/passwd"
    }
}

#[test]
fn a_script_in_the_workspace_resolves() {
    let root = workspace();
    let resolved = policy_at(&root)
        .resolve_script("scripts/build.sh")
        .expect("a file in the workspace resolves");

    assert!(resolved.ends_with("scripts/build.sh"));
    assert!(resolved.is_absolute(), "the caller gets a usable path");
}

#[test]
fn a_directory_in_the_workspace_resolves_as_a_working_directory() {
    let root = workspace();
    let resolved = policy_at(&root)
        .resolve_cwd("project")
        .expect("a directory in the workspace resolves");

    assert!(resolved.ends_with("project"));
}

#[test]
fn the_workspace_is_the_default_directory_a_step_runs_in() {
    let root = workspace();
    assert_eq!(policy_at(&root).workspace(), Some(root.path()));
}

#[test]
fn absolute_and_traversing_paths_are_refused() {
    let root = workspace();
    let policy = policy_at(&root);

    for (raw, needle) in [
        (absolute_path(), "must be relative to the workspace"),
        ("../../etc/passwd", "must not traverse outside"),
        ("scripts/../../escape.sh", "must not traverse outside"),
    ] {
        let error = policy
            .resolve_script(raw)
            .expect_err("a path outside the workspace must be refused");
        assert!(error.to_string().contains(needle), "{raw}: {error}");
    }

    let error = policy
        .resolve_cwd("../elsewhere")
        .expect_err("a cwd outside the workspace must be refused");
    assert!(error.to_string().contains("must not traverse outside"));
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_workspace_is_refused() {
    // The syntactic check cannot see this one: the path has no `..` in it, and
    // only canonicalizing reveals where it lands.
    let root = workspace();
    let outside = tempfile::tempdir().expect("tempdir");
    std::fs::write(outside.path().join("secret.sh"), "printf leaked").expect("write");
    std::os::unix::fs::symlink(
        outside.path().join("secret.sh"),
        root.path().join("link.sh"),
    )
    .expect("symlink");

    let error = policy_at(&root)
        .resolve_script("link.sh")
        .expect_err("a symlink out of the workspace must be refused");
    assert!(
        error.to_string().contains("resolves outside the workspace"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_missing_script_is_refused_rather_than_run_empty() {
    let root = workspace();
    let error = policy_at(&root)
        .resolve_script("scripts/absent.sh")
        .expect_err("a missing script must be refused");
    assert!(error.to_string().contains("does not resolve inside"));
}

#[test]
fn a_directory_is_not_a_script_and_a_file_is_not_a_directory() {
    let root = workspace();
    let policy = policy_at(&root);

    let error = policy
        .resolve_script("scripts")
        .expect_err("a directory is not a script");
    assert!(error.to_string().contains("is not a file"));

    let error = policy
        .resolve_cwd("scripts/build.sh")
        .expect_err("a file is not a working directory");
    assert!(error.to_string().contains("is not a directory"));
}

#[test]
fn a_host_without_a_workspace_refuses_paths_and_says_what_to_use_instead() {
    let policy = ScriptPolicy::new("");
    assert_eq!(policy.workspace(), None);

    let error = policy
        .resolve_script("scripts/build.sh")
        .expect_err("no workspace means no path may resolve");
    assert!(
        error.to_string().contains("pass `args.script` instead"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_workspace_that_is_not_a_directory_is_treated_as_absent() {
    let root = tempfile::tempdir().expect("tempdir");
    let file = root.path().join("not-a-dir");
    std::fs::write(&file, "x").expect("write");

    assert_eq!(ScriptPolicy::new(&file.to_string_lossy()).workspace(), None);
}

#[test]
fn a_declared_environment_reads_back_as_a_string_map() {
    let env = read_env(Some(&json!({ "PROFILE": "release", "TARGET": "wasm" })))
        .expect("a string map is valid");

    assert_eq!(env.get("PROFILE").map(String::as_str), Some("release"));
    assert_eq!(env.get("TARGET").map(String::as_str), Some("wasm"));
}

#[test]
fn no_environment_at_all_is_an_empty_map() {
    assert!(read_env(None).expect("absent is valid").is_empty());
}

#[test]
fn a_malformed_environment_is_refused_before_anything_spawns() {
    for (value, needle) in [
        (json!([]), "must be an object"),
        (json!({ "not a name": "x" }), "invalid variable name"),
        (json!({ "COUNT": 3 }), "must be a string, not a number"),
        (json!({ "TOKEN": "a\0b" }), "NUL byte"),
    ] {
        let error = read_env(Some(&value)).expect_err("malformed env must be refused");
        assert!(error.to_string().contains(needle), "{value}: {error}");
    }
}

#[test]
fn environment_names_follow_the_usual_shape() {
    for name in ["PATH", "_private", "A1", "a_b_1"] {
        assert!(is_valid_env_name(name), "{name} should be valid");
    }
    for name in ["", "1ST", "with space", "with-dash", "with=equals", "a\0b"] {
        assert!(!is_valid_env_name(name), "{name} should be invalid");
    }
}

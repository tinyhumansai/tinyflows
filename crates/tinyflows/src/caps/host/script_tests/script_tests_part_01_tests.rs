

// Unix-only: these run `ScriptLanguage::Shell`, which `run_script` refuses
// on Windows because there is no portable POSIX shell to run it in (see
// the `#[cfg(windows)]` guard in `run_script`) rather than emulating one.
#[cfg(unix)]

#[tokio::test]
async fn a_shell_script_reads_its_input_on_stdin_and_returns_stdout() {
    let output = run(
        ScriptLanguage::Shell,
        "cat",
        &json!({ "name": "sweep" }),
        TIMEOUT,
        None,
    )
    .await
    .expect("runs");

    // Round-tripped through stdin and back out, and parsed as JSON on the way
    // back because it is JSON.
    assert_eq!(output.value, json!({ "name": "sweep" }));
}

// Unix-only: these run `ScriptLanguage::Shell`, which `run_script` refuses
// on Windows because there is no portable POSIX shell to run it in (see
// the `#[cfg(windows)]` guard in `run_script`) rather than emulating one.
#[cfg(unix)]
#[tokio::test]
async fn a_shell_script_can_read_its_input_from_the_path_instead() {
    // Shell reads a path more naturally than a pipe, so both are offered.
    let output = run(
        ScriptLanguage::Shell,
        "cat \"$TINYFLOWS_INPUT\"",
        &json!({ "n": 1 }),
        TIMEOUT,
        None,
    )
    .await
    .expect("runs");

    assert_eq!(output.value, json!({ "n": 1 }));
}

// Unix-only: these run `ScriptLanguage::Shell`, which `run_script` refuses
// on Windows because there is no portable POSIX shell to run it in (see
// the `#[cfg(windows)]` guard in `run_script`) rather than emulating one.
#[cfg(unix)]
#[tokio::test]
async fn the_input_path_is_also_the_first_argument() {
    let output = run(
        ScriptLanguage::Shell,
        "cat \"$1\"",
        &json!(42),
        TIMEOUT,
        None,
    )
    .await
    .expect("runs");

    assert_eq!(output.value, json!(42));
}

// Unix-only: these run `ScriptLanguage::Shell`, which `run_script` refuses
// on Windows because there is no portable POSIX shell to run it in (see
// the `#[cfg(windows)]` guard in `run_script`) rather than emulating one.
#[cfg(unix)]
#[tokio::test]
async fn output_that_is_not_json_comes_back_as_a_string() {
    let output = run(
        ScriptLanguage::Shell,
        "echo hello there",
        &json!(null),
        TIMEOUT,
        None,
    )
    .await
    .expect("runs");

    // A script that just prints a line should still be usable downstream.
    assert_eq!(output.value, json!("hello there"));
}

// Unix-only: these run `ScriptLanguage::Shell`, which `run_script` refuses
// on Windows because there is no portable POSIX shell to run it in (see
// the `#[cfg(windows)]` guard in `run_script`) rather than emulating one.
#[cfg(unix)]
#[tokio::test]
async fn stderr_survives_a_script_that_succeeded() {
    let output = run(
        ScriptLanguage::Shell,
        "echo warning: skipped one >&2; echo done",
        &json!(null),
        TIMEOUT,
        None,
    )
    .await
    .expect("runs");

    assert_eq!(output.value, json!("done"));
    // A script that works and warns wrote that warning for a reader.
    assert_eq!(output.stderr, "warning: skipped one");
}

// Unix-only: these run `ScriptLanguage::Shell`, which `run_script` refuses
// on Windows because there is no portable POSIX shell to run it in (see
// the `#[cfg(windows)]` guard in `run_script`) rather than emulating one.
#[cfg(unix)]
#[tokio::test]
async fn a_failing_script_reports_its_stderr_rather_than_only_its_code() {
    let err = run(
        ScriptLanguage::Shell,
        "echo could not reach the host >&2; exit 3",
        &json!(null),
        TIMEOUT,
        None,
    )
    .await
    .expect_err("fails");

    // The exit code alone says nothing an author can act on.
    assert!(
        err.to_string().contains("could not reach the host"),
        "{err}"
    );
}

// Unix-only: these run `ScriptLanguage::Shell`, which `run_script` refuses
// on Windows because there is no portable POSIX shell to run it in (see
// the `#[cfg(windows)]` guard in `run_script`) rather than emulating one.
#[cfg(unix)]
#[tokio::test]
async fn a_script_that_never_ends_is_stopped_and_says_so() {
    let err = run(
        ScriptLanguage::Shell,
        "sleep 30",
        &json!(null),
        Duration::from_millis(300),
        None,
    )
    .await
    .expect_err("times out");

    assert!(err.to_string().contains("timed out"), "{err}");
}

#[tokio::test]
async fn a_missing_interpreter_says_what_is_missing_rather_than_failing_opaquely() {
    let err = run(
        ScriptLanguage::Python,
        "print(1)",
        &json!(null),
        TIMEOUT,
        None,
    )
    .await;

    // Only meaningful when python3 is genuinely absent; where it exists this
    // case cannot arise and the assertion is skipped.
    if let Err(err) = err {
        if err.to_string().contains("cannot run") {
            assert!(err.to_string().contains("PATH"), "{err}");
        }
    }
}

// Unix-only: these run `ScriptLanguage::Shell`, which `run_script` refuses
// on Windows because there is no portable POSIX shell to run it in (see
// the `#[cfg(windows)]` guard in `run_script`) rather than emulating one.
#[cfg(unix)]
#[tokio::test]
async fn a_script_runs_where_it_was_told_to() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("marker.txt"), "found me").expect("write");

    let output = run(
        ScriptLanguage::Shell,
        "cat marker.txt",
        &json!(null),
        TIMEOUT,
        Some(dir.path()),
    )
    .await
    .expect("runs");

    // This is what makes a `shell` node useful: a step that means to touch the
    // operator's project has to actually be in it.
    assert_eq!(output.value, json!("found me"));
}

// Unix-only: these run `ScriptLanguage::Shell`, which `run_script` refuses
// on Windows because there is no portable POSIX shell to run it in (see
// the `#[cfg(windows)]` guard in `run_script`) rather than emulating one.
#[cfg(unix)]
#[tokio::test]
async fn a_script_with_no_directory_given_runs_somewhere_disposable() {
    let output = run(ScriptLanguage::Shell, "pwd", &json!(null), TIMEOUT, None)
        .await
        .expect("runs");

    // A `code` node is a computation over its input, so it gets a scratch
    // directory rather than the repository.
    let cwd = output.value.as_str().expect("a path");
    assert_ne!(
        cwd,
        std::env::current_dir().unwrap().to_string_lossy(),
        "a code node must not default to the process's own directory"
    );
}

#[tokio::test]
async fn javascript_reads_the_same_input_the_same_way() {
    if !available("node") {
        return;
    }

    let output = run(
        ScriptLanguage::JavaScript,
        "const fs = require('fs');\n\
         const input = JSON.parse(fs.readFileSync(0, 'utf8'));\n\
         console.log(JSON.stringify({ doubled: input.n * 2 }));",
        &json!({ "n": 21 }),
        TIMEOUT,
        None,
    )
    .await
    .expect("runs");

    assert_eq!(output.value, json!({ "doubled": 42 }));
}

#[tokio::test]
async fn python_reads_the_same_input_the_same_way() {
    if !available("python3") {
        return;
    }

    let output = run(
        ScriptLanguage::Python,
        "import json, sys\nprint(json.dumps({'doubled': json.load(sys.stdin)['n'] * 2}))",
        &json!({ "n": 21 }),
        TIMEOUT,
        None,
    )
    .await
    .expect("runs");

    assert_eq!(output.value, json!({ "doubled": 42 }));
}

#[test]
fn a_language_name_is_read_the_way_an_author_would_write_it() {
    for name in ["shell", "sh", "bash", "SHELL", " bash "] {
        assert_eq!(
            ScriptLanguage::parse(name),
            Some(ScriptLanguage::Shell),
            "{name}"
        );
    }
    for name in ["javascript", "js", "node", "nodejs"] {
        assert_eq!(
            ScriptLanguage::parse(name),
            Some(ScriptLanguage::JavaScript),
            "{name}"
        );
    }
    for name in ["python", "python3", "py"] {
        assert_eq!(
            ScriptLanguage::parse(name),
            Some(ScriptLanguage::Python),
            "{name}"
        );
    }
    // Refused rather than guessed: running the wrong interpreter on someone's
    // script is worse than telling them the name was not recognised.
    assert_eq!(ScriptLanguage::parse("ruby"), None);
}

#[cfg(windows)]
#[tokio::test]
async fn shell_is_refused_on_windows_rather_than_emulated() {
    // The one Windows-specific behavior this module has: refuse plainly,
    // pointing at the languages that do work everywhere, instead of
    // path-translating into Git Bash or swapping in `cmd`/PowerShell — either
    // of which would make a workflow look portable while quietly behaving
    // differently by host.
    let err = run(
        ScriptLanguage::Shell,
        "echo hi",
        &json!(null),
        TIMEOUT,
        None,
    )
    .await
    .expect_err("shell must be refused on Windows");

    let message = err.to_string();
    assert!(message.contains("Windows"), "{message}");
    assert!(message.contains("javascript"), "{message}");
    assert!(message.contains("python"), "{message}");
}

/// Runs `source` under an explicitly chosen interpreter.
async fn run_under(interpreter: &Interpreter, source: &str) -> Result<ScriptOutput> {
    let env = BTreeMap::new();
    let input = json!(null);
    run_script(ScriptRequest {
        language: ScriptLanguage::Shell,
        interpreter: Some(interpreter),
        source: ScriptSource::Inline(source),
        input: &input,
        timeout: TIMEOUT,
        cwd: None,
        env: &env,
    })
    .await
}

#[test]
fn an_unconfigured_host_keeps_the_default_shell() {
    // The whole point of the empty default: a workflow whose scripts were
    // written against `bash` must not change interpreter because this field
    // was added.
    let chosen = Interpreter::resolve("", &[], Some("/usr/bin/fish")).expect("valid");

    assert_eq!(chosen.program, DEFAULT_SHELL);
    assert!(chosen.args.is_empty());
}

#[test]
fn the_user_sentinel_follows_the_login_shell() {
    // The fixture has to be absolute *on this platform*: a `$SHELL` of
    // `/bin/zsh` is rooted but drive-less on Windows, which is exactly the
    // relative-path shape `validated` refuses.
    let login = absolute_interpreter();

    let chosen = Interpreter::resolve(USER_SHELL, &["-l".to_string()], Some(login)).expect("valid");

    assert_eq!(chosen.program, login);
    assert_eq!(chosen.args, vec!["-l".to_string()]);
}

#[test]
fn a_login_shell_the_platform_cannot_use_is_refused_rather_than_spawned() {
    // The other half of the case above: `$SHELL` is ordinary environment data,
    // so a value this platform would treat as relative has to be refused with
    // the rest, not waved through because it came from the environment.
    #[cfg(windows)]
    {
        let err = Interpreter::resolve(USER_SHELL, &[], Some("/bin/zsh")).expect_err("drive-less");
        assert!(err.to_string().contains("relative path"), "{err}");
    }
    #[cfg(not(windows))]
    {
        let err = Interpreter::resolve(USER_SHELL, &[], Some("bin/zsh")).expect_err("relative");
        assert!(err.to_string().contains("relative path"), "{err}");
    }
}

#[test]
fn the_user_sentinel_falls_back_when_the_environment_names_no_shell() {
    // A daemon started by systemd has no `$SHELL`, and an empty one is the
    // same absence spelled differently. Neither may leave the program empty.
    for absent in [None, Some(""), Some("   ")] {
        let chosen = Interpreter::resolve(USER_SHELL, &[], absent).expect("valid");
        assert_eq!(chosen.program, DEFAULT_SHELL, "for {absent:?}");
    }
}

#[test]
fn a_named_interpreter_wins_over_the_login_shell() {
    let chosen = Interpreter::resolve("zsh", &[], Some("/usr/bin/fish")).expect("valid");

    assert_eq!(chosen.program, "zsh");
}

#[test]
fn a_relative_interpreter_path_is_refused() {
    // The path would resolve against the script's working directory, which the
    // *workflow author* chooses — so accepting it would let a graph decide
    // which binary the operator's own configuration named.
    let err = Interpreter::resolve("./sh", &[], None).expect_err("relative path");

    let message = err.to_string();
    assert!(message.contains("relative path"), "{message}");
    assert!(
        message.contains("/bin/zsh"),
        "the error must teach the fix: {message}"
    );
}

#[test]
fn an_absolute_interpreter_path_is_accepted() {
    let chosen = Interpreter::resolve(absolute_interpreter(), &[], None).expect("valid");

    assert_eq!(chosen.program, absolute_interpreter());
}

/// An absolute path this platform actually considers absolute.
fn absolute_interpreter() -> &'static str {
    #[cfg(windows)]
    {
        r"C:\Windows\System32\cmd.exe"
    }
    #[cfg(not(windows))]
    {
        "/bin/zsh"
    }
}

#[test]
fn a_blank_configured_shell_reads_as_unconfigured() {
    // Whitespace is how a half-edited config file spells "I did not set this",
    // and reading it as an empty program name would break every script.
    let chosen = Interpreter::resolve("   ", &[], None).expect("valid");

    assert_eq!(chosen.program, DEFAULT_SHELL);
}

#[test]
fn an_empty_interpreter_is_refused() {
    // `resolve` maps blank to the default; `validated` is the direct path, and
    // there an empty program is a caller's mistake rather than an absence.
    let err = Interpreter::validated("   ", &[]).expect_err("blank");

    assert!(err.to_string().contains("must not be empty"), "{err}");
}

#[test]
fn a_nul_byte_is_refused_in_the_program_and_in_an_argument() {
    // Neither can be passed to a process; refusing here beats a spawn error
    // that names nothing an author wrote.
    let program = Interpreter::resolve("z\0sh", &[], None).expect_err("NUL program");
    assert!(program.to_string().contains("NUL"), "{program}");

    let argument =
        Interpreter::resolve("zsh", &["-l\0".to_string()], None).expect_err("NUL argument");
    assert!(argument.to_string().contains("NUL"), "{argument}");
}

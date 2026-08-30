
#[cfg(unix)]
#[tokio::test]
async fn a_chosen_interpreter_actually_runs_the_script() {
    // `sh` rather than `zsh`: every unix host has it, so this pins that the
    // override reaches the spawn rather than that a particular shell exists.
    let chosen = Interpreter::validated("sh", &[]).expect("valid");

    let output = run_under(&chosen, "echo chosen").await.expect("runs");

    assert_eq!(output.value, json!("chosen"));
}

#[cfg(unix)]
#[tokio::test]
async fn interpreter_arguments_reach_the_command_line() {
    // `-x` traces to stderr, which is the observable proof the argument landed
    // *before* the script path rather than being dropped.
    let chosen = Interpreter::validated("sh", &["-x".to_string()]).expect("valid");

    let output = run_under(&chosen, "echo traced").await.expect("runs");

    assert_eq!(output.value, json!("traced"));
    assert!(
        output.stderr.contains("echo traced"),
        "expected an -x trace, got {:?}",
        output.stderr
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_missing_interpreter_names_itself() {
    let chosen = Interpreter::validated("tinyflows-no-such-shell", &[]).expect("valid");

    let err = run_under(&chosen, "echo hi").await.expect_err("missing");

    let message = err.to_string();
    assert!(message.contains("tinyflows-no-such-shell"), "{message}");
    assert!(message.contains("PATH"), "{message}");
}

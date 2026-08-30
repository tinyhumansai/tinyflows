use super::*;

/// Run the script `request` describes, requiring it to succeed.
///
/// # Errors
///
/// Fails when the interpreter is missing, the script exits non-zero, or it
/// outlives `request.timeout`. The message carries the interpreter's own stderr,
/// which is the only thing that says what actually went wrong.
pub async fn run_script(request: ScriptRequest<'_>) -> Result<ScriptOutput> {
    /// Bytes of `stderr` folded into the error message. This message becomes
    /// `RunRecord::error` once the engine surfaces it, which — unlike step
    /// `input`/`output` — is not passed through `bounded_within`; a script
    /// that dumps a large stack trace must not be able to grow that field
    /// without limit.
    const MAX_STDERR_BYTES: usize = 4 * 1024;

    let program = request.program().to_string();
    let completion = run_script_capture(request).await?;
    if completion.exit_code != 0 {
        let stderr = if completion.stderr.len() > MAX_STDERR_BYTES {
            let end = completion
                .stderr
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= MAX_STDERR_BYTES)
                .last()
                .unwrap_or(0);
            format!("{} …[truncated]", &completion.stderr[..end])
        } else {
            completion.stderr.clone()
        };
        return Err(EngineError::Capability(format!(
            "script: {program} exited with {}{}",
            completion.exit_code,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        )));
    }
    Ok(ScriptOutput {
        // Structured when the script printed JSON, the raw text otherwise — a
        // script that just prints a line should still be usable downstream.
        value: serde_json::from_str(&completion.stdout)
            .unwrap_or(Value::String(completion.stdout.clone())),
        stderr: completion.stderr,
    })
}

/// Run the script `request` describes, reporting its exit status rather than
/// treating a non-zero one as a failure.
///
/// # Errors
///
/// Fails when the interpreter cannot be spawned, the request cannot be staged
/// on disk, or the script outlives `request.timeout` — that is, when there is no
/// exit status to report at all.
pub async fn run_script_capture(request: ScriptRequest<'_>) -> Result<ScriptCompletion> {
    use tokio::io::AsyncWriteExt;

    let ScriptRequest {
        language,
        interpreter,
        source,
        input,
        timeout,
        cwd,
        env,
    } = request;

    // Refused rather than emulated. `argv[1]` and `TINYFLOWS_INPUT` are real
    // filesystem paths this host wrote (`C:\...` on Windows), and Git Bash — the
    // only `bash` a Windows host is likely to have — cannot open a Windows path
    // without translating it, which is exactly the kind of per-platform
    // reinterpretation that would make a workflow look portable while quietly
    // behaving differently by host. `javascript` and `python` need no such
    // translation and stay available everywhere their interpreter is.
    #[cfg(windows)]
    if language == ScriptLanguage::Shell {
        return Err(EngineError::Capability(
            "script: shell scripts are not supported on Windows (no portable POSIX shell to \
             run them in); use language: \"javascript\" or \"python\" instead"
                .to_string(),
        ));
    }

    // The extension always comes from the language; only the program is
    // negotiable. A `.sh` staged for `zsh` is still a shell script.
    let (default_program, extension) = language.program();
    let (program, leading_args) = match interpreter {
        Some(chosen) => (chosen.program.as_str(), chosen.args.as_slice()),
        None => (default_program, &[][..]),
    };
    let dir =
        tempfile::tempdir().map_err(|err| EngineError::Capability(format!("script: {err}")))?;

    let script: PathBuf = match source {
        ScriptSource::Inline(source) => {
            let staged = dir.path().join(format!("script.{extension}"));
            std::fs::write(&staged, source)
                .map_err(|err| EngineError::Capability(format!("script: {err}")))?;
            staged
        }
        ScriptSource::File(path) => path.to_path_buf(),
    };

    // The input reaches the script two ways because the languages want
    // different ones: a pipe reads naturally in node and python, a path reads
    // naturally in shell. Writing both costs one small file.
    let input_path = dir.path().join("input.json");
    let body = serde_json::to_vec(input)
        .map_err(|err| EngineError::Capability(format!("script: {err}")))?;
    std::fs::write(&input_path, &body)
        .map_err(|err| EngineError::Capability(format!("script: {err}")))?;

    let mut command = tokio::process::Command::new(program);
    command
        .args(leading_args)
        .arg(&script)
        .arg(&input_path)
        .env(INPUT_ENV, &input_path)
        // Layered after `INPUT_ENV` so a workflow's own declaration wins over
        // the inherited value of the same name — that is what declaring one is
        // for. A host that wants the path under a second name of its own (an
        // older, product-specific spelling its authored workflows already use)
        // puts it here.
        .envs(env)
        .current_dir(cwd.unwrap_or_else(|| dir.path()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Tokio does not kill a child when its future is dropped, so a timeout
        // would otherwise leave an infinite script running forever with nothing
        // holding a handle to it.
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|err| {
        EngineError::Capability(format!(
            "script: cannot run `{program}` ({err}). Is it installed and on PATH?"
        ))
    })?;

    // Writing stdin and draining stdout/stderr happen concurrently, not one
    // after the other: a script that prints before it finishes reading stdin
    // fills its stdout pipe while this side is still blocked in `write_all` on
    // stdin, and neither side would ever unblock the other — a real deadlock,
    // not just a slow path, for any input near the OS pipe buffer size. The
    // writer runs on its own task so `wait_with_output` starts reading
    // immediately; if the child exits without reading all of stdin, the pipe
    // simply closes underneath the writer, which surfaces as a write error we
    // ignore (the exit status and stderr are the story in that case, not this).
    let mut stdin = child.stdin.take();
    let writer = tokio::spawn(async move {
        if let Some(mut stdin) = stdin.take() {
            let _ = stdin.write_all(&body).await;
            let _ = stdin.shutdown().await;
        }
    });
    let output = tokio::time::timeout(timeout, async {
        let output = child.wait_with_output().await;
        // Joined so a slow writer is still bounded by `timeout` above, not left
        // running past the point this function returns.
        let _ = writer.await;
        output
    })
    .await
    .map_err(|_| {
        EngineError::Capability(format!("script: timed out after {}s", timeout.as_secs()))
    })?
    .map_err(|err| EngineError::Capability(format!("script: {program}: {err}")))?;

    Ok(ScriptCompletion {
        // `-1` for a signal: `ExitStatus::code()` is `None` when a process was
        // terminated rather than exiting, and a caller comparing against zero
        // must not read that as success.
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

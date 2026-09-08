use std::process::{Command, Stdio};

use tempfile::TempDir;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tmnotify"))
}

fn private_environment(command: &mut Command, temporary: &TempDir) {
    command
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("HOME", temporary.path())
        .env("XDG_CONFIG_HOME", temporary.path().join("config"))
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .env("XDG_RUNTIME_DIR", temporary.path().join("runtime"));
}

#[test]
fn display_command_outside_tmux_never_guesses_a_server() {
    let temporary = TempDir::new().unwrap();
    let mut command = binary();
    private_environment(&mut command, &temporary);
    let output = command.arg("send").arg("done").output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("tmux target is required"));
}

#[cfg(unix)]
#[test]
fn attention_outside_tmux_rejects_missing_source_before_daemon_start() {
    use std::os::unix::net::UnixListener;

    let temporary = TempDir::new().unwrap();
    let tmux_socket = temporary.path().join("tmux.sock");
    let _listener = UnixListener::bind(&tmux_socket).unwrap();
    let mut command = binary();
    private_environment(&mut command, &temporary);
    let output = command
        .arg("-S")
        .arg(&tmux_socket)
        .args(["send", "--attention", "needs input"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Source Pane"), "{stderr}");
}

#[test]
fn malformed_hook_input_is_always_silent_and_successful() {
    let temporary = TempDir::new().unwrap();
    let mut command = binary();
    private_environment(&mut command, &temporary);
    let mut child = command
        .args(["__hook-event", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child.stdin.take().unwrap().write_all(b"not-json").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn all_server_plain_history_works_without_tmux_and_uses_stdout() {
    let temporary = TempDir::new().unwrap();
    let mut command = binary();
    private_environment(&mut command, &temporary);
    let output = command
        .args(["history", "--all-servers", "--plain"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(
        temporary
            .path()
            .join("state/tmnotify/history.sqlite3")
            .exists()
    );
}

#[test]
fn non_tty_clear_requires_yes_before_deleting() {
    let temporary = TempDir::new().unwrap();
    let mut command = binary();
    private_environment(&mut command, &temporary);
    let output = command
        .args(["history", "clear", "--all", "--all-servers"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --yes"));
}

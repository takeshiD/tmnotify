use std::process::{Command, Stdio};
use std::{
    ffi::OsString,
    fs,
    io::{Read as _, Write as _},
    path::Path,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use tempfile::TempDir;

#[cfg(unix)]
/// Keeps socket fixtures below macOS `SUN_LEN`; callers deliberately retain
/// their ordinary `TempDir` for HOME, configuration, and state path coverage.
fn short_socket_directory() -> TempDir {
    tempfile::Builder::new()
        .prefix("tmnotify-cli-sockets-")
        .tempdir_in("/tmp")
        .expect("create a short, unique RAII-owned Unix socket directory")
}

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

fn write_private(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn config_show_uses_xdg_path_merges_defaults_and_is_stable_without_tmux() {
    let temporary = TempDir::new().unwrap();
    let config = temporary.path().join("config/tmnotify/config.toml");
    write_private(&config, "[toast]\nwidth = 57\n");

    let run = || {
        let mut command = binary();
        private_environment(&mut command, &temporary);
        command.args(["config", "show"]).output().unwrap()
    };
    let first = run();
    let second = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stderr.is_empty());
    assert_eq!(first.stdout, second.stdout);
    let shown = String::from_utf8(first.stdout).unwrap();
    assert!(shown.contains("width = 57"));
    assert!(shown.contains("max_visible = 4"));
    assert!(shown.contains("[hooks.codex]"));
}

#[test]
fn config_show_uses_home_fallback_and_defaults_for_an_absent_file() {
    let temporary = TempDir::new().unwrap();
    let mut fallback = binary();
    private_environment(&mut fallback, &temporary);
    fallback.env_remove("XDG_CONFIG_HOME");
    write_private(
        &temporary.path().join(".config/tmnotify/config.toml"),
        "[toast]\nheight = 8\n",
    );
    let output = fallback.args(["config", "show"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("height = 8"));

    let absent = TempDir::new().unwrap();
    let mut defaults = binary();
    private_environment(&mut defaults, &absent);
    let output = defaults.args(["config", "show"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("width = 42"));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn config_show_rejects_invalid_insecure_and_symlinked_files_on_stderr() {
    let cases = ["invalid", "insecure", "symlink"];
    for case in cases {
        let temporary = TempDir::new().unwrap();
        let config = temporary.path().join("config/tmnotify/config.toml");
        match case {
            "invalid" => write_private(&config, "[toast]\nwidth = 2\n"),
            "insecure" => {
                write_private(&config, "");
                fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
            }
            "symlink" => {
                let target = temporary.path().join("target.toml");
                write_private(&target, "");
                fs::create_dir_all(config.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(target, &config).unwrap();
            }
            _ => unreachable!(),
        }
        let mut command = binary();
        private_environment(&mut command, &temporary);
        let output = command.args(["config", "show"]).output().unwrap();
        assert!(!output.status.success(), "{case}");
        assert!(output.stdout.is_empty(), "{case}");
        assert!(!output.stderr.is_empty(), "{case}");
    }
}

#[cfg(unix)]
#[test]
fn config_reload_requires_an_existing_selected_daemon() {
    use std::os::unix::net::UnixListener;

    let temporary = TempDir::new().unwrap();
    let sockets = short_socket_directory();
    let tmux_socket = sockets.path().join("tmux.sock");
    let _listener = UnixListener::bind(&tmux_socket).unwrap();
    let mut command = binary();
    private_environment(&mut command, &temporary);
    command.env("XDG_RUNTIME_DIR", sockets.path().join("runtime"));
    let output = command
        .arg("-S")
        .arg(tmux_socket)
        .args(["config", "reload", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("daemon is unavailable"));
}

#[cfg(unix)]
#[test]
fn config_reload_reports_plain_and_json_daemon_acknowledgements() {
    use std::os::unix::net::{UnixListener, UnixStream};

    use tmnotify::daemon::runtime::ServerIdentity;
    use tmnotify::platform::{Environment, PlatformPaths};

    let temporary = TempDir::new().unwrap();
    let sockets = short_socket_directory();
    let tmux_socket = sockets.path().join("tmux.sock");
    let _tmux_listener = UnixListener::bind(&tmux_socket).unwrap();
    let environment = Environment::from_pairs([
        (
            OsString::from("HOME"),
            temporary.path().as_os_str().to_owned(),
        ),
        (
            OsString::from("XDG_CONFIG_HOME"),
            temporary.path().join("config").into_os_string(),
        ),
        (
            OsString::from("XDG_STATE_HOME"),
            temporary.path().join("state").into_os_string(),
        ),
        (
            OsString::from("XDG_RUNTIME_DIR"),
            sockets.path().join("runtime").into_os_string(),
        ),
    ]);
    let paths = PlatformPaths::resolve(&environment).unwrap();
    let identity = ServerIdentity::resolve(&tmux_socket).unwrap();
    let daemon_socket = paths.socket_path(identity.server_id()).unwrap();
    fs::create_dir_all(daemon_socket.parent().unwrap()).unwrap();
    let daemon = UnixListener::bind(daemon_socket).unwrap();
    let server = std::thread::spawn(move || {
        for changed in [true, false] {
            let (mut stream, _) = daemon.accept().unwrap();
            reply_to_reload(&mut stream, changed);
        }
    });

    let mut plain = binary();
    private_environment(&mut plain, &temporary);
    plain.env("XDG_RUNTIME_DIR", sockets.path().join("runtime"));
    let output = plain
        .arg("-S")
        .arg(&tmux_socket)
        .args(["config", "reload"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"changed\n");
    assert!(output.stderr.is_empty());

    let mut json = binary();
    private_environment(&mut json, &temporary);
    json.env("XDG_RUNTIME_DIR", sockets.path().join("runtime"));
    let output = json
        .arg("-S")
        .arg(tmux_socket)
        .args(["config", "reload", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({"accepted": true, "changed": false})
    );
    assert!(output.stderr.is_empty());
    server.join().unwrap();

    fn reply_to_reload(stream: &mut UnixStream, changed: bool) {
        let mut request = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).unwrap();
            if byte[0] == b'\n' {
                break;
            }
            request.push(byte[0]);
        }
        let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
        assert_eq!(request["version"], 1);
        assert_eq!(request["type"], "config-reload");
        let response = serde_json::json!({
            "version": 1,
            "request_id": request["request_id"],
            "result": {"accepted": true, "changed": changed},
        });
        serde_json::to_writer(&mut *stream, &response).unwrap();
        stream.write_all(b"\n").unwrap();
    }
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
    let sockets = short_socket_directory();
    let tmux_socket = sockets.path().join("tmux.sock");
    let _listener = UnixListener::bind(&tmux_socket).unwrap();
    let mut command = binary();
    private_environment(&mut command, &temporary);
    command.env("XDG_RUNTIME_DIR", sockets.path().join("runtime"));
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

# tmnotify

`tmnotify` is a local notification center for tmux. It delivers Toast and
Attention notifications to the windows currently viewed by attached clients,
keeps durable History, and can return you to the Source Pane.

```console
tmnotify send --key build "build finished"
tmnotify jump --key build
tmnotify history
```

## Configuration

tmnotify reads one user configuration from
`$XDG_CONFIG_HOME/tmnotify/config.toml`, falling back to
`~/.config/tmnotify/config.toml`. Print the validated effective configuration,
including defaults, without tmux or a running daemon:

```console
tmnotify config show
```

Reload the file into an already-running daemon selected by the same `-L`, `-S`,
or `$TMUX` rules as other display commands:

```console
tmnotify config reload
tmnotify -L work config reload --json
```

The plain acknowledgement is `changed` or `unchanged`; JSON includes
`accepted` and `changed`. Invalid or unsafe files fail without replacing the
daemon's last-known-good configuration. Hook preset changes take effect in
provider files only after an explicit `tmnotify hook sync`.

A complete configuration is:

```toml
[daemon]
idle_timeout = "never"

[queue]
max_pending = 1000

[toast]
position = "top-right"
width = 42
height = 3
timeout = "3s"
max_visible = 4
gap = 1
stack_order = "oldest-first"
body = "first-line"

[toast.animation]
enabled = true
fps = 20
enter_duration = "180ms"
exit_duration = "150ms"
enter_easing = "ease-out"
exit_easing = "ease-in"

[attention]
width = "60%"
minimum_width = 32
minimum_height = 7
capture_all_keys = true
close_on_outside_click = false

[history]
enabled = true
max_entries = 10000

[display]
color = "auto"
unicode = "auto"

[hooks.claude]
preset = "minimal"
enable = []
disable = []

[hooks.codex]
preset = "minimal"
enable = []
disable = []
```

A key binding can invoke the same safe, key-based jump without putting
Notification text in a shell command:

```tmux
bind-key B run-shell 'tmnotify jump --key build'
```

tmnotify does not install or change global tmux key bindings.

## Requirements

- Linux or macOS;
- a tmux build exposing the documented 3.8-compatible floating-pane surface;
- Rust 1.88 or newer when building from source.

The MSRV is 1.88 because the selected Ratatui 0.30 dependency family declares
Rust 1.88. Releases bundle SQLite into the single `tmnotify` executable, so an
external SQLite installation is not required.

## Install from a GitHub Release

Download the archive for your platform, verify its adjacent `.sha256` file,
extract it, and place `tmnotify` on `PATH`.

```console
shasum -a 256 -c tmnotify-VERSION-TARGET.tar.gz.sha256
tar -xzf tmnotify-VERSION-TARGET.tar.gz
install -m 0755 tmnotify-VERSION-TARGET/tmnotify ~/.local/bin/tmnotify
tmnotify --version
```

Release archives are provided for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

## Build from source

```console
cargo build --locked --release
```

tmnotify is one executable and performs no network access, telemetry, or
automatic update checks. See [the implementation design](tmnotify-design.md)
and [ADRs](docs/adr/) for the complete product contract.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)); or
- MIT License ([LICENSE-MIT](LICENSE-MIT)).

at your option.

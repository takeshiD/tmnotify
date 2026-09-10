# tmnotify

`tmnotify` is a local notification center for tmux. It delivers Toast and
Attention notifications to the windows currently viewed by attached clients,
keeps durable History, and can return you to the Source Pane.

```console
tmnotify send --key build "build finished"
tmnotify jump --key build
tmnotify history
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

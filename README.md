# tmnotify

`tmnotify` is a local notification center for tmux. It shows non-blocking
Toasts and Attention Gates in the windows you are looking at, keeps durable
History, and can take you back to the Source Pane that produced a Notification.

![tmnotify Toast, keyed update, Attention, History, and configuration demo](demo/tmnotify.gif)

```console
tmnotify send --key build --timeout never "Building..."
tmnotify update --key build --level success --timeout 3s "Build finished"
tmnotify history
```

tmnotify is one executable. It installs no service or global tmux key binding,
and it has no network access, telemetry, or automatic update checks.

## Supported systems and requirements

- Linux and macOS are officially supported; other Unix systems are best effort.
- tmux must expose the documented 3.8-compatible floating-pane capabilities.
  tmnotify checks capabilities instead of relying only on `tmux -V`.
- A release archive needs no Rust toolchain, external SQLite, `jq`, Python, or
  Node.js. SQLite is bundled.
- Building or installing with Cargo requires Rust 1.88 or newer.

Release archives are built for `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, and
`aarch64-apple-darwin`.

## Install

### Release archive

Set `VERSION` and select one of the targets listed above, then download the
archive and its adjacent checksum from [GitHub Releases].

```console
VERSION=0.1.0
TARGET=x86_64-unknown-linux-gnu
ARCHIVE="tmnotify-$VERSION-$TARGET.tar.gz"
BASE_URL="https://github.com/takeshiD/tmnotify/releases/download/v$VERSION"

curl -LO "$BASE_URL/$ARCHIVE"
curl -LO "$BASE_URL/$ARCHIVE.sha256"
shasum -a 256 -c "$ARCHIVE.sha256"
tar -xzf "$ARCHIVE"
install -d "$HOME/.local/bin"
install -m 0755 "tmnotify-$VERSION-$TARGET/tmnotify" "$HOME/.local/bin/tmnotify"
tmnotify --version
```

Ensure `$HOME/.local/bin` is on `PATH`. On Linux,
`sha256sum -c "$ARCHIVE.sha256"` is an equivalent checksum command.

### Cargo or source

Install the published crate:

```console
cargo install --locked tmnotify
```

Or build a checkout and install that exact source tree:

```console
git clone https://github.com/takeshiD/tmnotify.git
cd tmnotify
cargo install --locked --path .
```

[GitHub Releases]: https://github.com/takeshiD/tmnotify/releases

## First use

Run these commands inside tmux so tmnotify can capture the current Source Pane:

```console
tmnotify doctor
tmnotify send --title tmnotify "Your first Toast"
tmnotify send --attention --title tmnotify "Press Enter to return here"
tmnotify history
```

A Toast never takes focus. An Attention Gate waits without a timeout: Enter
jumps to its Source Pane, while Esc or q dismisses it. `history` opens an
interactive viewer on a terminal; when stdout is redirected it emits plain
rows. Use `history --plain` explicitly in scripts or `history --json` for
untruncated NDJSON.

## Main features

- **Toast:** non-interactive, timed notifications in any of six positions.
  Toasts stack without moving older visible items by default and follow the
  distinct tmux windows viewed by attached clients.
- **Attention Gate:** a focusable, timeout-free prompt to jump to the Source
  Pane or dismiss the Notification. It is an attention mechanism, not an
  approval UI.
- **History:** durable SQLite History shared by the user's per-server daemons.
  It supports an adaptive interactive viewer, plain output, NDJSON, reversible
  Hide, and explicit Clear filters.
- **Source Pane jump:** Enter in an Attention Gate or History returns to the
  pane by stable tmux pane ID. A live keyed Notification also supports
  `tmnotify jump --key KEY`.
- **Local, lazy operation:** the first display request starts one daemon for
  the selected tmux server. No manually supervised daemon is required.

Inside the History viewer, use j/k or the arrow keys to move, Enter to jump,
Space for details, `/` to filter, d to Hide, u to undo the latest Hide, `?` for
help, and q or Esc to close.

## Notification Keys and updates

A Notification Key is a producer-chosen name for one ongoing event; it is not
a tmux key binding or a database ID. Sending changed content with the same live
key updates that Notification and its existing History row. Identical content
is a no-op.

```console
tmnotify send --key deploy --timeout never --title Deploy "Uploading..."
tmnotify update --key deploy --level success --timeout 3s "Deploy complete"
tmnotify jump --key deploy
tmnotify dismiss --key deploy
```

After a keyed Notification closes, reusing its key creates a new Notification.
Notification text and keys are treated as data and are never expanded into
shell or tmux commands. If you choose to bind a known, fixed key yourself:

```tmux
bind-key B run-shell 'tmnotify jump --key build'
```

tmnotify does not install or modify global tmux key bindings.

## Claude Code and Codex hooks

The default `minimal` Hook Preset turns attention, completion, and failure
events into Notifications. Installation is conservative: unrelated provider
configuration is preserved, one rolling backup is kept before a mutation, and
tmnotify never grants provider trust.

```console
tmnotify hook install claude user
tmnotify hook install codex user
tmnotify hook status

# After changing a Hook Preset, update only existing installations:
tmnotify hook sync

tmnotify hook remove claude user
tmnotify hook remove codex user
```

Claude supports `user`, `project`, and `local` scopes. Codex supports `user`
and `project`. If Codex inline TOML hooks coexist with `hooks.json`, install and
sync refuse the mixed representation unless `--allow-mixed` is explicit.
Provider trust remains `unknown`; verify it using the provider's own `/hooks`
interface.

## Doctor

`doctor` is read-only. It checks configuration, tmux capabilities, runtime
permissions, daemon reachability, History writability, and hook ownership/sync.

```console
tmnotify doctor
tmnotify doctor --json
```

Use `-L NAME` or `-S PATH` with tmux-dependent commands when running outside
tmux. tmnotify never guesses a default server in that situation.

## Configuration

tmnotify reads only the user configuration at
`$XDG_CONFIG_HOME/tmnotify/config.toml`, falling back to
`~/.config/tmnotify/config.toml` on both Linux and macOS. CLI options override
the file; unknown fields and out-of-range values are errors.

Show the validated effective configuration as stable TOML. This merges the
user file with built-in defaults and works without tmux or a running daemon:

```console
tmnotify config show
```

The daemon also notices valid file changes and atomically adopts them. To apply
the XDG file immediately to the selected live daemon and wait for an
acknowledgement that reports whether settings changed:

```console
tmnotify config reload
tmnotify -L work config reload --json
```

`config reload` uses the normal tmux target rules: run it inside tmux or select
the server with `-L NAME` or `-S PATH`. It contacts an already-running daemon
and does not start one. The plain acknowledgement on stdout is `changed` or
`unchanged`; `--json` emits `{"accepted":true,"changed":true}` or the same
object with `changed` set to `false`. Invalid, insecure, or symlink-unsafe
configuration fails nonzero with diagnostics on stderr and does not replace the
daemon's last-known-good snapshot. Hook Preset changes affect provider files
only after `tmnotify hook sync`. `config show --json` is intentionally invalid;
the effective configuration format is stable TOML.

A complete configuration with the important defaults is:

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

The XDG state and runtime paths are:

```text
$XDG_STATE_HOME/tmnotify/history.sqlite3
$XDG_STATE_HOME/tmnotify/tmnotify.log
$XDG_RUNTIME_DIR/tmnotify/<server-id>.sock
```

`XDG_STATE_HOME` falls back to `~/.local/state`. If `XDG_RUNTIME_DIR` is unset,
tmnotify uses a private mode-0700 per-user directory under the system temporary
directory. Configuration and private state files use mode 0600.

## Demo and development

The checked-in demo is rendered by VHS 0.11.0 from
[`demo/tmnotify.tape`](demo/tmnotify.tape). Its harness creates a temporary HOME,
XDG directories, Claude/Codex configuration, and a dedicated tmux socket. It
never contacts the default tmux server or real provider files, and its exit trap
stops the temporary server and removes all temporary state.

```console
vhs validate demo/tmnotify.tape
vhs demo/tmnotify.tape
```

See the [release process](docs/release.md), [product design](tmnotify-design.md),
[domain language](CONTEXT.md), and [accepted ADRs](docs/adr/) for the complete
contracts.

## License

Licensed under either Apache License 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or
MIT ([LICENSE-MIT](LICENSE-MIT)), at your option.

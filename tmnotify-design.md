# tmnotify Design

- Status: Accepted for implementation
- Target: tmux 3.8-compatible floating panes
- Implementation: Rust 2024 edition
- Distribution: one executable
- Initial release: the complete MVP described here

This is the canonical implementation design. Domain terms are defined in
[`CONTEXT.md`](./CONTEXT.md), and architectural rationale that should survive
future rewrites is recorded in [`docs/adr/`](./docs/adr/).

## 1. Product contract

`tmnotify` is a notification center for tmux. It delivers a logical
Notification to every distinct tmux window currently viewed by an attached
client, follows those windows while the Notification remains visible, stores
durable History, and can return the user to the Source Pane.

```bash
tmnotify send "build finished"
tmnotify send --attention "Codex needs input"
tmnotify jump --key build
tmnotify history
tmnotify hook install claude
tmnotify hook install codex
tmnotify doctor
```

The user installs one binary. No service file, shell initialization, plugin
runtime, `jq`, Python, Node.js, or manually supervised daemon is required.

### 1.1 Supported systems

- Linux and macOS are officially supported; other Unix systems are best effort.
- tmux compatibility is detected by required capabilities, not just `tmux -V`.
- The documented compatibility target is tmux 3.8 or later.
- tmnotify has no network access, telemetry, or automatic update check.
- Releases are dual-licensed under MIT OR Apache-2.0.

### 1.2 MVP scope

The first release includes the whole usable product:

- Toast and Attention notifications;
- all six placements, styling, timeout, and enter/stay/exit animation;
- Window Displays that follow all attached clients;
- priority queueing, stacking, key-based upsert, dismiss, and bounded overload;
- a lazy daemon per tmux server;
- durable SQLite History, adaptive viewer, plain output, and NDJSON;
- jump to Source Pane;
- Claude Code and Codex hook install, sync, remove, and status;
- configurable hook presets;
- diagnostics, hardening, tests, and release artifacts.

Progress widgets, arbitrary actions, shell lifecycle capture, OSC 133,
desktop mirroring, network transport, cross-host sync, and provider plugins are
outside the MVP.

## 2. Core principles

1. **One binary.** CLI, daemon, renderers, History UI, hook receiver, and
   installer are modes of the same executable.
2. **One live owner per tmux server.** Queue, timers, scheduling, animation,
   key indexes, and Window Displays belong to one daemon in memory.
3. **History is not the live queue.** One user-level SQLite database is shared
   by otherwise independent daemons.
4. **tmux is the display backend.** tmnotify does not implement a multiplexer.
5. **Provider schemas stop at adapters.** Provider event names do not enter the
   scheduler.
6. **Notifications are auxiliary.** Hook failure never changes agent behavior.
7. **Untrusted text is plain text.** It is never interpolated into shell
   commands or emitted as raw terminal control sequences.
8. **Behavior is bounded.** Buffers, retries, queues, shutdown, and request
   caches have explicit limits.

## 3. Domain model

### 3.1 Notification

```rust
struct Notification {
    id: NotificationId, // UUIDv7 newtype
    key: Option<NotificationKey>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    level: Level,
    priority: Priority,
    presentation: Presentation,
    title: String,
    body: String,
    timeout: Timeout,
    source: Option<SourceContext>,
    overrides: PresentationOverrides,
    delivery: DeliveryState,
    close_reason: Option<CloseReason>,
    hidden_at: Option<DateTime<Utc>>,
    last_jumped_at: Option<DateTime<Utc>>,
    metadata: NormalizedMetadata,
}

enum Presentation { Toast, Attention }
enum Level { Info, Success, Warning, Error }
enum Priority { Low, Normal, High, Critical }
enum Timeout { After(Duration), Never }
enum DeliveryState { Pending, Visible, Closed }

enum CloseReason {
    TimedOut,
    Dismissed,
    Jumped,
    RenderSuppressed,
    RenderFailed,
    DaemonInterrupted,
    DaemonStopped,
    ServerEnded,
}
```

Persistence is independent of delivery state. When History is enabled, a row
is created when a request is accepted and updated as the Notification changes.
`Archived` is not a lifecycle state.

`Jumped` is a close reason when an active Notification causes the jump, either
from Attention or `jump --key`. Jumping later from History preserves the
original close reason and updates only `last_jumped_at`. A missing Source Pane
produces a nonfatal UI error and does not rewrite the lifecycle.

### 3.2 Safe text boundary

Title and body become safe plain text at ingress:

- CRLF and CR become LF;
- tabs expand to spaces;
- ESC, C0/C1, DEL, bidirectional override/isolate, and other display controls
  are removed or visibly replaced;
- ANSI styling is not accepted;
- display width is measured in terminal cells, including CJK and emoji;
- History and JSON output contain the normalized value.

The entire IPC request is limited to 64 KiB.

### 3.3 Source Context

```rust
struct SourceContext {
    provider: Option<Provider>,
    provider_session_id: Option<String>,
    tmux_server_id: TmuxServerId,
    session_id: String, // $N
    window_id: String,  // @N
    pane_id: String,    // %N
    cwd: Option<PathBuf>,
    command: Option<String>,
    pane_title: Option<String>,
}
```

Stable tmux IDs are authoritative. Names and indexes are not. Complete argv and
provider transcript paths are not captured. `--no-source` omits the entire
Source Context. A Toast may have no source; Attention without a valid Source
Pane is rejected.

At jump time, tmnotify resolves the pane's current session and window from its
pane ID because it may have moved. Saved session/window IDs are historical
metadata only.

### 3.4 Notification Key

A Notification Key identifies one ongoing occurrence within a tmux server.
Producers own their namespace; adapters use keys such as
`codex:<session-id>:<purpose>`.

For Pending or Visible Notifications:

- `send --key` with a new key creates a Notification;
- identical content is a no-op, reuses the ID, and does not reset timeout;
- changed content updates the same ID and restarts display timeout;
- `update --key` applies specified fields and errors if the live key is absent.

`created_at` remains stable and `updated_at` changes. Once Closed, the same key
creates a new Notification and History row.

A live keyed Notification can be used as a stable direct-jump handle:

```bash
tmnotify jump --key build
```

The command resolves only the live key in the selected tmux server. It requires
a valid Source Context, resolves the Source Pane current location by pane ID,
closes the Notification as Jumped after a successful switch, and closes all of
its Window Displays. A missing live key, absent Source Context, missing pane, or
failed switch is a nonzero direct-command error and leaves the Notification
open. Closed rows remain reachable through History rather than by key, avoiding
ambiguity when a key has been reused.

### 3.5 Agent event model

```rust
enum AgentEventKind {
    NeedsAttention,
    Completed,
    Failed,
    Started,
    Interrupted,
    SubagentCompleted,
    ToolStarted,
    ToolCompleted,
}
```

Only allowlisted normalized metadata is stored. Raw provider JSON, transcript
paths, and arbitrary tool input are not persisted or logged.

## 4. Window delivery model

This model follows [ADR 0002](./docs/adr/0002-broadcast-notifications-to-visible-windows.md).

### 4.1 Attention Windows and Window Displays

The daemon derives the distinct current windows of all attached, non-control
tmux clients. Each eligible window receives one Window Display. Clients viewing
the same window share it.

```text
clients:       c1 -> @1    c2 -> @4    c3 -> @4
window set:          @1          @4
displays:             1           1
Notification:              one lifecycle
History:                   one row
```

Client attach, detach, session switch, and window change trigger
reconciliation. A display removed from one window and added to another is
recreated; Notification identity, timeout, queue position, and History do not
change. Follow recreations do not replay enter animation.

### 4.2 Multi-client Attention action

tmux discards client identity before ordinary pane input reaches the renderer.
When Enter arrives from a shared Attention display, the daemon selects the most
recently active client among those viewing that window as the likely actor.

That client is switched to the Source Pane, the Notification closes as Jumped,
and all its Window Displays close. Clients sharing a tmux session may move
together because current window is session state and active pane is window
state. This is an explicit best-effort limitation.

### 4.3 No clients and partial failures

- With no attached display clients, Notifications remain Pending and Toast
  timeout does not advance.
- The tmnotify control client is never a display client.
- Failure in one window does not tear down successful displays elsewhere.
- Failed windows retry up to three times with short backoff.
- Topology or content change resets that window's retry budget.
- If eligible windows exist and all exhaust retries, close as RenderFailed.
- Toast timeout advances only while content is visible in at least one window.

## 5. Toast behavior

Toasts never steal focus and their display surface is not interactive. A Toast
with a Notification Key can be jumped to from another pane or a user-defined
tmux key binding by invoking `tmnotify jump --key <key>`; otherwise jumping is
done through History. tmnotify does not install or mutate global tmux key
bindings.

```toml
[toast]
position = "top-right"
width = 42
height = 3
timeout = "3s"
max_visible = 4
gap = 1
stack_order = "oldest-first"
body = "first-line"
```

All six placements are supported:

```text
top-left      top-center      top-right
bottom-left   bottom-center   bottom-right
```

Stacks grow inward. By default, the oldest visible Toast stays nearest the
anchor so arrivals do not move existing items. `newest-first` is configurable;
its reflow is immediate, not animated. Pending order is descending Priority and
then FIFO. Priority never preempts or reorders visible Notifications.

### 5.1 Body presentation

- `first-line` (default): first nonempty line, with ellipsis when more exists;
- `join-lines`: line breaks become spaces, then content is truncated;
- `wrap`: wrap within fixed pane height and truncate overflow.

Complete safe content remains in History. Pane height never grows from content.

### 5.2 Narrow windows and capacity

- Normal windows use the bordered 42x3 layout.
- At 24 columns and above, insufficient width uses a borderless one-line form.
- Below 24 columns, that window receives no Toast content display.
- Each window displays what fits, up to `max_visible`.
- Overflow is shown as `+N waiting`.
- A small window does not reduce capacity in larger windows.

Symbols are paired with text/color and have ASCII fallbacks such as `[ok]`,
`[!]`, and `[x]`. Nerd Fonts are not required or auto-detected.

### 5.3 Persistent Toasts

```bash
tmnotify send --key build --timeout never "Building..."
tmnotify update --key build --level success --timeout 3s "Done"
tmnotify dismiss --key build
```

`--timeout never` remains until a finite update or explicit dismiss.

## 6. Attention behavior

Attention is an attention gate, not a provider approval UI. It only jumps to the
Source Pane or dismisses the Notification.

- only one Attention Notification is active globally per daemon;
- it appears in every eligible Attention Window;
- additional Attention requests queue by Priority/FIFO;
- it has no timeout;
- Enter closes every Window Display and jumps;
- Esc or q dismisses without jumping;
- outside click does not close it;
- missing source leaves the gate open with a nonfatal error;
- Toast displays and timeouts pause while Attention is active;
- Toast delivery resumes after Jumped or Dismissed.

The modal closes before selecting an underlying pane.

Responsive layout:

- 60x10 and above: centered, roughly 60% width, wrapped body/source/footer;
- 32x7 through 59x9: nearly full width, compact content, metadata omitted;
- below 32x7: use available space and show
  `terminal too small · Enter jump · Esc dismiss`.

Long content truncates rather than scrolls; History retains the full body.

## 7. Queue, lifecycle, and overload

The daemon owns Notifications by ID, the live key index, Priority/FIFO queues,
one active Attention, the active Toast set, desired/actual Window Displays,
monotonic timers, animation, and the bounded request-result cache.

The pending limit is 1,000 per server. On overflow, retain the best 1,000 by
descending Priority and ascending creation time. Losing items close as
RenderSuppressed and remain in History. Visible Notifications are not preempted.

State transitions receive monotonic time explicitly rather than using a Clock
trait. Display timeout and animation use monotonic time; History uses UTC wall
clock time.

## 8. Animation and style

```toml
[toast.animation]
enabled = true
fps = 20
enter_duration = "180ms"
exit_duration = "150ms"
enter_easing = "ease-out"
exit_easing = "ease-in"
```

Positions are interpolated in cells. Quantized duplicates are not sent. Updates
for all windows in a tick are batched. Reflow animation and color interpolation
are outside the MVP.

Animation is disabled by `enabled = false`, zero duration, or `TERM=dumb`.
Follow recreation does not replay enter animation.

Semantic ANSI 16-color tokens are default; RGB themes are opt-in. Color is never
the sole signal. `NO_COLOR`, `TERM=dumb`, or `color = "never"` uses monochrome.

## 9. Daemon and tmux process model

### 9.1 Lazy ownership

There is one daemon for each canonical tmux server socket. It starts on first
request and lives until the tmux server ends:

```text
resolve tmux socket
  -> connect tmnotify socket
  -> if absent/refused, spawn hidden daemon mode
  -> daemon bind is the ownership primitive
  -> client retries with the same request ID
```

Concurrent starters race on bind; exactly one succeeds. PID files are not the
primary lock. Idle timeout is disabled. Zero attached clients do not stop the
daemon while the tmux server exists.

The daemon socket name is a short cryptographic hash of canonical tmux socket
path plus user ID.

### 9.2 Stale socket recovery

After failed connections, unlink is allowed only if the runtime directory is
current-user-owned and mode 0700, the entry is a Unix socket rather than a
symlink, the socket is current-user-owned, and a final connect still fails.

### 9.3 Control mode

Per [ADR 0003](./docs/adr/0003-observe-tmux-through-control-mode.md), each
daemon keeps one control-mode connection for topology events, correlated command
responses, and batched display operations. Event updates are backed by
low-frequency full reconciliation. The control client is excluded from delivery.

During temporary disconnection, existing displays remain and animation/Toast
timeout pause. Reconnect uses exponential backoff and a full reconciliation.
Only confirmed server loss closes live Notifications as ServerEnded.

### 9.4 Capability gate

Before display service begins, verify the actual command/format/event surface:
floating/modal creation and flags, movement/resizing, stable IDs, and required
control events. `list-commands` may be inspected instead of assuming a version
string. Missing capability prevents daemon startup but not tmux-independent
commands such as plain History or hook status.

### 9.5 Shutdown and recovery

On SIGTERM, SIGINT, or server termination:

1. stop accepting requests;
2. close Window Displays;
3. record DaemonStopped or ServerEnded;
4. flush the History worker;
5. unlink only the owned socket;
6. exit within two seconds.

A second SIGINT exits immediately. After crash/SIGKILL, the next daemon changes
stale Pending/Visible rows to DaemonInterrupted and does not replay them.

## 10. Renderer processes

Per [ADR 0008](./docs/adr/0008-stream-renderer-content-from-the-daemon.md), pane
renderers receive only a Window Display ID and one-time token, never text:

```bash
tmnotify __render-toast --window-display <id> --token <nonce>
tmnotify __render-attention --window-display <id> --token <nonce>
tmnotify __history-ui ...
```

The renderer redeems the token over the user-private daemon socket and keeps the
connection for updates/termination. Tokens are paired with expected IDs,
invalidated on recreation, and never persisted or logged. Stale or mismatched
renderers receive no content.

Toast rendering is purpose-built. Per
[ADR 0004](./docs/adr/0004-use-ratatui-only-for-interactive-views.md), Ratatui
with Crossterm is limited to interactive Attention and History views.

## 11. IPC protocol

Transport is a current-user Unix socket with newline-delimited JSON. Every
request/response includes a protocol version and UUIDv7 request ID.

```json
{
  "version": 1,
  "request_id": "019...",
  "type": "send",
  "notification": {
    "title": "Codex",
    "body": "Implementation completed",
    "level": "success"
  }
}
```

```json
{
  "version": 1,
  "request_id": "019...",
  "accepted": true,
  "notification_id": "019...",
  "history_persisted": true,
  "disposition": "queued"
}
```

Disposition is `queued`, `visible`, `updated`, `duplicate`, or `suppressed`.
ACK means validation, History attempt, and live-state mutation completed; it
does not await rendering or animation.

Unknown fields are ignored. Missing required fields, invalid types, unknown
request types, and unsupported versions are rejected. Malformed input never
crashes the daemon. Requests are at most 64 KiB.

Connections may multiplex requests; IDs, not response order, correlate them.
Normal CLI/hooks use one request. History UI keeps its connection. Buffers,
in-flight requests, and idle time are bounded.

The daemon caches 10,000 request results for ten minutes in an LRU. Repeating an
ID and payload returns the original response; the same ID with different payload
is an error. This cache is not durable across daemon restart.

## 12. Paths and permissions

Per [ADR 0001](./docs/adr/0001-use-xdg-paths-on-linux-and-macos.md), Linux and
macOS use one XDG policy:

```text
$XDG_RUNTIME_DIR/tmnotify/<server-id>.sock
$XDG_STATE_HOME/tmnotify/history.sqlite3
$XDG_STATE_HOME/tmnotify/tmnotify.log
$XDG_CONFIG_HOME/tmnotify/config.toml
```

Fallbacks:

```text
XDG_CONFIG_HOME -> ~/.config
XDG_STATE_HOME  -> ~/.local/state
XDG_RUNTIME_DIR -> <system-temp>/tmnotify-<uid>/, mode 0700
```

Private state/config files use mode 0600. Runtime validation does not follow
attacker-controlled symlinks.

## 13. History

Per [ADR 0005](./docs/adr/0005-share-one-history-database-across-daemons.md),
one SQLite database is shared by per-server daemons. It uses WAL, versioned
migrations, short transactions, bounded busy retries, and a dedicated blocking
worker per daemon. DB work never blocks scheduling or animation.

History is enabled by default and supports `history.enabled = false`. Intentional
disablement is not a warning. Display may succeed when persistence fails; direct
`send` exits zero with a stderr warning, while hook mode logs privately.

### 13.1 Retention

- maximum 10,000 rows across all servers;
- hidden rows count and are removed oldest-first before visible rows;
- default scope is the current tmux server;
- default sort is `updated_at DESC`;
- Hide is reversible; Clear physically deletes.

### 13.2 Interactive viewer

Inside tmux, History opens in a modal floating pane. Outside tmux it uses the
current terminal and requires `-L`, `-S`, or `--all-servers`.

When `--all-servers` is active, selecting a row from another server requires
confirmation before tmnotify connects to that server and attempts the jump.

- 100+ columns: list and live detail side by side;
- 48-99 columns: one list; Space toggles detail;
- below 48x10: `terminal too small — need 48×10`;
- lists are virtualized and measured in cells.

```text
j/k, Up/Down   move
Enter          jump to Source Pane
Space          toggle detail
/              smart-case filter
d              Hide
u              undo the latest Hide in this UI session
q/Esc          back or close
?              key help
```

Selection uses reverse video plus color. The footer shows only common actions.
Terminal state is restored on every exit path. If Attention is active in the
target window, History refuses to open and explains why.

### 13.3 Plain and machine output

```bash
tmnotify history --plain
tmnotify history --json
```

Plain mode emits one borderless, width-truncated row per Notification. JSON mode
emits untruncated NDJSON. Non-TTY stdout defaults to plain. Results use stdout;
diagnostics use stderr.

### 13.4 Clear safety

```bash
tmnotify history clear --hidden
tmnotify history clear --before 30d
tmnotify history clear --all
tmnotify history clear --all --all-servers
```

Clear requires a selector, shows count/scope, and defaults to No. Non-TTY input
requires `--yes`. `--all-servers` alone is not a selector. Deletion is one
transaction and does not create a full DB backup.

## 14. Configuration

Only user-level tmnotify configuration is loaded. Repositories cannot provide a
`.tmnotify.toml`. Precedence is CLI, defined environment variables, user config,
then built-in defaults.

The daemon checks mtime before requests and during periodic reconciliation. A
valid file atomically replaces the last-known-good snapshot. Theme, placement,
stack, animation, and capacity reconcile active Window Displays immediately.
Invalid changes retain the previous snapshot and log a warning; direct CLI
validation reports an error. Hook preset changes require `hook sync`.

Unknown config fields are errors. Values have bounded ranges.

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

## 15. CLI

```text
tmnotify [-L NAME|-S PATH] send [OPTIONS] <MESSAGE|->
tmnotify [-L NAME|-S PATH] update (--id ID|--key KEY) [OPTIONS] [MESSAGE|-]
tmnotify [-L NAME|-S PATH] dismiss (--id ID|--key KEY)
tmnotify [-L NAME|-S PATH] jump --key KEY
tmnotify [-L NAME|-S PATH] history [--plain|--json|--all|--all-servers]
tmnotify history clear <FILTER> [--yes]
tmnotify hook install <claude|codex> [SCOPE]
tmnotify hook remove <claude|codex> [SCOPE]
tmnotify hook sync [claude|codex]
tmnotify hook status [claude|codex]
tmnotify doctor [--json]
```

`-L/--socket-name` and `-S/--socket-path` mirror tmux and are mutually
exclusive. Inside tmux, resolve from `$TMUX`; outside, display commands require
an explicit target and never guess.

`send -` and `update ... -` read bounded multiline stdin. Successful commands
are silent by default; `--json` emits structured acknowledgement. Errors and
warnings use stderr.

Send defaults are Info, Normal, Toast, and automatic Source capture. Options:

```text
--title
--level <info|success|warning|error>
--priority <low|normal|high|critical>
--attention
--timeout <duration|never>
--position <placement>
--key
--no-source
```

There is no `--modal`; attention describes intent while modal is an
implementation detail. Update/dismiss require explicit `--id` or `--key`.
Direct jump intentionally requires `--key`, addresses only a live Notification
in the selected server, and never guesses a closed History row.

## 16. Provider hooks

Provider integration follows
[ADR 0006](./docs/adr/0006-manage-provider-hooks-conservatively.md).

### 16.1 Receiver contract

Installed hooks invoke only tmnotify and send bounded JSON on stdin. The
receiver normalizes input, captures Source Context, submits, waits at most two
seconds for ACK, and returns empty stdout, empty stderr, and exit zero—even on
failure. Details go to the private log.

Unknown events are ignored; unknown fields are tolerated. Missing required
fields, wrong types, and oversized input are logged and ignored. Hooks are
synchronous so completion notifications are not cancelled during teardown.

The provider module exposes one deep operation equivalent to:

```rust
normalize(provider, bounded_json, hook_policy)
    -> Result<Option<NotificationDraft>, ProviderError>
```

### 16.2 Presets

`minimal` is default: attention, completion, and failure. `normal` adds subagent
completion and interruption. `verbose` may include all safely observable
lifecycle events, including configured tool events. `enable`/`disable` adjust
individual events after preset expansion.

Only enabled events are installed. Config changes require `hook sync`, which
changes only existing tmnotify scopes. Only install creates a new scope.

### 16.3 Claude defaults

```text
PermissionRequest                -> Attention / High / NeedsAttention
Notification(agent_needs_input)  -> Attention / High / NeedsAttention
Stop                             -> Toast / Normal / Completed
StopFailure                      -> Toast / High / Failed
```

Delayed `Notification(permission_prompt)` and duplicative
`Notification(agent_completed)` are excluded.

```text
user     $CLAUDE_CONFIG_DIR/settings.json or ~/.claude/settings.json
project  .claude/settings.json
local    .claude/settings.local.json
```

Claude uses its shell-free absolute `command` plus `args` form.

### 16.4 Codex defaults

```text
PermissionRequest -> Attention / High / NeedsAttention
Stop              -> Toast / Normal / Completed
```

```text
user     ~/.codex/hooks.json
project  <repo>/.codex/hooks.json
```

Codex receives a safely quoted fixed command string containing the absolute
binary path. If inline hooks exist in the same `config.toml` layer, install
refuses unless `--allow-mixed`; tmnotify never edits inline TOML.

### 16.5 Ownership and trust

Installation preserves unrelated JSON fields/handlers and existing key order,
indentation, newline convention, and final newline where practical. It writes a
same-directory temporary file, flushes, and atomically renames it.

Before each real mutation it saves one mode-0600 rolling `.tmnotify.bak`. Parse
failure and idempotent no-op do not write. Repeated install creates one handler.

A handler is owned only when executable basename is `tmnotify`, argv is exactly
`__hook-event <provider>`, and there is no wrapper, redirect, pipe, or extra
operation. Ambiguous handlers are untouched. Sync can therefore update an old
absolute path after relocation.

tmnotify never grants/bypasses provider trust or reads undocumented trust
stores. Neither provider exposes stable machine-readable trust status, so status
shows `trust: unknown — verify in /hooks`.

References:

- <https://code.claude.com/docs/en/hooks>
- <https://code.claude.com/docs/en/settings>
- <https://learn.chatgpt.com/docs/hooks>

Fixtures record source URL and verification date. Golden tests cover
normalization, idempotent install, relocation sync, and preserving remove.

## 17. History schema

Migration version one stores normalized values rather than raw payload:

```sql
CREATE TABLE notifications (
    id TEXT PRIMARY KEY,
    notification_key TEXT,
    tmux_server_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    level TEXT NOT NULL,
    priority TEXT NOT NULL,
    presentation TEXT NOT NULL,
    delivery_state TEXT NOT NULL,
    close_reason TEXT,
    timeout_ms INTEGER,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    provider TEXT,
    provider_session_id TEXT,
    session_id TEXT,
    window_id TEXT,
    pane_id TEXT,
    cwd TEXT,
    command TEXT,
    pane_title TEXT,
    hidden_at INTEGER,
    last_jumped_at INTEGER,
    normalized_metadata_json TEXT NOT NULL
);
```

Indexes cover update-time ordering, server scope, pane ID, and live key lookup.
Exact SQL may evolve without changing the domain contract.

## 18. Logging and diagnostics

Warnings/errors go to private mode-0600 files:

```text
$XDG_STATE_HOME/tmnotify/tmnotify.log
$XDG_STATE_HOME/tmnotify/tmnotify.log.1
```

Rotate around 1 MiB with one generation. Title, body, raw payload, complete
request, renderer token, and transcript data are not logged.

`doctor` is read-only. It checks config, tmux capabilities, runtime permissions,
daemon reachability, History writability, hook ownership/sync, and explains that
trust is unknown. Human output uses symbols plus words; `--json` is stable.

## 19. Failure semantics

Direct commands fail nonzero for invalid input/config, missing/ambiguous target,
capability/startup failure, Attention without source, missing update key, and
failed primary actions. If display is accepted but History fails, send exits
zero with a stderr warning and structured `history_persisted: false`.

Hook mode always uses empty output and exit zero. No hook approves, denies,
blocks, or rewrites an agent event.

Interactive views restore raw mode, alternate screen, mouse state, and cursor on
normal exit, error, signal, and panic. Debug output never enters an owned TUI.

## 20. Module design

Per [ADR 0007](./docs/adr/0007-organize-around-deep-modules.md):

```text
src/
├── main.rs
├── cli.rs
├── config.rs
├── protocol.rs
├── daemon/
├── tmux/
├── history/
├── providers/
├── hooks/
├── ui/
└── render/
```

Modules are deep; private files may split implementations without expanding
caller-facing interfaces.

The daemon's tmux seam is:

```text
capabilities()
topology()
events()
reconcile(desired_display_plan)
jump(source, likely_client)
```

Production hides control parsing, pane IDs, command construction, renderer
creation, geometry diffing, and jump order. A deterministic fake supports daemon
tests. History stays concrete SQLite until a second implementation creates a
real seam.

Tokio owns sockets, timers, daemon tasks, control mode, and child coordination.
A dedicated blocking worker owns each rusqlite connection over bounded channels.
Ratatui/Crossterm view processes use synchronous event loops.

## 21. Test strategy

### 21.1 Pure and module-interface tests

- queue, overload, lifecycle, key upsert, request idempotency, deterministic time;
- easing, quantization, placement, ordering, and capacity;
- safe text with CJK, combining marks, emoji, controls, and bidi;
- config validation/reload, protocol framing/limits/multiplexing;
- fake-tmux multi-client follow and partial failure.

### 21.2 Storage and UI tests

- temporary SQLite: migrations, WAL concurrency, retention, Hide/undo, Clear,
  server scope, sorting, crash recovery, and disabled History;
- Ratatui snapshots: wide, 80x24, 60 columns, 48x10, and too small;
- UI states: empty, loading, error, disconnected, monochrome, Unicode, ASCII;
- PTY smoke: key handling, resize, exit, signal, and panic cleanup.

### 21.3 Provider and tmux tests

- official-schema provider fixtures and config golden tests;
- isolated tmux servers for capabilities, floating/modal panes, follow,
  shared/distinct client windows, stacking, animation dedup, Attention queue,
  jump after pane movement, source loss, reconnect, races, stale sockets, and
  shutdown.

Before tmux 3.8 release, CI builds a pinned upstream commit and a scheduled job
checks current master. After release, the gate moves to the 3.8 tag. Linux runs
integration CI; macOS runs unit/build CI and a required release smoke test until
stable tmux 3.8 CI is available.

## 22. Implementation sequence

The complete MVP ships together, through verified slices:

1. tmux capability and control-mode spike;
2. domain state plus fake tmux adapter;
3. daemon, IPC, lazy startup, and idempotency;
4. Toast, Attention, window follow, and animation;
5. SQLite persistence and adaptive History UI;
6. Claude/Codex adapters and conservative installers;
7. doctor, security hardening, failure injection, and cleanup;
8. packaging and full acceptance matrix.

A spike is not released as the MVP.

## 23. Release contract

The package declares and tests an MSRV compatible with Rust 2024 and selected
dependencies. SQLite is bundled. GitHub Releases initially provide archives and
SHA-256 checksums for:

```text
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
x86_64-apple-darwin
aarch64-apple-darwin
```

Homebrew is deferred until artifact and upgrade behavior are stable.

## 24. MVP acceptance criteria

### Toast

- first send lazily starts exactly one daemon and does not steal focus;
- every distinct attached-client window gets one appropriate Window Display;
- displays follow window changes without restarting timeout;
- stacking, placements, narrow fallback, animation, update, persistent timeout,
  and dismiss match this document;
- a live keyed Toast can jump to its Source Pane through `jump --key`, then
  closes every Window Display as Jumped;
- History records accepted Notifications when enabled.

### Attention

- requires a valid Source Pane;
- one is active globally and later requests queue;
- all eligible windows show it while Toast time is paused;
- Enter closes and jumps with documented best-effort client attribution;
- Esc/q dismisses; outside click and timeout do not;
- missing source is nonfatal and leaves the gate operable.

### History

- adaptive TUI restores the terminal on every exit path;
- plain/NDJSON output composes with scripts;
- scoping, ordering, Hide/undo, retention, and safe Clear work;
- History may be disabled without disabling delivery;
- jumps resolve current pane location at action time.

### Hooks

- install preserves unrelated config and is byte-identical when repeated;
- sync changes only installed scopes and updates relocated binary paths;
- remove deletes only owned handlers;
- Codex mixed representations require explicit permission;
- status does not invent trust state;
- supported events notify without provider output or behavioral changes.

### Reliability and security

- startup races produce one daemon and request retries do not duplicate;
- stale sockets recover only after ownership/type checks;
- malformed/oversized input cannot crash or exhaust the daemon;
- content cannot inject shell/terminal controls and is absent from argv/logs;
- control loss, renderer crash, SQLite contention, daemon crash, and tmux exit
  reach bounded documented outcomes;
- doctor reports actionable diagnostics without mutation.

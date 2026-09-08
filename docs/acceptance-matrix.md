# MVP acceptance matrix

This matrix is the traceability index for tmnotify-design.md sections 21 and
24. The ordinary Linux and macOS jobs run the complete locked test suite.
scripts/check-acceptance-trace.sh additionally fails when a named automated
check below is removed or renamed without updating this contract. TMUX checks
run only against a temporary -S socket with -f /dev/null.

## Toast

| Acceptance behavior | Automated evidence | Release smoke |
|---|---|---|
| Lazy start is single-owner and does not steal focus | RELIABILITY-RACE; TMUX-FOCUS | RS-01 |
| One Window Display per distinct viewed window | TOAST-DELIVERY; TMUX-TOPOLOGY | RS-02 |
| Displays follow without replaying enter or restarting timeout | TOAST-FOLLOW, TOAST-TIMEOUT; TMUX-FOLLOW | RS-02 |
| Placement, stacking, narrow fallback, animation, update, persistent timeout, dismiss | TOAST-LAYOUT, TOAST-STACKING, TOAST-NARROW, TOAST-ANIMATION, TOAST-UPDATE, TOAST-TIMEOUT | RS-03 |
| Live keyed jump resolves Source Pane and closes as Jumped | TOAST-JUMP; TMUX-PANE-LIFECYCLE | RS-04 |
| Accepted Notifications reach History when enabled | history::tests::migrates_to_wal_and_round_trips_normalized_notifications | RS-05 |

## Attention

| Acceptance behavior | Automated evidence | Release smoke |
|---|---|---|
| Valid Source Pane is required | ATTENTION-SOURCE | RS-06 |
| One gate is active and later requests use priority/FIFO | ATTENTION-QUEUE | RS-06 |
| Every eligible window shows the gate and Toast time pauses | ATTENTION-GLOBAL, TOAST-TIMEOUT | RS-06 |
| Enter closes globally and jumps with best-effort attribution | TOAST-JUMP, ATTENTION-KEYS | RS-06 |
| Esc/q dismiss; unrelated input does not close | ATTENTION-KEYS | RS-06 |
| Missing source is nonfatal and the gate remains operable | ATTENTION-LOSS; TMUX-PANE-LIFECYCLE | RS-06 |

## History

| Acceptance behavior | Automated evidence | Release smoke |
|---|---|---|
| Adaptive TUI restores terminal state on every exit path | HISTORY-RESTORE, ui::history::tests::control_c_returns_interrupted | RS-07 |
| Plain and NDJSON compose with scripts | HISTORY-OUTPUT | RS-08 |
| Scope, order, Hide/undo, retention, and safe Clear | HISTORY-SCOPE, HISTORY-HIDE, HISTORY-CLEAR, history::tests::retention_removes_oldest_hidden_before_any_visible_row | RS-07 |
| History can be disabled independently | HISTORY-DISABLED | RS-05 |
| Jump resolves the pane current location | HISTORY-JUMP; TMUX-PANE-LIFECYCLE | RS-04 |

## Hooks

| Acceptance behavior | Automated evidence | Release smoke |
|---|---|---|
| Install preserves unrelated data and repeats byte-identically | HOOK-INSTALL | RS-09 |
| Sync changes installed scopes and relocated paths only | HOOK-SYNC | RS-09 |
| Remove deletes owned handlers only | HOOK-REMOVE | RS-09 |
| Codex mixed representations require permission | HOOK-MIXED | RS-09 |
| Status does not invent provider trust | HOOK-STATUS | RS-09 |
| Supported events remain auxiliary and omit raw provider data | HOOK-AUXILIARY plus provider normalization tests | RS-09 |

## Reliability and security

| Acceptance behavior | Automated evidence | Release smoke |
|---|---|---|
| Startup races select one owner; retries are idempotent | RELIABILITY-RACE; TMUX-RACE | RS-01 |
| Stale sockets recover only after ownership/type checks | RELIABILITY-STALE | RS-10 |
| Malformed and oversized requests remain bounded | RELIABILITY-INPUT plus notification size tests | - |
| Content cannot inject controls or enter argv/logs | RELIABILITY-CONTENT, RELIABILITY-ARGV plus private logger tests | - |
| Control, renderer, SQLite, daemon, and tmux failures terminate boundedly | RELIABILITY-RENDER, RELIABILITY-SQLITE, RELIABILITY-DAEMON; TMUX-RECONNECT, TMUX-SHUTDOWN | RS-10 |
| Doctor is actionable and read-only | RELIABILITY-DOCTOR | RS-10 |

## Isolated tmux check IDs

scripts/test-tmux-capabilities.sh emits behavior-specific failures and covers:

- TMUX-CAPABILITIES: required commands, flags, formats, and correlated control
  framing;
- TMUX-TOPOLOGY: shared and distinct client windows plus control-client
  identity;
- TMUX-FOLLOW: window-add and session-window-change observation;
- TMUX-FOCUS: detached floating panes do not change the active pane;
- TMUX-STACKING: two simultaneous floating panes;
- TMUX-PANE-LIFECYCLE: stable pane identity across movement and source loss;
- TMUX-RACE: parallel topology changes;
- TMUX-RECONNECT: observer replacement and fresh command correlation; and
- TMUX-SHUTDOWN: bounded observer exit and an unreachable stopped server.

Stack ordering, animation-frame deduplication, and Attention priority/FIFO are
product policy rather than tmux protocol behavior, so their deterministic
checks run in the same required Linux job as TOAST-STACKING, TOAST-ANIMATION,
and ATTENTION-QUEUE.

## Temporary platform exception

Until a stable tmux 3.8 runner is available on macOS, pull-request CI runs all
unit/module tests and a release build there, while Linux builds the pinned
upstream floating-pane commit and the scheduled job checks current master.
Every release still requires the macOS interactive checks RS-02, RS-04, RS-06,
RS-07, and RS-10 from docs/release.md. Record the tmux version, host
architecture, and result in the release notes. This is the temporary exception
allowed by design section 21.3; it does not waive the release gate.

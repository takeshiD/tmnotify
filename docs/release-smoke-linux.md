# Linux interactive release smoke evidence

- Date: 2026-09-10 (Asia/Tokyo)
- OS: Linux 6.18.38
- Architecture: x86_64
- Rust: rustc 1.97.1 (8bab26f4f 2026-07-14)
- tmux version: tmux next-3.8
- tmux revision: `d44bfda26d2468b5f474087b56f93eda34c541b6`
- Isolation: explicit temporary `-S` socket, `-f /dev/null`, disposable
  XDG directories and HOME, and real tmux/script PTYs
- Redaction: terminal streams, Notification content, provider payloads,
  Notification IDs, credentials, and renderer tokens were not retained

Command:

```console
TMNOTIFY_TMUX_REVISION=d44bfda26d2468b5f474087b56f93eda34c541b6 \
TMNOTIFY_INTERACTIVE_ARTIFACT_DIR=target/interactive-smoke \
  scripts/test-interactive-smoke.sh /home/tkcd/.local/bin/tmux \
  target/release/tmnotify
```

| Check | Automated observation | Result |
|---|---|---|
| RS-01 | Concurrent first sends, exact-socket single daemon, two History rows | PASS |
| RS-02 | Three PTY clients, shared/distinct Window Displays, follow, focus | PASS |
| RS-03 | Six placements, stack, narrow suppression, update, persistent timeout, dismiss | PASS |
| RS-04 | Moved Source Pane jump and killed-source nonfatal refusal | PASS |
| RS-05 | Enabled persistence and restarted disabled-History delivery without DB writes | PASS |
| RS-06 | Global modal queue, paused Toast, ignored/dismiss/jump input, source loss | PASS |
| RS-07 | PTY wide/compact/too-small resize, Hide/undo, q/SIGINT, `stty` restoration | PASS |
| RS-08 | Plain shell pipeline and complete ordered NDJSON validation | PASS |
| RS-09 | Disposable Claude/Codex install/idempotence/status/sync/mixed/event/remove | PASS |
| RS-10 | Renderer recreation, daemon restart, read-only doctor, tmux shutdown | PASS |

No human terminal input was required. macOS evidence is intentionally produced
on GitHub-hosted runners by the manual `Interactive release smoke` workflow;
both architecture jobs must pass before release.

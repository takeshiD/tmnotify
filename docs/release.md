# Release process

GitHub Actions builds native release binaries for the four targets in the
product contract. SQLite is compiled into each executable through rusqlite's
`bundled` feature.

## MSRV evidence

The package declares Rust 1.88 as its minimum supported Rust version. At the
time this baseline was selected, Cargo metadata for the locked dependency graph
reported `rust_version = 1.88.0` for Ratatui 0.30.2 and its core, Crossterm,
widget, and terminal integration crates. CI installs Rust 1.88.0 and runs the
locked test suite, so a dependency update cannot silently raise the baseline.

## Artifacts

Pushing a `v*` tag that exactly matches the Cargo package version runs the
release workflow. Each native
runner:

1. builds `tmnotify` with `cargo build --locked --release --target TARGET`;
2. confirms `tmnotify --version` works;
3. packages it twice with the commit timestamp as `SOURCE_DATE_EPOCH` and
   requires byte-identical archives;
4. emits an archive and adjacent SHA-256 checksum; and
5. smoke-tests the public command surface of both the build output and the
   extracted executable on the build host.

Archives contain exactly one executable plus README, licenses, domain/design
documents, the acceptance matrix, this release guide, and accepted ADRs. The
publishing job has the only `contents: write` permission and runs only for a
tag event. A manual run builds and verifies artifacts but does not create or
modify a GitHub Release.

The packaging script can also be exercised locally:

```console
cargo build --locked --release
SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)" \
  scripts/package-release.sh \
  x86_64-unknown-linux-gnu target/release/tmnotify dist
```

## tmux compatibility

The required Linux gate builds the floating-pane upstream commit verified by
the project and probes it only through a private temporary socket with
`-f /dev/null`. A scheduled workflow separately builds the then-current tmux
`master`, records its resolved commit, and runs the same isolated probe. No CI
command addresses a user's or runner's default tmux server.

The pinned compatibility commit is updated only after the capability spike is
repeated and its result is recorded in `docs/tmux-3.8-capability-spike.md`.
The automated and interactive evidence for every MVP acceptance criterion is
indexed in `docs/acceptance-matrix.md`.

## Required interactive release smoke

Run `scripts/test-interactive-smoke.sh` on Linux and macOS with tmux 3.8 or the
recorded pinned commit. The harness uses only an explicit temporary `-S` socket
with `-f /dev/null`, disposable XDG directories and HOME, and tmux/script PTYs.
It never addresses the default tmux server or real Claude/Codex configuration.
Set `TMNOTIFY_INTERACTIVE_ARTIFACT_DIR` to
retain a sanitized Markdown report containing OS, architecture, Rust, tmux
version/revision, command classes, and results. Terminal captures, Notification
content, provider input, IDs, credentials, and renderer tokens are never
written to the report.

Linux can be exercised locally:

```console
cargo build --locked --release
TMNOTIFY_TMUX_REVISION=d44bfda26d2468b5f474087b56f93eda34c541b6 \
TMNOTIFY_INTERACTIVE_ARTIFACT_DIR=target/interactive-smoke \
  scripts/test-interactive-smoke.sh /path/to/pinned/tmux target/release/tmnotify
```

For macOS, manually dispatch the **Interactive release smoke** workflow. Its
`macOS RS-01..RS-10 (arm64)` and `(x86_64)` jobs build the pinned next-3.8
revision, run the same PTY harness and focused failure probes, and upload only
the sanitized report. Download both artifacts and require all RS rows to be
PASS before release.

No checklist item requires human terminal input. PTY automation covers client
attachment, History resize/key/signal paths, and Attention input. The only
human step is dispatching the macOS workflow (GitHub Actions → Interactive
release smoke → Run workflow) because this branch is intentionally not pushed
by the harness and GitHub-hosted macOS hardware is external to a local Linux
checkout.

1. **RS-01 — lazy daemon and races:** issue two first `send` commands
   concurrently; observe one daemon and one History row per request.
2. **RS-02 — broadcast and follow:** attach clients to one shared and one
   distinct window, send a Toast, then move each client. Observe one Window
   Display per distinct viewed window, follow behavior, and no focus change.
3. **RS-03 — Toast presentation:** exercise all placements, stacking, narrow
   fallback, enter/stay/exit, keyed update, `--timeout never`, and dismiss.
4. **RS-04 — pane resolution:** move the Source Pane before `jump --key` and
   History jump, then repeat after killing it. Successful jumps use its current
   location; loss is nonfatal and does not falsely close the Notification.
5. **RS-05 — History delivery:** verify enabled History records accepted
   Notifications and disabled History leaves delivery working.
6. **RS-06 — Attention:** queue two gates, verify global display and paused
   Toasts, then exercise Enter, Esc/q, ignored input, and source loss.
7. **RS-07 — History TUI:** resize through wide, compact, and too-small states;
   exercise Hide/undo and normal, signal, and error exits; verify terminal
   restoration.
8. **RS-08 — script output:** pipe `history --plain` and NDJSON into basic
   shell consumers and validate ordering and complete records.
9. **RS-09 — provider hooks:** on disposable Claude and Codex configs, run
   install twice, status, relocated-binary sync, mixed-form refusal, event
   delivery, and remove; diff against the originals to prove unrelated content
   is preserved.
10. **RS-10 — recovery:** exercise an owned stale socket, control reconnect,
    renderer termination, SQLite contention, daemon restart, tmux shutdown, and
    read-only `doctor`; verify bounded, actionable outcomes.

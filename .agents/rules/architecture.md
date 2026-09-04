# Architecture guardrails

These rules translate tmnotify's accepted ADRs into implementation constraints. Apply them while planning, implementing, reviewing, and refactoring product code, CLI behavior, persistence, provider integration, build configuration, and release behavior.

The ADRs remain authoritative. If a proposed change conflicts with a rule or its source ADR, pause implementation and write a new ADR that explicitly supersedes the old decision. Update this file in the same change after that ADR is accepted.

## Platform paths and local-only boundary

Source: [ADR-0001](../../docs/adr/0001-use-xdg-paths-on-linux-and-macos.md). Additional product constraints: [`tmnotify-design.md`](../../tmnotify-design.md), sections 1 and 2.

- Use XDG environment variables and XDG-style fallback paths on both Linux and macOS. Do not introduce macOS-only `~/Library/Application Support` behavior.
- When `XDG_RUNTIME_DIR` is unavailable, place runtime IPC in a mode-0700 per-user directory under the system temporary directory.
- Preserve a single executable containing the CLI, daemon, renderers, History UI, hook receiver, and installer.
- Keep tmnotify local-only: no network access, telemetry, or automatic update checks.
- Treat external text as untrusted plain text. Normalize display controls at ingress and never interpolate input into shell commands.
- Put explicit bounds on buffers, queues, retries, request caches, IPC request size, and shutdown work.

## Notification delivery and lifecycle

Source: [ADR-0002](../../docs/adr/0002-broadcast-notifications-to-visible-windows.md).

- Project one live Notification into every distinct tmux window currently viewed by an attached, non-control client.
- Clients viewing the same window share one Window Display. Do not create a duplicate display per client.
- Keep one Notification lifecycle, timeout, key identity, and History record across all Window Displays.
- Reconcile Window Displays as clients attach, detach, or move.
- Pause a Toast timeout while no Window Display is visible. Resolve an Attention action globally across its displays.
- Treat jump attribution as best effort: use the most recently active client viewing the Attention Window, and do not assume independent movement for clients that share tmux session state.

## tmux ownership and seam

Sources: [ADR-0003](../../docs/adr/0003-observe-tmux-through-control-mode.md) and [ADR-0007](../../docs/adr/0007-organize-around-deep-modules.md).

- Maintain one tmux control-mode connection per tmnotify daemon for topology events and display command coordination.
- Do not install global tmux hooks or replace event-driven observation with continuous polling. A low-frequency reconciliation pass is permitted.
- Exclude tmnotify's infrastructure control client and its windows from notification recipients.
- Keep the tmux boundary plan-oriented: callers provide desired display plans, while the tmux module owns control-mode events, pane identities, command sequencing, and reconciliation.
- Provide fake tmux adapters for deterministic tests without leaking protocol mechanics into the rest of the program.

## Interactive and Toast rendering

Source: [ADR-0004](../../docs/adr/0004-use-ratatui-only-for-interactive-views.md).

- Use Ratatui with Crossterm for the interactive History viewer and Attention Gate.
- Keep Toast rendering as a small purpose-built renderer; do not route it through Ratatui merely to share widgets.
- Keep business state independently testable from terminal IO.
- Restore raw mode, alternate screen, cursor state, and other terminal state on normal exit, interruption, and recoverable error paths.
- Ensure History and Attention layouts degrade truthfully in narrow tmux panes, including an explicit too-small state where necessary.

## Daemons, live state, and History

Sources: [ADR-0005](../../docs/adr/0005-share-one-history-database-across-daemons.md) and [`tmnotify-design.md`](../../tmnotify-design.md), sections 2 and 7.

- Run one live-state daemon per tmux server. Queueing, timers, animation, key indexes, and Window Displays belong to that daemon.
- Share one user-level SQLite History database across all of the user's daemons.
- Use SQLite WAL mode, short write and migration transactions, and bounded busy retries.
- Never block scheduler or renderer event loops on database work. Serialize writes through the designated History writer boundary.
- Keep History distinct from the live queue. Persist lifecycle changes without turning closed History rows into live scheduler state.
- Keep History as a concrete SQLite module until a second real storage implementation justifies an abstraction seam.

## Provider hook safety

Source: [ADR-0006](../../docs/adr/0006-manage-provider-hooks-conservatively.md).

- Own only structurally identified hook handlers that invoke the installing tmnotify binary by absolute path.
- Preserve one mode-0600 rolling backup before each real provider configuration change.
- Synchronize only scopes where tmnotify is already installed.
- Never grant provider trust, read undocumented trust stores, or claim to determine trust status; direct users to the provider's own trust interface.
- Do not rewrite Codex inline TOML hooks.
- Refuse mixed Codex hook representations unless the user explicitly supplies `--allow-mixed`.
- Keep hook failures auxiliary: they must not change the coding agent's behavior or outcome.

## Module boundaries

Source: [ADR-0007](../../docs/adr/0007-organize-around-deep-modules.md).

- Organize code into a small set of deep top-level modules aligned with product responsibilities.
- Do not split state, scheduler, lifecycle, and policy into shallow layers with broad cross-module coupling.
- Keep provider schemas inside provider adapters and convert them to the normalized agent event model at ingress.
- Preserve test seams at external boundaries—tmux, process execution, clocks, filesystem/configuration, and provider input—without abstracting concrete internal components prematurely.

## Renderer content and authentication

Source: [ADR-0008](../../docs/adr/0008-stream-renderer-content-from-the-daemon.md).

- Pass only a Window Display ID and one-time token in renderer process arguments.
- Never expose Notification title, body, or normalized metadata through process arguments, environment variables, or temporary files.
- Stream initial content and subsequent updates from the daemon over the user-private socket.
- Reject stale renderers after Window Display recreation and consume or expire one-time credentials safely.
- Use the daemon's existing tmux control-mode connection for batched display commands; do not create an independent tmux observer per renderer.

## Completion check

Before completing an affected change, identify every section above that applies and verify the diff and tests against its source ADR. A change is incomplete while an applicable invariant is unverified or an ADR conflict is unresolved.

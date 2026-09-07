# tmux 3.8 capability and control-mode spike

Issue #4 was verified on 2026-09-08 against `tmux next-3.8`. Every runtime
probe used a dedicated `-S /tmp/tmnotify-issue4-*.sock` server created with
`-f /dev/null`; the user's default server was never contacted.

## Confirmed capability surface

tmnotify gates display service on observed commands, flags, formats, and a real
short-lived control-mode handshake, not on the version string.

| Need | Confirmed surface |
|---|---|
| Floating pane creation | Create a normal pane, then `break-pane -W -s %N -t @N -X x -Y y -x width -y height -d`. `pane_floating_flag=1` confirms the result. |
| Movement | `move-pane -t %N -L/-R/-U/-D cells`; `-z` changes stacking order. Absolute `-X/-Y` flags exist, but the relative flags were observed changing `pane_left`/`pane_top` and are the dependable reconciliation primitive. |
| Resize | `resize-pane -t %N -x width -y height`. For a floating pane, the reported content size excludes its border (a requested 32x9 produced `pane_width=30`, `pane_height=7`). Reconciliation must compare like-for-like geometry. |
| Stable identity | `$N`, `@N`, and `%N` are available as `session_id`, `window_id`, and `pane_id`. Names and indexes are not required for identity. |
| Topology/client formats | `client_name`, `client_control_mode`, `client_activity`, stable `session_id`/`window_id`, `socket_path`, floating flag, and pane geometry fields are present. `client_session` is a name and is deliberately not used as identity. |
| Infrastructure exclusion | The attached `-C` client reported `client_control_mode=1`; recipient derivation can exclude it without relying on its generated name. |
| Correlation | A `-C` command returned `%begin <time> <command> <flags>`, payload, matching `%end`, then `%exit`. Caller request tickets can be paired FIFO with tmux command numbers. |
| Events | The isolated scenario observed `%session-changed`, `%window-add`, `%session-window-changed`, `%layout-change`, `%window-close`, and `%exit`. The parser also recognizes the documented client/session/window variants needed to invalidate topology. |

`display-popup` exists, but it is not the Window Display primitive: the accepted
design needs stable pane identity, movement, resizing, and renderer streaming.
The floating-pane sequence above supplies those properties. Attention remains
modal because its floating pane owns input while active; key handling belongs
to the renderer, not to global tmux bindings.

## Code boundary

`tmux::Backend` is plan-oriented: callers submit a `DisplayPlan`, request a
jump, and consume product-level topology/disconnection events. Stable tmux IDs,
command construction, response framing, and raw notification names remain
inside `tmux`. A deterministic fake records plans and jumps for daemon tests.

The production capability probe returns a structured `CapabilityReport`.
`require_display_service` blocks only daemon display startup. Callers for plain
History and hook status need not invoke that gate, preserving tmux-independent
commands on unsupported servers.

The control codec handles response/event interleaving and rejects malformed or
uncorrelated frames without panicking. Its checked-in fixture is reduced from
the isolated 3.8 run and contains no user pane content.

## Follow-up risks

- tmux's floating-pane work is on the `next-3.8` surface. Before release, repeat
  this probe against the final 3.8 release on Linux and macOS.
- Floating dimensions distinguish requested outer geometry from reported inner
  pane geometry. The renderer/display implementation must centralize that
  border conversion and test the six placements at narrow sizes.
- Events invalidate cached topology; they are not a complete state log. The
  daemon still needs the accepted low-frequency full reconciliation and
  reconnect backoff. It must not install global hooks or continuously poll.
- `%exit` proves control disconnection, not server death. Only a failed
  reconnect/server probe may close Notifications as `ServerEnded`.
- A shared Attention Window loses exact actor identity before pane input reaches
  the renderer. Jump remains best effort using the most recently active
  eligible client, as specified by ADR 0002.

The spike confirms ADR 0003 and ADR 0007; no superseding ADR is needed.

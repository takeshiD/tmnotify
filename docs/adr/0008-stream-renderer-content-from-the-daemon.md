# Stream renderer content from the daemon

Pane renderers receive only a window-display ID and one-time token in their process arguments, then connect to the user-private daemon socket to obtain content and subsequent updates. This avoids exposing Notification text through process arguments, environment variables, or temporary files, supports in-place key updates, and lets stale renderers be rejected after window-display recreation; the daemon uses its single tmux control-mode connection for both topology events and batched display commands.

#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
binary="$repository_root/target/debug/tmnotify"

if [ ! -x "$binary" ]; then
    echo "demo binary is missing: run cargo build first" >&2
    exit 1
fi

demo_root=$(mktemp -d "${TMPDIR:-/tmp}/tmnotify-vhs-demo.XXXXXXXX")
socket="$demo_root/tmux.sock"

cleanup() {
    tmux -S "$socket" kill-server >/dev/null 2>&1 || true
    rm -rf -- "$demo_root"
}
trap cleanup EXIT HUP INT TERM

export HOME="$demo_root/home"
export XDG_CONFIG_HOME="$demo_root/xdg/config"
export XDG_STATE_HOME="$demo_root/xdg/state"
export XDG_RUNTIME_DIR="$demo_root/xdg/runtime"
export CLAUDE_CONFIG_DIR="$demo_root/providers/claude"
binary_directory=$(dirname -- "$binary")
PATH="$binary_directory:$PATH"
export PATH

mkdir -p \
    "$HOME/.codex" \
    "$CLAUDE_CONFIG_DIR" \
    "$demo_root/project" \
    "$XDG_CONFIG_HOME/tmnotify" \
    "$XDG_STATE_HOME" \
    "$XDG_RUNTIME_DIR"
chmod 0700 \
    "$HOME/.codex" \
    "$CLAUDE_CONFIG_DIR" \
    "$demo_root/project" \
    "$XDG_CONFIG_HOME/tmnotify" \
    "$XDG_STATE_HOME" \
    "$XDG_RUNTIME_DIR"
printf '{}\n' >"$HOME/.codex/hooks.json"
printf '{}\n' >"$CLAUDE_CONFIG_DIR/settings.json"

cat >"$XDG_CONFIG_HOME/tmnotify/config.toml" <<'EOF'
[toast]
position = "top-right"
timeout = "4s"

[toast.animation]
enabled = false

[display]
color = "always"
unicode = "always"
EOF
chmod 0600 \
    "$HOME/.codex/hooks.json" \
    "$CLAUDE_CONFIG_DIR/settings.json" \
    "$XDG_CONFIG_HOME/tmnotify/config.toml"

"$binary" hook install claude user

tmux -S "$socket" -f /dev/null new-session -d -c "$demo_root/project" -s tmnotify-demo \
    "env PS1='$ ' bash --noprofile --norc"
tmux -S "$socket" set-option -g status off
tmux -S "$socket" attach-session -t tmnotify-demo

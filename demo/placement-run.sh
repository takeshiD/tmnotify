#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
binary="$repository_root/target/debug/tmnotify"
position=${1:-}

case "$position" in
    top-left | top-center | top-right | bottom-left | bottom-center | bottom-right) ;;
    *)
        echo "usage: $0 <top-left|top-center|top-right|bottom-left|bottom-center|bottom-right>" >&2
        exit 2
        ;;
esac

case "$position" in
    *-left) edge=left ;;
    *-right) edge=right ;;
    top-center) edge=top ;;
    bottom-center) edge=bottom ;;
esac

showcase_root=$(mktemp -d "${TMPDIR:-/tmp}/tmnotify-placement.XXXXXXXX")
socket="$showcase_root/tmux.sock"
session=tmnotify-placement

cleanup() {
    tmux -S "$socket" kill-server >/dev/null 2>&1 || true
    rm -rf -- "$showcase_root"
}
trap cleanup EXIT HUP INT TERM

export TERM=xterm-256color
export HOME="$showcase_root/home"
export XDG_CONFIG_HOME="$showcase_root/xdg/config"
export XDG_STATE_HOME="$showcase_root/xdg/state"
export XDG_RUNTIME_DIR="$showcase_root/xdg/runtime"

mkdir -p \
    "$HOME" \
    "$showcase_root/project" \
    "$XDG_CONFIG_HOME/tmnotify" \
    "$XDG_STATE_HOME" \
    "$XDG_RUNTIME_DIR"
chmod 0700 "$HOME" "$XDG_CONFIG_HOME" "$XDG_CONFIG_HOME/tmnotify" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR"

cat >"$XDG_CONFIG_HOME/tmnotify/config.toml" <<EOF
[toast]
position = "$position"
width = 56
height = 3
timeout = "1500ms"

[toast.animation]
enabled = true
fps = 30
enter_duration = "600ms"
exit_duration = "500ms"
enter_easing = "ease-out"
exit_easing = "ease-in"

[display]
color = "always"
unicode = "always"
EOF
chmod 0600 "$XDG_CONFIG_HOME/tmnotify/config.toml"

tmux -S "$socket" -f /dev/null new-session -d -x 100 -y 30 -s "$session" -c "$showcase_root/project" \
    "printf 'tmnotify · %s\\nenter: %s · exit: %s\\n\\n' '$position' '$edge' '$edge'; exec env PS1='$ ' bash --noprofile --norc"
tmux -S "$socket" set-option -g status off

(
    # Wait for the recorded tmux client so the toast has a visible window.
    tries=0
    while ! tmux -S "$socket" list-clients -F '#{client_name}' | grep -q .; do
        tries=$((tries + 1))
        if [ "$tries" -ge 50 ]; then
            exit 1
        fi
        sleep 0.1
    done
    sleep 1
    "$binary" -S "$socket" send \
        --title "$position" \
        --timeout 1500ms \
        "enter from $edge · exit to $edge"
    # enter + timeout + exit, plus startup jitter
    sleep 3.5
    tmux -S "$socket" detach-client -s "$session" >/dev/null 2>&1 || true
) &
sender_pid=$!

tmux -S "$socket" attach-session -t "$session"
wait "$sender_pid" 2>/dev/null || true

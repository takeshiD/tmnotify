#!/bin/sh
set -eu

tmux_binary=${1:-tmux}
[ -x "$tmux_binary" ] || command -v "$tmux_binary" >/dev/null 2>&1 || {
    echo "tmux executable not found: $tmux_binary" >&2
    exit 2
}

test_directory=$(mktemp -d "${TMPDIR:-/tmp}/tmnotify-tmux-ci.XXXXXXXX")
socket="$test_directory/server.sock"
control_input="$test_directory/control.in"
control_output="$test_directory/control.out"
control_pid=

cleanup() {
    "$tmux_binary" -S "$socket" kill-server >/dev/null 2>&1 || true
    exec 3>&- 2>/dev/null || true
    if [ -n "$control_pid" ]; then
        wait "$control_pid" 2>/dev/null || true
    fi
    rm -rf "$test_directory"
}
trap cleanup EXIT HUP INT TERM

run_tmux() {
    "$tmux_binary" -S "$socket" -f /dev/null "$@"
}

run_tmux new-session -d -s tmnotify-ci 'sleep 30'
mkfifo "$control_input"
exec 3<>"$control_input"
"$tmux_binary" -S "$socket" -f /dev/null -C attach-session -t tmnotify-ci < "$control_input" > "$control_output" 2>&1 &
control_pid=$!

attempt=0
while ! run_tmux list-clients -F '#{client_control_mode}' | grep -q '^1$'; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 50 ]; then
        echo "isolated tmux control client did not attach" >&2
        exit 1
    fi
    sleep 0.1
done

commands=$(run_tmux list-commands)
formats=$(run_tmux display-message -a -p)

for requirement in break-pane move-pane resize-pane; do
    printf '%s\n' "$commands" | grep -q "^$requirement " || {
        echo "missing tmux command: $requirement" >&2
        exit 1
    }
done

for requirement in client_activity client_control_mode client_name pane_floating_flag pane_height pane_id pane_left pane_top pane_width session_id socket_path window_id; do
    printf '%s\n' "$formats" | grep -q "^$requirement=" || {
        echo "missing tmux format: $requirement" >&2
        exit 1
    }
done

target_window=$(run_tmux display-message -p -t tmnotify-ci:0 '#{window_id}')
run_tmux new-window -d -t tmnotify-ci -n source 'sleep 30'
source_pane=$(run_tmux display-message -p -t tmnotify-ci:source '#{pane_id}')
run_tmux break-pane -W -d -s "$source_pane" -t "$target_window" -X 1 -Y 1 -x 32 -y 9

# A -W floating pane retains its owning window ID while being projected into
# the destination window, so use the all-panes surface used by the daemon.
pane_snapshot=$(run_tmux list-panes -a -F '#{pane_id} #{pane_floating_flag}')
floating_pane=$(printf '%s\n' "$pane_snapshot" | awk '$2 == 1 { print $1; exit }')
[ -n "$floating_pane" ] || {
    echo "break-pane did not create a floating pane" >&2
    printf '%s\n' "$pane_snapshot" >&2
    exit 1
}

run_tmux move-pane -t "$floating_pane" -R 1
run_tmux resize-pane -t "$floating_pane" -x 30 -y 7

printf '%s\n' 'display-message -p tmnotify-control-probe' >&3
attempt=0
while ! grep -q '^tmnotify-control-probe$' "$control_output"; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 50 ]; then
        echo "tmux control-mode command did not complete" >&2
        exit 1
    fi
    sleep 0.1
done
grep -q '^%begin ' "$control_output"
grep -q '^%end ' "$control_output"

echo "isolated tmux capability probe passed: $socket"

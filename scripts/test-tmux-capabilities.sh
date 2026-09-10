#!/bin/sh
set -eu

tmux_binary=${1:-tmux}
[ -x "$tmux_binary" ] || command -v "$tmux_binary" >/dev/null 2>&1 || {
    echo "tmux executable not found: $tmux_binary" >&2
    exit 2
}

test_directory=$(mktemp -d "${TMPDIR:-/tmp}/tmnotify-tmux-ci.XXXXXXXX")
socket="$test_directory/server.sock"
control_one_input="$test_directory/control-one.in"
control_two_input="$test_directory/control-two.in"
control_three_input="$test_directory/control-three.in"
control_reconnect_input="$test_directory/control-reconnect.in"
control_one_output="$test_directory/control-one.out"
control_two_output="$test_directory/control-two.out"
control_three_output="$test_directory/control-three.out"
control_reconnect_output="$test_directory/control-reconnect.out"
snapshot="$test_directory/topology.txt"
control_pids=
probe_status=FAIL

preserve_artifacts() {
    [ -n "${TMNOTIFY_TMUX_ARTIFACT_DIR:-}" ] || return 0
    mkdir -p "$TMNOTIFY_TMUX_ARTIFACT_DIR"
    report="$TMNOTIFY_TMUX_ARTIFACT_DIR/tmux-capabilities.md"
    {
        echo '# Isolated tmux capability probe'
        echo
        printf -- '- OS: %s %s\n' "$(uname -s)" "$(uname -r)"
        printf -- '- Architecture: %s\n' "$(uname -m)"
        command -v rustc >/dev/null 2>&1 && printf -- '- Rust: %s\n' "$(rustc --version)"
        printf -- '- tmux: %s\n' "$("$tmux_binary" -V)"
        printf -- '- tmux revision: %s\n' "${TMNOTIFY_TMUX_REVISION:-unknown}"
        echo '- Isolation: explicit temporary -S socket and -f /dev/null'
        echo '- Redaction: control streams and pane output are not retained'
        echo
        echo '| Checks | Result |'
        echo '|---|---|'
        printf '| TMUX-CAPABILITIES, TMUX-TOPOLOGY, TMUX-FOLLOW, TMUX-FOCUS, TMUX-STACKING, TMUX-PANE-LIFECYCLE, TMUX-RACE, TMUX-RECONNECT, TMUX-SHUTDOWN | %s |\n' "$probe_status"
    } > "$report"
}

cleanup() {
    preserve_artifacts || true
    "$tmux_binary" -S "$socket" kill-server >/dev/null 2>&1 || true
    exec 3>&- 4>&- 5>&- 6>&- 2>/dev/null || true
    for pid in $control_pids; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$test_directory"
}
trap cleanup EXIT HUP INT TERM

run_tmux() {
    "$tmux_binary" -S "$socket" -f /dev/null "$@"
}

wait_for() {
    description=$1
    shift
    attempt=0
    until "$@"; do
        attempt=$((attempt + 1))
        if [ "$attempt" -ge 100 ]; then
            echo "timed out waiting for $description" >&2
            return 1
        fi
        sleep 0.1
    done
}

has_control_clients() {
    expected=$1
    actual=$(run_tmux list-clients -F '#{client_control_mode}' 2>/dev/null | grep -c '^1$' || true)
    [ "$actual" -eq "$expected" ]
}

output_contains() {
    file=$1
    pattern=$2
    grep -q "$pattern" "$file"
}

pane_exists() {
    pane=$1
    run_tmux list-panes -a -F '#{pane_id}' | grep -qx "$pane"
}

run_tmux new-session -d -s shared 'sleep 300'
run_tmux new-session -d -s distinct 'sleep 300'

mkfifo "$control_one_input" "$control_two_input" "$control_three_input"
exec 3<>"$control_one_input"
exec 4<>"$control_two_input"
exec 5<>"$control_three_input"
"$tmux_binary" -S "$socket" -f /dev/null -C attach-session -t shared < "$control_one_input" > "$control_one_output" 2>&1 &
control_one_pid=$!
control_pids="$control_one_pid"
"$tmux_binary" -S "$socket" -f /dev/null -C attach-session -t shared < "$control_two_input" > "$control_two_output" 2>&1 &
control_two_pid=$!
control_pids="$control_pids $control_two_pid"
"$tmux_binary" -S "$socket" -f /dev/null -C attach-session -t distinct < "$control_three_input" > "$control_three_output" 2>&1 &
control_three_pid=$!
control_pids="$control_pids $control_three_pid"
wait_for 'three isolated control clients' has_control_clients 3

commands=$(run_tmux list-commands)
formats=$(run_tmux display-message -a -p)

for requirement in break-pane move-pane new-pane resize-pane; do
    printf '%s\n' "$commands" | grep -q "^$requirement " || {
        echo "missing tmux command: $requirement" >&2
        exit 1
    }
done

for requirement in client_activity client_control_mode client_name pane_floating_flag pane_height pane_id pane_left pane_modal_flag pane_top pane_width session_id socket_path window_id; do
    printf '%s\n' "$formats" | grep -q "^$requirement=" || {
        echo "missing tmux format: $requirement" >&2
        exit 1
    }
done
printf '%s\n' "$commands" | grep '^new-pane ' | grep -q 'O' || {
    echo 'new-pane does not expose modal pane creation (-O)' >&2
    exit 1
}

# Two clients share one window while the third remains in a distinct session.
run_tmux list-clients -F '#{client_name} #{session_id} #{window_id} #{client_control_mode}' > "$snapshot"
shared_session=$(run_tmux display-message -p -t shared:0 '#{session_id}')
distinct_session=$(run_tmux display-message -p -t distinct:0 '#{session_id}')
[ "$(awk -v session="$shared_session" '$2 == session { count++ } END { print count + 0 }' "$snapshot")" -eq 2 ] || {
    echo 'shared-window topology did not expose exactly two clients' >&2
    exit 1
}
[ "$(awk -v session="$distinct_session" '$2 == session { count++ } END { print count + 0 }' "$snapshot")" -eq 1 ] || {
    echo 'distinct-window topology did not remain independent' >&2
    exit 1
}
[ "$(awk '{ windows[$3] = 1 } END { print length(windows) }' "$snapshot")" -eq 2 ] || {
    echo 'client topology did not deduplicate to two distinct windows' >&2
    exit 1
}

# Changing the shared session follows both clients and leaves the distinct one alone.
run_tmux new-window -d -t shared -n followed
followed_window=$(run_tmux display-message -p -t shared:followed '#{window_id}')
run_tmux select-window -t shared:followed
run_tmux list-clients -F '#{client_name} #{session_id} #{window_id} #{client_control_mode}' > "$snapshot"
[ "$(awk -v session="$shared_session" -v window="$followed_window" '$2 == session && $3 == window { count++ } END { print count + 0 }' "$snapshot")" -eq 2 ] || {
    echo 'shared clients did not follow their session window change' >&2
    exit 1
}
[ "$(awk -v session="$distinct_session" '$2 == session { count++ } END { print count + 0 }' "$snapshot")" -eq 1 ] || {
    echo 'distinct client moved during another session follow event' >&2
    exit 1
}
wait_for 'window-add control event' output_contains "$control_one_output" '^%window-add '
wait_for 'session-window-changed control event' output_contains "$control_one_output" '^%session-window-changed '

# A detached floating Toast surface must not steal focus. Two surfaces exercise stacking.
target_window=$(run_tmux display-message -p -t shared:followed '#{window_id}')
active_before=$(run_tmux display-message -p -t "$target_window" '#{pane_id}')
renderer_one=$(run_tmux split-window -d -P -F '#{pane_id}' -t "$active_before")
run_tmux break-pane -W -d -s "$renderer_one" -X 1 -Y 1 -x 32 -y 9
renderer_two=$(run_tmux split-window -d -P -F '#{pane_id}' -t "$active_before")
run_tmux break-pane -W -d -s "$renderer_two" -X 35 -Y 1 -x 32 -y 9

pane_snapshot=$(run_tmux list-panes -t "$target_window" -F '#{pane_id} #{pane_floating_flag} #{pane_left} #{pane_top} #{pane_width} #{pane_height}')
[ "$(printf '%s\n' "$pane_snapshot" | awk '$2 == 1 { count++ } END { print count + 0 }')" -eq 2 ] || {
    echo 'stacking probe did not create two floating panes' >&2
    printf '%s\n' "$pane_snapshot" >&2
    exit 1
}
[ "$(run_tmux display-message -p -t "$target_window" '#{pane_id}')" = "$active_before" ] || {
    echo 'detached floating pane creation stole focus' >&2
    exit 1
}

left_before=$(run_tmux display-message -p -t "$renderer_one" '#{pane_left}')
run_tmux move-pane -t "$renderer_one" -R 1
left_after=$(run_tmux display-message -p -t "$renderer_one" '#{pane_left}')
[ "$left_after" -gt "$left_before" ] || {
    echo 'floating pane movement did not change pane_left' >&2
    exit 1
}
[ "$(run_tmux display-message -p -t "$renderer_one" '#{pane_width}x#{pane_height}')" = '30x7' ] || {
    echo '32x9 bordered floating pane did not report 30x7 content cells' >&2
    exit 1
}
run_tmux resize-pane -t "$renderer_one" -x 30 -y 7
[ "$(run_tmux display-message -p -t "$renderer_one" '#{pane_width}x#{pane_height}')" = '28x5' ] || {
    actual_size=$(run_tmux display-message -p -t "$renderer_one" '#{pane_width}x#{pane_height}')
    echo "30x7 bordered resize reported unexpected content cells: $actual_size" >&2
    exit 1
}

# Modal Attention surfaces must be both floating and active.
modal_pane=$(run_tmux new-pane -O -P -F '#{pane_id}' -t "$active_before" -X 10 -Y 4 -x 32 -y 9)
[ "$(run_tmux display-message -p -t "$modal_pane" '#{pane_floating_flag}:#{pane_modal_flag}')" = '1:1' ] || {
    echo 'new-pane -O did not create a modal floating pane' >&2
    exit 1
}
[ "$(run_tmux display-message -p -t "$target_window" '#{pane_id}')" = "$modal_pane" ] || {
    echo 'modal pane was not the active input pane' >&2
    exit 1
}
run_tmux kill-pane -t "$modal_pane"

# Stable Source Pane IDs survive movement and disappear after source loss.
source_pane=$(run_tmux split-window -d -P -F '#{pane_id}' -t "$active_before")
run_tmux new-window -d -t distinct -n moved-source
destination_pane=$(run_tmux display-message -p -t distinct:moved-source '#{pane_id}')
run_tmux join-pane -d -s "$source_pane" -t "$destination_pane"
pane_exists "$source_pane" || {
    echo 'pane identity was lost while moving between windows' >&2
    exit 1
}
run_tmux kill-pane -t "$source_pane"
if pane_exists "$source_pane"; then
    echo 'killed source pane remained resolvable' >&2
    exit 1
fi

# Parallel topology changes exercise event/command interleaving without a shared default server.
run_tmux new-window -d -t distinct -n race-one &
race_one_pid=$!
run_tmux new-window -d -t distinct -n race-two &
race_two_pid=$!
wait "$race_one_pid"
wait "$race_two_pid"
[ "$(run_tmux list-windows -t distinct -F '#{window_name}' | grep -c '^race-')" -eq 2 ] || {
    echo 'parallel topology commands lost a window' >&2
    exit 1
}

# Drop one observer, reconnect it, and require fresh correlated control framing.
control_one_name=$(awk -v session="$shared_session" '$2 == session { print $1; exit }' "$snapshot")
run_tmux detach-client -t "$control_one_name"
wait_for 'one control client disconnect' has_control_clients 2
exec 3>&-
kill "$control_one_pid" 2>/dev/null || true
wait "$control_one_pid" 2>/dev/null || true
mkfifo "$control_reconnect_input"
exec 6<>"$control_reconnect_input"
"$tmux_binary" -S "$socket" -f /dev/null -C attach-session -t shared < "$control_reconnect_input" > "$control_reconnect_output" 2>&1 &
control_reconnect_pid=$!
control_pids="$control_two_pid $control_three_pid $control_reconnect_pid"
wait_for 'reconnected control client' has_control_clients 3
printf '%s\n' 'display-message -p tmnotify-control-probe' >&6
wait_for 'correlated reconnect response' output_contains "$control_reconnect_output" '^tmnotify-control-probe$'
grep -q '^%begin ' "$control_reconnect_output"
grep -q '^%end ' "$control_reconnect_output"

# Server shutdown must terminate every observer promptly and leave no usable socket.
run_tmux kill-server
wait_for 'first control client shutdown event' output_contains "$control_two_output" '^%exit'
wait_for 'second control client shutdown event' output_contains "$control_three_output" '^%exit'
wait_for 'reconnected control client shutdown event' output_contains "$control_reconnect_output" '^%exit'
exec 4>&- 5>&- 6>&-
kill "$control_two_pid" "$control_three_pid" "$control_reconnect_pid" 2>/dev/null || true
wait "$control_two_pid"
wait "$control_three_pid"
wait "$control_reconnect_pid"
if "$tmux_binary" -S "$socket" -f /dev/null list-sessions >/dev/null 2>&1; then
    echo 'tmux server remained reachable after shutdown' >&2
    exit 1
fi

probe_status=PASS
echo "isolated tmux acceptance probe passed: $socket"

#!/bin/sh
set -eu
umask 077

tmux_binary=${1:-tmux}
tmnotify_binary=${2:-target/release/tmnotify}
artifact_directory=${TMNOTIFY_INTERACTIVE_ARTIFACT_DIR:-}
tmux_revision=${TMNOTIFY_TMUX_REVISION:-unknown}

[ -x "$tmnotify_binary" ] || {
    echo "tmnotify executable not found: $tmnotify_binary" >&2
    exit 2
}
[ -x "$tmux_binary" ] || command -v "$tmux_binary" >/dev/null 2>&1 || {
    echo "tmux executable not found: $tmux_binary" >&2
    exit 2
}
command -v script >/dev/null 2>&1 || {
    echo 'script is required for PTY automation' >&2
    exit 2
}
command -v jq >/dev/null 2>&1 || {
    echo 'jq is required for content-free NDJSON validation' >&2
    exit 2
}

case $tmux_binary in
    /*) ;;
    */*) tmux_binary="$(CDPATH='' cd -- "$(dirname "$tmux_binary")" && pwd)/$(basename "$tmux_binary")" ;;
    *) tmux_binary=$(command -v "$tmux_binary") ;;
esac
case $tmnotify_binary in
    /*) ;;
    */*) tmnotify_binary="$(CDPATH='' cd -- "$(dirname "$tmnotify_binary")" && pwd)/$(basename "$tmnotify_binary")" ;;
    *) tmnotify_binary=$(command -v "$tmnotify_binary") ;;
esac

os_version="$(uname -s) $(uname -r)"
architecture=$(uname -m)
rust_version=$(rustc --version)
tmux_version=$("$tmux_binary" -V)

test_directory=$(mktemp -d "${TMPDIR:-/tmp}/tmnotify-interactive.XXXXXXXX")
socket="$test_directory/tmux.sock"
xdg_config="$test_directory/xdg-config"
xdg_state="$test_directory/xdg-state"
xdg_runtime="$test_directory/xdg-runtime"
fake_home="$test_directory/home"
project="$test_directory/project"
results="$test_directory/results.md"
client_pids=

cleanup() {
    "$tmux_binary" -S "$socket" -f /dev/null kill-server >/dev/null 2>&1 || true
    for pid in $client_pids; do
        kill "$pid" >/dev/null 2>&1 || true
        wait "$pid" 2>/dev/null || true
    done
    if [ -n "$artifact_directory" ]; then
        mkdir -p "$artifact_directory"
        cp "$results" "$artifact_directory/interactive-smoke.md"
    fi
    rm -rf "$test_directory"
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$xdg_config/tmnotify" "$xdg_state" "$xdg_runtime" "$fake_home" "$project"
chmod 700 "$xdg_config" "$xdg_config/tmnotify" "$xdg_state" "$xdg_runtime" "$fake_home" "$project"
export XDG_CONFIG_HOME="$xdg_config"
export XDG_STATE_HOME="$xdg_state"
export XDG_RUNTIME_DIR="$xdg_runtime"
export HOME="$fake_home"
export TERM=xterm-256color
export NO_COLOR=1

run_tmux() {
    "$tmux_binary" -S "$socket" -f /dev/null "$@"
}

run_tmnotify() {
    "$tmnotify_binary" -S "$socket" "$@"
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

floating_count_is() {
    expected=$1
    window=$2
    actual=$(run_tmux list-panes -t "$window" -F '#{pane_floating_flag}' 2>/dev/null | awk '$1 == 1 { count++ } END { print count + 0 }')
    [ "$actual" -eq "$expected" ]
}

history_count_is() {
    expected=$1
    actual=$(run_tmnotify history --json | awk 'NF { count++ } END { print count + 0 }')
    [ "$actual" -eq "$expected" ]
}

daemon_count_is() {
    expected=$1
    expected_socket=$2
    actual=$(ps -ax -o command= | awk -v binary="$tmnotify_binary" -v socket="$expected_socket" '$1 == binary && $2 == "--socket-path" && $3 == socket && $4 == "__daemon" { count++ } END { print count + 0 }')
    [ "$actual" -eq "$expected" ]
}

daemon_pid() {
    expected_socket=$1
    ps -ax -o pid= -o command= | awk -v binary="$tmnotify_binary" -v socket="$expected_socket" '$2 == binary && $3 == "--socket-path" && $4 == socket && $5 == "__daemon" { print $1; exit }'
}

display_client_count_is() {
    expected=$1
    actual=$(run_tmux list-clients -F '#{client_control_mode}' | awk '$1 == 0 { count++ } END { print count + 0 }')
    [ "$actual" -eq "$expected" ]
}

modal_count_is() {
    expected=$1
    actual=$(run_tmux list-panes -a -F '#{pane_modal_flag}' 2>/dev/null | awk '$1 == 1 { count++ } END { print count + 0 }')
    [ "$actual" -eq "$expected" ]
}

window_modal_count_is() {
    expected=$1
    window=$2
    actual=$(run_tmux list-panes -t "$window" -F '#{pane_modal_flag}' 2>/dev/null | awk '$1 == 1 { count++ } END { print count + 0 }')
    [ "$actual" -eq "$expected" ]
}

record() {
    id=$1
    command_class=$2
    printf '| %s | %s | PASS |\n' "$id" "$command_class" >> "$results"
}

source_environment() {
    TMUX="$socket,$(run_tmux display-message -p '#{pid}'),0"
    TMUX_PANE=$1
    export TMUX TMUX_PANE
}

start_client() {
    session=$1
    if [ "$(uname -s)" = Darwin ]; then
        script -q /dev/null "$tmux_binary" -S "$socket" -f /dev/null attach-session -t "$session" >/dev/null 2>&1 &
    else
        script -q -c "$tmux_binary -S $socket -f /dev/null attach-session -t $session" /dev/null >/dev/null 2>&1 &
    fi
    pid=$!
    client_pids="$client_pids $pid"
}

{
    printf '# Isolated interactive release smoke\n\n'
    printf -- '- OS: %s\n' "$os_version"
    printf -- '- Architecture: %s\n' "$architecture"
    printf -- '- Rust: %s\n' "$rust_version"
    printf -- '- tmux: %s\n' "$tmux_version"
    printf -- '- tmux revision: %s\n' "$tmux_revision"
    printf -- '- Isolation: explicit temporary -S socket, -f /dev/null, disposable XDG/HOME, tmux PTYs\n'
    printf -- '- Redaction: Notification content, provider payloads, IDs, credentials, and renderer tokens are not captured\n\n'
    printf '| Check | Sanitized command class | Result |\n|---|---|---|\n'
} > "$results"

run_tmux new-session -d -s shared -x 120 -y 40 'sleep 300'
run_tmux new-session -d -s distinct -x 120 -y 40 'sleep 300'
start_client shared
start_client shared
start_client distinct
wait_for 'three PTY display clients' display_client_count_is 3

source_pane=$(run_tmux display-message -p -t shared:0 '#{pane_id}')
source_environment "$source_pane"

# RS-01: two concurrent first requests must converge on one daemon and two rows.
run_tmnotify send --no-source --timeout never --key smoke-race-a 'synthetic-a' >/dev/null &
race_one=$!
run_tmnotify send --no-source --timeout never --key smoke-race-b 'synthetic-b' >/dev/null &
race_two=$!
wait "$race_one"
wait "$race_two"
wait_for 'one lazy daemon for the exact socket' daemon_count_is 1 "$socket"
wait_for 'two History rows' history_count_is 2
record RS-01 'two concurrent send requests [content omitted]'

# RS-02: shared clients deduplicate by window; distinct clients receive their own display.
shared_window=$(run_tmux display-message -p -t shared:0 '#{window_id}')
distinct_window=$(run_tmux display-message -p -t distinct:0 '#{window_id}')
active_before=$(run_tmux display-message -p -t "$shared_window" '#{pane_id}')
wait_for 'two shared-window displays' floating_count_is 2 "$shared_window"
wait_for 'two distinct-window displays' floating_count_is 2 "$distinct_window"
[ "$(run_tmux display-message -p -t "$shared_window" '#{pane_id}')" = "$active_before" ]
run_tmux new-window -d -t shared -n followed 'sleep 300'
run_tmux select-window -t shared:followed
followed_window=$(run_tmux display-message -p -t shared:followed '#{window_id}')
wait_for 'displays following the shared clients' floating_count_is 2 "$followed_window"
wait_for 'old shared window cleared' floating_count_is 0 "$shared_window"
wait_for 'distinct window unchanged' floating_count_is 2 "$distinct_window"
record RS-02 'PTY attach + send + select-window + topology assertions'

run_tmnotify dismiss --key smoke-race-a
run_tmnotify dismiss --key smoke-race-b
wait_for 'race displays dismissed' floating_count_is 0 "$followed_window"

# RS-03: observe each placement, narrow suppression, stack, update, persistent lifetime, and exit.
for placement in top-left top-center top-right bottom-left bottom-center bottom-right; do
    run_tmnotify send --no-source --timeout never --key smoke-placement --position "$placement" 'synthetic-placement'
    wait_for "$placement display" floating_count_is 1 "$followed_window"
    run_tmnotify update --key smoke-placement --timeout never 'synthetic-update'
    wait_for "$placement update" floating_count_is 1 "$followed_window"
    run_tmnotify dismiss --key smoke-placement
    wait_for "$placement exit" floating_count_is 0 "$followed_window"
done
run_tmux resize-window -t "$followed_window" -x 20 -y 12
run_tmnotify send --no-source --timeout never --key smoke-narrow 'synthetic-narrow'
sleep 0.4
floating_count_is 0 "$followed_window"
run_tmux resize-window -t "$followed_window" -x 120 -y 40
run_tmnotify dismiss --key smoke-narrow
run_tmnotify send --no-source --timeout never --key smoke-stack-a 'synthetic-stack-a'
run_tmnotify send --no-source --timeout never --key smoke-stack-b 'synthetic-stack-b'
wait_for 'stacked Toast displays' floating_count_is 2 "$followed_window"
run_tmnotify dismiss --key smoke-stack-a
run_tmnotify dismiss --key smoke-stack-b
record RS-03 'six placements + keyed update + narrow resize + stack + dismiss [content omitted]'

# RS-04: live jump resolves a moved stable pane; source loss is nonfatal.
source_pane=$(run_tmux split-window -d -P -F '#{pane_id}' -t shared:followed 'sleep 300')
source_environment "$source_pane"
run_tmnotify send --timeout never --key smoke-jump 'synthetic-jump'
run_tmux new-window -d -t shared -n source-destination 'sleep 300'
destination=$(run_tmux display-message -p -t shared:source-destination '#{pane_id}')
run_tmux join-pane -d -s "$source_pane" -t "$destination"
run_tmnotify jump --key smoke-jump
run_tmux display-message -p -t "$source_pane" '#{pane_id}' >/dev/null
source_environment "$source_pane"
run_tmnotify send --timeout never --key smoke-lost 'synthetic-lost'
run_tmux kill-pane -t "$source_pane"
if run_tmnotify jump --key smoke-lost >/dev/null 2>&1; then
    echo 'jump unexpectedly succeeded after Source Pane loss' >&2
    exit 1
fi
run_tmnotify dismiss --key smoke-lost
record RS-04 'move-pane/jump and killed-Source-Pane refusal [content omitted]'
shared_visible_window=$(run_tmux display-message -p -t shared:source-destination '#{window_id}')

# RS-05: restart on the already-proven topology with History disabled.
main_daemon=$(daemon_pid "$socket")
[ -n "$main_daemon" ]
kill -TERM "$main_daemon"
wait_for 'enabled-History daemon shutdown' daemon_count_is 0 "$socket"
sleep 0.2
history_before=$(find "$xdg_state/tmnotify" -type f -exec cksum {} \; | sort)
printf '[history]\nenabled = false\n' > "$xdg_config/tmnotify/config.toml"
chmod 600 "$xdg_config/tmnotify/config.toml"
run_tmnotify send --no-source --timeout never --key smoke-disabled-a 'synthetic-disabled-a'
run_tmnotify send --no-source --timeout never --key smoke-disabled-b 'synthetic-disabled-b'
attention_windows=$(run_tmux list-clients -F '#{client_control_mode} #{window_id}' | awk '$1 == 0 { print $2 }' | sort -u)
for attention_window in $attention_windows; do
    wait_for "disabled-History delivery in $attention_window" floating_count_is 2 "$attention_window"
done
history_after=$(find "$xdg_state/tmnotify" -type f -exec cksum {} \; | sort)
[ "$history_before" = "$history_after" ]
run_tmnotify dismiss --key smoke-disabled-a
run_tmnotify dismiss --key smoke-disabled-b
disabled_daemon=$(daemon_pid "$socket")
[ -n "$disabled_daemon" ]
kill -TERM "$disabled_daemon"
wait_for 'disabled-History daemon shutdown' daemon_count_is 0 "$socket"
rm "$xdg_config/tmnotify/config.toml"
run_tmnotify send --no-source --timeout never --key smoke-enabled-again 'synthetic-enabled-again'
run_tmnotify dismiss --key smoke-enabled-again
record RS-05 'History enabled/disabled delivery comparison [content omitted]'

# RS-06: modal queue, ignored input, dismiss, jump, Toast pause, and source loss.
attention_source=$(run_tmux display-message -p -t shared:source-destination '#{pane_id}')
source_environment "$attention_source"
run_tmnotify send --no-source --timeout 1s --key smoke-paused 'synthetic-paused'
run_tmnotify send --attention --key smoke-attention-a 'synthetic-attention-a'
run_tmnotify send --attention --key smoke-attention-b 'synthetic-attention-b'
attention_windows=$(run_tmux list-clients -F '#{client_control_mode} #{window_id}' | awk '$1 == 0 { print $2 }' | sort -u)
for attention_window in $attention_windows; do
    wait_for "modal in Attention Window $attention_window" window_modal_count_is 1 "$attention_window"
done
sleep 1.2
modal=$(run_tmux list-panes -a -F '#{pane_id} #{pane_modal_flag}' | awk '$2 == 1 { print $1; exit }')
[ -n "$modal" ]
run_tmux send-keys -t "$modal" x
sleep 0.2
run_tmux display-message -p -t "$modal" '#{pane_id}' >/dev/null
run_tmux send-keys -t "$modal" q
sleep 0.3
modal=$(run_tmux list-panes -a -F '#{pane_id} #{pane_modal_flag}' | awk '$2 == 1 { print $1; exit }')
[ -n "$modal" ]
run_tmux send-keys -t "$modal" Enter
wait_for 'paused Toast resumed after Attention' floating_count_is 1 "$shared_visible_window"
lost_attention_source=$(run_tmux split-window -d -P -F '#{pane_id}' -t "$attention_source" 'sleep 300')
source_environment "$lost_attention_source"
run_tmnotify send --attention --key smoke-attention-lost 'synthetic-attention-lost'
sleep 0.3
run_tmux kill-pane -t "$lost_attention_source"
modal=$(run_tmux list-panes -a -F '#{pane_id} #{pane_modal_flag}' | awk '$2 == 1 { print $1; exit }')
[ -n "$modal" ]
run_tmux send-keys -t "$modal" Enter
sleep 0.2
run_tmux display-message -p -t "$modal" '#{pane_id}' >/dev/null
run_tmux send-keys -t "$modal" q
wait_for 'lost-source Attention dismissed globally' modal_count_is 0
wait_for 'resumed Toast timeout completed' floating_count_is 0 "$shared_visible_window"
record RS-06 'queued Attention PTY keys, Toast pause, jump/dismiss/source-loss policy'

# RS-07: real tmux PTY resize/key/signal runs and checks stty restoration.
history_wrapper="$test_directory/history-wrapper.sh"
# The single-quoted lines are intentionally expanded by the generated script.
# shellcheck disable=SC2016
printf '%s\n' '#!/bin/sh' \
    'before=$(stty -g)' \
    '"$TMNOTIFY_SMOKE_BINARY" -S "$TMNOTIFY_SMOKE_SOCKET" __history-ui' \
    'status=$?' \
    'after=$(stty -g)' \
    'result=1' \
    '[ "$before" = "$after" ] && [ "$status" -eq 0 ] && result=0' \
    '[ "$result" -ne 0 ] || : > "$TMNOTIFY_SMOKE_DIRECTORY/$1.ok"' \
    '"$TMNOTIFY_SMOKE_TMUX" -S "$TMNOTIFY_SMOKE_SOCKET" -f /dev/null wait-for -S "$1"' \
    'exit "$result"' \
    > "$history_wrapper"
chmod 700 "$history_wrapper"
export TMNOTIFY_SMOKE_BINARY="$tmnotify_binary"
export TMNOTIFY_SMOKE_SOCKET="$socket"
export TMNOTIFY_SMOKE_TMUX="$tmux_binary"
export TMNOTIFY_SMOKE_DIRECTORY="$test_directory"
run_tmux set-environment -g TMNOTIFY_SMOKE_BINARY "$tmnotify_binary"
run_tmux set-environment -g TMNOTIFY_SMOKE_SOCKET "$socket"
run_tmux set-environment -g TMNOTIFY_SMOKE_TMUX "$tmux_binary"
run_tmux set-environment -g TMNOTIFY_SMOKE_DIRECTORY "$test_directory"
run_tmux wait-for history-normal &
history_waiter=$!
history_pane=$(run_tmux new-window -d -P -F '#{pane_id}' -t distinct -n history-normal "$history_wrapper" history-normal)
sleep 0.4
run_tmux resize-window -t "$history_pane" -x 80 -y 20
sleep 0.15
run_tmux resize-window -t "$history_pane" -x 40 -y 8
sleep 0.15
run_tmux resize-window -t "$history_pane" -x 120 -y 40
run_tmux send-keys -t "$history_pane" d
sleep 0.15
run_tmux send-keys -t "$history_pane" u
run_tmux send-keys -t "$history_pane" q
wait "$history_waiter"
[ -f "$test_directory/history-normal.ok" ]
run_tmux wait-for history-signal &
history_waiter=$!
history_pane=$(run_tmux new-window -d -P -F '#{pane_id}' -t distinct -n history-signal "$history_wrapper" history-signal)
sleep 0.4
run_tmux send-keys -t "$history_pane" C-c
wait "$history_waiter"
[ -f "$test_directory/history-signal.ok" ]
record RS-07 'History TUI PTY wide/compact/too-small + hide/undo + q/SIGINT'

# RS-08: both output formats are consumed without retaining their content.
plain_rows=$(run_tmnotify history --plain | awk 'NF { count++ } END { print count + 0 }')
[ "$plain_rows" -gt 0 ]
run_tmnotify history --json | jq -cse 'length > 0 and all(.[]; type == "object") and ([.[].updated_at] == ([.[].updated_at] | sort | reverse))' >/dev/null
record RS-08 'history plain through awk; history JSON through order validator'

# RS-09: all provider paths are under disposable HOME/project roots.
mkdir -p "$fake_home/.claude" "$fake_home/.codex" "$project/.codex"
chmod 700 "$fake_home/.claude" "$fake_home/.codex" "$project/.codex"
printf '{"unrelated":{"keep":true}}\n' > "$fake_home/.claude/settings.json"
printf '{"unrelated":{"keep":true}}\n' > "$fake_home/.codex/hooks.json"
printf '[project]\nname = "preserved"\n' > "$project/.codex/config.toml"
chmod 600 "$fake_home/.claude/settings.json" "$fake_home/.codex/hooks.json" "$project/.codex/config.toml"
(
    cd "$project"
    "$tmnotify_binary" hook install claude user
    "$tmnotify_binary" hook install codex user
    claude_once=$(cksum "$fake_home/.claude/settings.json")
    codex_once=$(cksum "$fake_home/.codex/hooks.json")
    "$tmnotify_binary" hook install claude user
    "$tmnotify_binary" hook install codex user
    [ "$claude_once" = "$(cksum "$fake_home/.claude/settings.json")" ]
    [ "$codex_once" = "$(cksum "$fake_home/.codex/hooks.json")" ]
    "$tmnotify_binary" hook status >/dev/null
    mkdir -p "$test_directory/relocated"
    cp "$tmnotify_binary" "$test_directory/relocated/tmnotify"
    "$test_directory/relocated/tmnotify" hook sync
    printf '[hooks]\nStop = [{ command = "preserved" }]\n' > "$fake_home/.codex/config.toml"
    chmod 600 "$fake_home/.codex/config.toml"
    if "$tmnotify_binary" hook install codex user >/dev/null 2>&1; then
        echo 'mixed Codex hook representations were not refused' >&2
        exit 1
    fi
    source_environment "$(run_tmux display-message -p -t shared:source-destination '#{pane_id}')"
    printf '%s' '{"session_id":"synthetic","hook_event_name":"Stop"}' | "$tmnotify_binary" -S "$socket" __hook-event codex
    "$tmnotify_binary" hook remove claude user
    "$tmnotify_binary" hook remove codex user --allow-mixed
)
jq -e '.unrelated.keep == true' "$fake_home/.claude/settings.json" "$fake_home/.codex/hooks.json" >/dev/null
record RS-09 'disposable hook install/idempotence/status/relocation/mixed/event/remove'

# RS-10: real renderer/process/server failures plus focused bounded fault seams.
run_tmnotify send --no-source --timeout never --key smoke-recovery 'synthetic-recovery'
wait_for 'recovery renderer' floating_count_is 1 "$shared_visible_window"
renderer=$(run_tmux list-panes -t "$shared_visible_window" -F '#{pane_id} #{pane_floating_flag}' | awk '$2 == 1 { print $1; exit }')
run_tmux kill-pane -t "$renderer"
wait_for 'renderer recreation' floating_count_is 1 "$shared_visible_window"
doctor_before=$(find "$xdg_config" "$xdg_state" -type f -exec cksum {} \; | sort)
run_tmnotify doctor --json >/dev/null 2>&1 || true
doctor_after=$(find "$xdg_config" "$xdg_state" -type f -exec cksum {} \; | sort)
[ "$doctor_before" = "$doctor_after" ]
run_tmux kill-server
for pid in $client_pids; do
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" 2>/dev/null || true
done
client_pids=
wait_for 'daemon exit after tmux shutdown' daemon_count_is 0 "$socket"
record RS-10 'renderer kill/recreate + daemon restart + read-only doctor + tmux shutdown'

printf '\nAll RS-01 through RS-10 checks passed.\n' >> "$results"
echo 'isolated interactive release smoke passed'

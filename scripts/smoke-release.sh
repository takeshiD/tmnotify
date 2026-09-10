#!/bin/sh
set -eu

[ "$#" -eq 1 ] || {
    echo "usage: $0 <tmnotify-binary>" >&2
    exit 2
}

binary=$1
[ -x "$binary" ] || {
    echo "release smoke binary is missing or not executable: $binary" >&2
    exit 2
}

"$binary" --version
help=$("$binary" --help)
for command in send update dismiss jump history hook doctor; do
    printf '%s\n' "$help" | grep -q "  $command" || {
        echo "release binary help is missing command: $command" >&2
        exit 1
    }
done
for hidden in __daemon __render-toast __render-attention __history-ui __hook-event; do
    if printf '%s\n' "$help" | grep -q "$hidden"; then
        echo "internal command leaked into public help: $hidden" >&2
        exit 1
    fi
done

"$binary" send --help >/dev/null
"$binary" history --help >/dev/null
"$binary" hook --help >/dev/null
"$binary" doctor --help >/dev/null

echo 'release command-surface smoke passed'

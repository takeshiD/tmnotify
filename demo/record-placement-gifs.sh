#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
output_directory="$repository_root/demo/placements"
cast_directory=${CAST_DIRECTORY:-}

command -v asciinema >/dev/null 2>&1 || {
    echo "asciinema is required to record placement demos" >&2
    exit 1
}
command -v agg >/dev/null 2>&1 || {
    echo "agg is required to render placement demos as GIFs" >&2
    exit 1
}

cd "$repository_root"
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    cargo build --quiet
fi

if [ ! -x "$repository_root/target/debug/tmnotify" ]; then
    echo "target/debug/tmnotify is missing; run cargo build first" >&2
    exit 1
fi

if [ -n "$cast_directory" ]; then
    mkdir -p "$cast_directory"
else
    cast_directory=$(mktemp -d "${TMPDIR:-/tmp}/tmnotify-casts.XXXXXXXX")
    cleanup() {
        rm -rf -- "$cast_directory"
    }
    trap cleanup EXIT HUP INT TERM
fi
mkdir -p "$output_directory"

for position in top-left top-center top-right bottom-left bottom-center bottom-right; do
    cast="$cast_directory/$position.cast"
    output="$output_directory/$position.gif"
    echo "recording $position" >&2
    asciinema rec \
        --quiet \
        --headless \
        --overwrite \
        --window-size 100x30 \
        --idle-time-limit 1 \
        --command "demo/placement-run.sh $position" \
        "$cast"
    echo "rendering $output" >&2
    agg \
        --quiet \
        --theme dracula \
        --font-size 18 \
        --cols 100 \
        --rows 30 \
        --fps-cap 30 \
        --idle-time-limit 1 \
        --last-frame-duration 1 \
        "$cast" \
        "$output"
done

echo "wrote placement GIFs under $output_directory" >&2

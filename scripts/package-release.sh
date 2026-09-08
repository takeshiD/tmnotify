#!/bin/sh
set -eu

usage() {
    echo "usage: $0 <target> <binary> <output-directory>" >&2
    exit 2
}

[ "$#" -eq 3 ] || usage

target=$1
binary=$2
output_directory=$3

case "$target" in
    x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu | x86_64-apple-darwin | aarch64-apple-darwin) ;;
    *)
        echo "unsupported release target: $target" >&2
        exit 2
        ;;
esac

[ -f "$binary" ] && [ -x "$binary" ] || {
    echo "release binary is missing or not executable: $binary" >&2
    exit 2
}

script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_directory/.." && pwd)
version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$repository_root/Cargo.toml" | head -n 1)
[ -n "$version" ] || {
    echo "could not read package version" >&2
    exit 2
}

archive_root="tmnotify-$version-$target"
mkdir -p "$output_directory"
output_directory=$(CDPATH= cd -- "$output_directory" && pwd)
archive="$output_directory/$archive_root.tar.gz"
epoch=${SOURCE_DATE_EPOCH:-0}

case "$epoch" in
    *[!0-9]* | "")
        echo "SOURCE_DATE_EPOCH must be a nonnegative integer" >&2
        exit 2
        ;;
esac

if command -v gtar >/dev/null 2>&1; then
    tar_command=gtar
elif tar --version 2>/dev/null | grep -q 'GNU tar'; then
    tar_command=tar
else
    echo "reproducible packaging requires GNU tar (gtar)" >&2
    exit 2
fi

stage=$(mktemp -d "${TMPDIR:-/tmp}/tmnotify-package.XXXXXXXX")
cleanup() {
    rm -rf "$stage"
}
trap cleanup EXIT HUP INT TERM

package_root="$stage/$archive_root"
mkdir -p "$package_root/docs/adr"
install -m 0755 "$binary" "$package_root/tmnotify"
for file in README.md LICENSE-MIT LICENSE-APACHE CONTEXT.md tmnotify-design.md; do
    install -m 0644 "$repository_root/$file" "$package_root/$file"
done
install -m 0644 "$repository_root/docs/release.md" "$package_root/docs/release.md"
install -m 0644 "$repository_root/docs/acceptance-matrix.md" "$package_root/docs/acceptance-matrix.md"
for file in "$repository_root"/docs/adr/*.md; do
    install -m 0644 "$file" "$package_root/docs/adr/$(basename -- "$file")"
done

raw_archive="$stage/package.tar"
(CDPATH= cd -- "$stage" && "$tar_command" \
    --sort=name \
    --format=ustar \
    --mtime="@$epoch" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -cf "$raw_archive" "$archive_root")
gzip -n -c "$raw_archive" > "$archive"

echo "$archive"

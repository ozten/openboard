#!/bin/sh
# Write or check SHA256SUMS for the release archives in a directory.
#
# Usage: scripts/release/checksums.sh write <dir>
#        scripts/release/checksums.sh check <dir>
#
# `write` covers every openboard-*.tar.gz in <dir>, in the format sha256sum
# and `shasum -a 256` both read: "<hex digest>  <file name>".
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: checksums.sh write|check <dir>" >&2
    exit 2
fi
mode=$1
dir=$2

if command -v sha256sum >/dev/null 2>&1; then
    sum() { sha256sum "$@"; }
elif command -v shasum >/dev/null 2>&1; then
    sum() { shasum -a 256 "$@"; }
else
    echo "checksums: neither sha256sum nor shasum is installed" >&2
    exit 1
fi

cd "$dir"
case $mode in
    write)
        set -- openboard-*.tar.gz
        if [ ! -f "$1" ]; then
            echo "checksums: no openboard-*.tar.gz in $dir" >&2
            exit 1
        fi
        sum "$@" > SHA256SUMS
        cat SHA256SUMS
        ;;
    check)
        sum -c SHA256SUMS
        ;;
    *)
        echo "usage: checksums.sh write|check <dir>" >&2
        exit 2
        ;;
esac

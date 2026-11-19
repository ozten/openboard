#!/bin/sh
# Check that a release tag matches the workspace version in Cargo.toml.
#
# Usage: scripts/release/check-tag.sh <tag> [path/to/Cargo.toml]
#
# Prints the version (the tag without its leading "v") on success. Fails if
# the tag is not of the form vMAJOR.MINOR.PATCH (with an optional pre-release
# suffix such as -rc.1) or differs from the version under [workspace.package].
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: check-tag.sh <tag> [Cargo.toml]" >&2
    exit 2
fi
tag=$1
manifest=${2:-Cargo.toml}

# A tag is untrusted input: accept only the characters a version can hold,
# on a single line.
case $tag in
    *'
'*)
        echo "check-tag: the tag must be a single line" >&2
        exit 1
        ;;
esac
if ! printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?$'; then
    echo "check-tag: '$tag' is not a release tag; expected vMAJOR.MINOR.PATCH, for example v0.1.0" >&2
    exit 1
fi

if [ ! -f "$manifest" ]; then
    echo "check-tag: $manifest not found" >&2
    exit 1
fi

# The first `version = "..."` line inside the [workspace.package] table.
version=$(awk '
    /^\[/ { in_table = ($0 ~ /^\[workspace\.package\][ \t]*(#.*)?$/) }
    in_table && /^[ \t]*version[ \t]*=/ {
        line = $0
        sub(/^[^"]*"/, "", line)
        sub(/".*$/, "", line)
        print line
        exit
    }
' "$manifest")

if [ -z "$version" ]; then
    echo "check-tag: no version found under [workspace.package] in $manifest" >&2
    exit 1
fi

if [ "v$version" != "$tag" ]; then
    echo "check-tag: tag $tag does not match the workspace version $version in $manifest" >&2
    echo "check-tag: bump the version in $manifest (and Cargo.lock), or tag v$version instead" >&2
    exit 1
fi

printf '%s\n' "$version"

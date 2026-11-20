#!/bin/sh
# Checks that the TypeScript SDK's version is the workspace version, and
# prints it. The SDK is published to npm from the same tag as the binaries,
# so the two versions move together.
#
#   check-sdk-version.sh [Cargo.toml] [package.json]

set -eu

if [ "$#" -gt 2 ]; then
    echo "usage: check-sdk-version.sh [Cargo.toml] [package.json]" >&2
    exit 2
fi
manifest=${1:-Cargo.toml}
package=${2:-sdk/typescript/package.json}

if [ ! -f "$package" ]; then
    echo "check-sdk-version: $package not found" >&2
    exit 1
fi

# The first "version" key in the file, which is the package's own.
sdk_version=$(awk '
    /^[ \t]*"version"[ \t]*:/ {
        line = $0
        sub(/^[^:]*:[ \t]*"/, "", line)
        sub(/".*$/, "", line)
        print line
        exit
    }
' "$package")

if [ -z "$sdk_version" ]; then
    echo "check-sdk-version: no version found in $package" >&2
    exit 1
fi

if ! sh "$(dirname "$0")/check-tag.sh" "v$sdk_version" "$manifest" > /dev/null 2>&1; then
    echo "check-sdk-version: $package is at $sdk_version, which is not the workspace version in $manifest" >&2
    echo "check-sdk-version: set \"version\" in $package to the version under [workspace.package]" >&2
    exit 1
fi

printf '%s\n' "$sdk_version"

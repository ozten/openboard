#!/bin/sh
# Print the notes for a GitHub Release: the version's section of the
# changelog, then how to install that release.
#
# Usage: scripts/release/release-notes.sh <tag> [path/to/CHANGELOG.md]
#
# Fails, printing nothing, if the changelog has no section for the version.
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: release-notes.sh <tag> [CHANGELOG.md]" >&2
    exit 2
fi
tag=$1
changelog=${2:-CHANGELOG.md}

section=$(sh "$(dirname "$0")/changelog-section.sh" "${tag#v}" "$changelog")

printf '%s\n' "$section"
cat <<EOF

## Install or upgrade

\`\`\`sh
curl -fsSL https://raw.githubusercontent.com/ozten/openboard/main/scripts/install.sh | sh -s -- --version $tag
\`\`\`

[docs/upgrading.md](https://github.com/ozten/openboard/blob/main/docs/upgrading.md)
covers upgrading a running board and verifying a download by hand.
EOF

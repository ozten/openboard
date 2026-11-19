#!/bin/sh
# Print one version's section of CHANGELOG.md, without its heading.
#
# Usage: scripts/release/changelog-section.sh <version> [path/to/CHANGELOG.md]
#
# The section starts at a heading "## [<version>]" or "## <version>" (anything
# may follow on the line, such as a date) and ends before the next "## "
# heading. Fails if the section is missing or has no text, so a release is
# never published without notes.
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: changelog-section.sh <version> [CHANGELOG.md]" >&2
    exit 2
fi
version=${1#v}
changelog=${2:-CHANGELOG.md}

if [ -z "$version" ]; then
    echo "changelog-section: the version is empty" >&2
    exit 2
fi
if [ ! -f "$changelog" ]; then
    echo "changelog-section: $changelog not found" >&2
    exit 1
fi

# The version is compared as text, never used as a pattern. It reaches awk
# through the environment because `-v` would interpret backslashes.
section=$(VERSION=$version awk '
    BEGIN { version = ENVIRON["VERSION"]; n = length(version) }
    /^## / {
        if (found) { exit }
        heading = $0
        sub(/^##[ \t]+/, "", heading)
        sub(/^\[/, "", heading)
        # The version must be the whole first word: 0.1.0 is not 0.1.01.
        rest = substr(heading, n + 1, 1)
        if (substr(heading, 1, n) == version && (rest == "" || rest == "]" || rest == " " || rest == "\t")) {
            found = 1
        }
        next
    }
    # Link reference definitions at the foot of the file are not notes.
    found && /^\[[^]]+\]:[ \t]/ { next }
    found { print }
' "$changelog")

# Drop leading and trailing blank lines.
section=$(printf '%s\n' "$section" | awk '
    NF { started = 1 }
    started { lines[++n] = $0; if (NF) last = n }
    END { for (i = 1; i <= last; i++) print lines[i] }
')

if [ -z "$section" ]; then
    echo "changelog-section: no section with text for version $version in $changelog" >&2
    echo "changelog-section: add a heading '## [$version] - YYYY-MM-DD' with the notes under it" >&2
    exit 1
fi

printf '%s\n' "$section"

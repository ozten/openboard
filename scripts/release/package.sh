#!/bin/sh
# Pack the release archive for one target.
#
# Usage: scripts/release/package.sh <version> <target> <out-dir>
#
# Run from the repository root after scripts/release/build.sh <target>.
# Writes <out-dir>/openboard-<version>-<target>.tar.gz holding one directory,
# openboard-<version>-<target>/, with:
#   openboard, obd, LICENSE, README.md, skills/openboard/SKILL.md
set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: package.sh <version> <target> <out-dir>" >&2
    exit 2
fi
version=$1
target=$2
out_dir=$3

name="openboard-$version-$target"
bin_dir="target/$target/release"

for file in "$bin_dir/openboard" "$bin_dir/obd" LICENSE README.md skills/openboard/SKILL.md; do
    if [ ! -f "$file" ]; then
        echo "package: $file is missing" >&2
        exit 1
    fi
done

mkdir -p "$out_dir"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$name/skills/openboard"
cp "$bin_dir/openboard" "$bin_dir/obd" LICENSE README.md "$stage/$name/"
cp skills/openboard/SKILL.md "$stage/$name/skills/openboard/"
chmod 755 "$stage/$name" "$stage/$name/skills" "$stage/$name/skills/openboard"
chmod 755 "$stage/$name/openboard" "$stage/$name/obd"
chmod 644 "$stage/$name/LICENSE" "$stage/$name/README.md" "$stage/$name/skills/openboard/SKILL.md"

# Record root as the owner, not whoever ran the build. GNU tar (Linux) and
# bsdtar (macOS) spell that differently.
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
    tar --owner=0 --group=0 --numeric-owner -czf "$out_dir/$name.tar.gz" -C "$stage" "$name"
else
    # COPYFILE_DISABLE keeps macOS tar from adding ._* resource-fork entries.
    COPYFILE_DISABLE=1 tar --uid 0 --gid 0 --numeric-owner -czf "$out_dir/$name.tar.gz" -C "$stage" "$name"
fi

echo "$out_dir/$name.tar.gz"

#!/bin/sh
# Build the release binaries for one target, as the release workflow does.
#
# Usage: scripts/release/build.sh <target>
#
# Run from the repository root. Builds `openboard` and `obd` in release mode
# into target/<target>/release/, stripped, with the commit and its date
# embedded. The target must already be installed (`rustup target add`).
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: build.sh <target>" >&2
    exit 2
fi
target=$1

# The version identity the binaries print. Both values come from the commit,
# not from the clock, so two builds of one commit say the same thing.
OPENBOARD_BUILD_COMMIT=$(git rev-parse --short=12 HEAD)
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct HEAD)
export OPENBOARD_BUILD_COMMIT SOURCE_DATE_EPOCH

# Strip symbols at link time. Cargo does this the right way on each
# platform, including keeping the ad-hoc signature macOS needs on arm64.
export CARGO_PROFILE_RELEASE_STRIP=symbols

case $target in
    aarch64-unknown-linux-musl)
        # ring and the bundled SQLite are C. musl-tools provides musl-gcc for
        # the machine's own architecture, so nothing is cross-compiled.
        export CC_aarch64_unknown_linux_musl="${CC_aarch64_unknown_linux_musl:-musl-gcc}"
        ;;
    x86_64-unknown-linux-musl)
        export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}"
        ;;
    *-apple-darwin)
        # The oldest macOS the binaries run on.
        export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
        ;;
esac

echo "building $target at $OPENBOARD_BUILD_COMMIT (SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH)" >&2
cargo build --release --locked --target "$target" -p openboard-cli -p openboard-server

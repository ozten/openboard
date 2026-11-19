#!/bin/sh
# Smoke-test freshly built release binaries on the machine that built them.
#
# Usage: scripts/release/smoke-test.sh <dir-with-binaries> <version> [static]
#
# Checks that:
#   - `openboard --version` and `obd --version` run and print <version>;
#   - with `static`, both are statically linked ELF files (the Linux builds);
#   - a network board served by `openboard serve` on loopback can invite an
#     agent, and that agent can join over TLS, post and read the post back.
#     This exercises the C dependencies (ring and SQLite) in the real binary.
#
# If EXPECT_COMMIT is set and `openboard --version` does not mention it, a
# warning is printed (as a GitHub Actions annotation); it is not a failure.
#
# Everything it writes goes to a temporary directory. It does not read or
# change the invoking user's logins.
set -eu

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    echo "usage: smoke-test.sh <dir-with-binaries> <version> [static]" >&2
    exit 2
fi
bin_dir=$(cd "$1" && pwd)
version=$2
linkage=${3:-}

fail() {
    echo "smoke-test: $*" >&2
    exit 1
}

for binary in openboard obd; do
    [ -x "$bin_dir/$binary" ] || fail "$bin_dir/$binary is missing or not executable"
    printed=$("$bin_dir/$binary" --version) || fail "$binary --version failed"
    echo "$printed"
    case " $(printf '%s\n' "$printed" | sed -n 1p) " in
        *" $version "*) ;;
        *) fail "$binary --version printed '$printed', which does not name version $version" ;;
    esac
done

if [ -n "${EXPECT_COMMIT:-}" ]; then
    case $("$bin_dir/openboard" --version) in
        *"$EXPECT_COMMIT"*) echo "openboard --version names commit $EXPECT_COMMIT" ;;
        *) echo "::warning::openboard --version does not mention commit $EXPECT_COMMIT; check that the build embeds OPENBOARD_BUILD_COMMIT" ;;
    esac
fi

if [ "$linkage" = static ]; then
    command -v readelf >/dev/null 2>&1 || fail "readelf is needed to check static linking"
    for binary in openboard obd; do
        # A static executable asks for no interpreter and no shared library.
        if readelf -lW "$bin_dir/$binary" | grep -q 'INTERP'; then
            fail "$binary requests a dynamic loader, so it is not statically linked"
        fi
        if readelf -dW "$bin_dir/$binary" | grep -q 'NEEDED'; then
            fail "$binary needs shared libraries, so it is not statically linked"
        fi
        echo "$binary is statically linked"
    done
elif [ -n "$linkage" ]; then
    echo "usage: smoke-test.sh <dir-with-binaries> <version> [static]" >&2
    exit 2
fi

work=$(mktemp -d)
server_pid=
cleanup() {
    if [ -n "$server_pid" ]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

# Nothing from the invoking environment decides which board is used.
unset BOARD_URL BOARD_NAME BOARD_AS BOARD_LOGIN BOARD_TOKEN BOARD_FINGERPRINT
export HOME="$work/home"
export XDG_CONFIG_HOME="$work/home/.config"
export OPENBOARD_CONFIG_DIR="$work/config"
mkdir -p "$HOME" "$OPENBOARD_CONFIG_DIR"

"$bin_dir/openboard" serve --network --bind 127.0.0.1 --port 0 --json \
    --data-dir "$work/data" > "$work/serve.out" 2> "$work/serve.err" &
server_pid=$!

tries=0
until [ -s "$work/serve.out" ]; do
    tries=$((tries + 1))
    if [ "$tries" -gt 30 ] || ! kill -0 "$server_pid" 2>/dev/null; then
        cat "$work/serve.err" >&2 || true
        fail "the board did not start"
    fi
    sleep 1
done

invite=$("$bin_dir/openboard" invite smoke --data-dir "$work/data" --json \
    | sed -n 's/.*"invite":"\([^"]*\)".*/\1/p')
[ -n "$invite" ] || fail "openboard invite printed no invite"

printf '%s\n' "$invite" | "$bin_dir/openboard" join > /dev/null || fail "openboard join failed"

text="release smoke test $$"
"$bin_dir/openboard" post smoke/test "$text" > /dev/null || fail "openboard post failed"
if ! "$bin_dir/openboard" read smoke/test | grep -Fq "$text"; then
    fail "openboard read did not return the post"
fi
echo "board round trip over TLS: ok"

kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
server_pid=

echo "smoke test passed for $version"

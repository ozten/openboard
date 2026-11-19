#!/bin/sh
# Install or upgrade Openboard from a GitHub Release.
#
#   curl -fsSL https://raw.githubusercontent.com/ozten/openboard/main/scripts/install.sh | sh
#
# With options:
#
#   curl -fsSL https://raw.githubusercontent.com/ozten/openboard/main/scripts/install.sh | sh -s -- --version v0.1.1
#
# Running it again upgrades in place. It needs curl, tar and either
# sha256sum or shasum, never root, and it contacts only GitHub: one request
# for the list of releases (skipped with --version) and two downloads. It
# sends nothing about you or this machine beyond those requests.
#
# The whole script is one function called on the last line, so a download
# that is cut short runs nothing.
set -eu

usage() {
    cat <<'EOF'
Install or upgrade Openboard from a GitHub Release.

Usage: install.sh [options]

Options:
  --version <tag>   Release to install, for example v0.1.1 (default: the newest)
  --dir <dir>       Where to put the binaries (default: ~/.local/bin)
  --no-obd          Install only `openboard`, not the server binary `obd`
  -h, --help        Show this help

Environment variables, each overridden by its flag:
  OPENBOARD_VERSION       Same as --version
  OPENBOARD_INSTALL_DIR   Same as --dir
  OPENBOARD_NO_OBD=1      Same as --no-obd

For testing the installer only:
  OPENBOARD_INSTALL_BASE_URL   Take releases from this URL instead of GitHub.
                               It must serve releases.json (the GitHub API's
                               list of releases) and
                               download/<tag>/<file>. Only https:// URLs and
                               http:// on 127.0.0.1 or localhost are accepted.
EOF
}

say() {
    printf '%s\n' "$*"
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "this installer needs \`$1\`, which was not found on PATH"
}

# fetch <url> <file>: download over HTTPS only. Plain HTTP is accepted for a
# loopback address, which only the testing override can produce.
fetch() {
    case $1 in
        https://*)
            curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o "$2" "$1"
            ;;
        http://127.0.0.1:*|http://127.0.0.1/*|http://localhost:*|http://localhost/*)
            curl -fsSL -o "$2" "$1"
            ;;
        *)
            die "refusing to download over anything but HTTPS: $1"
            ;;
    esac
}

# sha256_of <file>: print the file's SHA-256 as lowercase hex, or fail.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        return 1
    fi
}

# physical <dir>: the directory's absolute path with symlinks resolved, or
# nothing if it does not exist.
physical() {
    case $1 in
        /*) (cd "$1" 2>/dev/null && pwd -P) || true ;;
    esac
}

# first_line <command...>: the first line a command prints, or nothing.
first_line() {
    "$@" 2>/dev/null | sed -n 1p || true
}

# server_pids <binary path> <openboard|obd>: the PIDs of servers running from
# that file. `obd` is always a server; `openboard` is one when run as
# `openboard serve`. A best effort: it finds nothing where it cannot look.
server_pids() {
    sp_path=$1
    sp_kind=$2
    if [ -d /proc/self ]; then
        for sp_proc in /proc/[0-9]*; do
            sp_exe=$(readlink "$sp_proc/exe" 2>/dev/null) || continue
            if [ "$sp_exe" = "$sp_path" ]; then
                printf '%s\n' "${sp_proc#/proc/}"
            fi
        done
    else
        # On macOS `comm` is the full path of the executable.
        ps -A -o pid= -o comm= 2>/dev/null | while read -r sp_pid sp_comm; do
            if [ "$sp_comm" = "$sp_path" ]; then
                printf '%s\n' "$sp_pid"
            fi
        done
    fi | while read -r sp_pid; do
        if [ "$sp_kind" = obd ]; then
            printf '%s\n' "$sp_pid"
        else
            case " $(ps -p "$sp_pid" -o args= 2>/dev/null) " in
                *" serve "*) printf '%s\n' "$sp_pid" ;;
            esac
        fi
    done
}

# install_binary <source> <destination>: replace the destination in one step.
# The new file is written next to the old one and renamed over it, so a
# process running the old file keeps running it, undisturbed, until it exits.
install_binary() {
    partial="$2.new.$$"
    cp "$1" "$partial"
    chmod 755 "$partial"
    mv -f "$partial" "$2"
    partial=
}

cleanup() {
    if [ -n "${work_dir:-}" ]; then
        rm -rf "$work_dir"
    fi
    if [ -n "${partial:-}" ]; then
        rm -f "$partial"
    fi
}

main() {
    version=${OPENBOARD_VERSION:-}
    install_dir=${OPENBOARD_INSTALL_DIR:-}
    no_obd=${OPENBOARD_NO_OBD:-}
    work_dir=
    partial=

    while [ "$#" -gt 0 ]; do
        case $1 in
            --version)
                [ "$#" -ge 2 ] || die "--version needs a value, for example --version v0.1.1"
                version=$2
                shift 2
                ;;
            --version=*)
                version=${1#--version=}
                shift
                ;;
            --dir)
                [ "$#" -ge 2 ] || die "--dir needs a directory"
                install_dir=$2
                shift 2
                ;;
            --dir=*)
                install_dir=${1#--dir=}
                shift
                ;;
            --no-obd)
                no_obd=1
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                usage >&2
                die "unknown option: $1"
                ;;
        esac
    done
    case $no_obd in
        ''|0|false|no) no_obd= ;;
    esac

    if [ -n "${OPENBOARD_INSTALL_BASE_URL:-}" ]; then
        base=${OPENBOARD_INSTALL_BASE_URL%/}
        download_url="$base/download"
        releases_url="$base/releases.json"
        say "taking releases from $base (OPENBOARD_INSTALL_BASE_URL is set; this is for testing)"
    else
        download_url="https://github.com/ozten/openboard/releases/download"
        releases_url="https://api.github.com/repos/ozten/openboard/releases?per_page=10"
    fi

    need curl
    need tar
    need uname
    need mktemp

    # Which build this machine needs.
    os=$(uname -s)
    arch=$(uname -m)
    case $arch in
        arm64|aarch64) arch=aarch64 ;;
        x86_64|amd64) arch=x86_64 ;;
        *) die "there is no release for $os on $arch; releases cover macOS and Linux on arm64 and x86_64. Build from source instead: https://github.com/ozten/openboard#readme" ;;
    esac
    case $os in
        Darwin)
            # A shell under Rosetta reports x86_64 on Apple silicon.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                arch=aarch64
            fi
            target="$arch-apple-darwin"
            ;;
        Linux)
            target="$arch-unknown-linux-musl"
            ;;
        *)
            die "there is no release for $os; releases cover macOS and Linux on arm64 and x86_64. Build from source instead: https://github.com/ozten/openboard#readme"
            ;;
    esac

    # Where to install.
    if [ -z "$install_dir" ]; then
        [ -n "${HOME:-}" ] || die "HOME is not set; pass --dir <dir>"
        install_dir="$HOME/.local/bin"
    fi
    mkdir -p "$install_dir" || die "cannot create $install_dir; pass --dir <dir> to install somewhere you can write"
    install_dir=$(cd "$install_dir" && pwd)
    physical_dir=$(cd "$install_dir" && pwd -P)
    [ -w "$install_dir" ] || die "cannot write to $install_dir; pass --dir <dir> to install somewhere you can write (this installer never needs root)"

    work_dir=$(mktemp -d)
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    # Which release.
    if [ -z "$version" ]; then
        fetch "$releases_url" "$work_dir/releases.json" \
            || die "could not get the list of releases from $releases_url. Check the network, or name a release: --version v0.1.0"
        version=$(tr ',' '\n' < "$work_dir/releases.json" \
            | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
            | sed -n 1p)
        [ -n "$version" ] || die "no release has been published yet. Build from source instead: https://github.com/ozten/openboard#readme"
    fi
    case $version in
        v*) ;;
        *) version="v$version" ;;
    esac
    if ! printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'; then
        die "'$version' is not a release tag; expected something like v0.1.0"
    fi

    archive="openboard-${version#v}-$target.tar.gz"
    say "installing Openboard $version for $target into $install_dir"

    fetch "$download_url/$version/$archive" "$work_dir/$archive" \
        || die "could not download $archive for release $version. Either that release does not exist or it has no build for $target; see https://github.com/ozten/openboard/releases"
    fetch "$download_url/$version/SHA256SUMS" "$work_dir/SHA256SUMS" \
        || die "could not download SHA256SUMS for release $version, so the archive cannot be verified. Nothing was installed"

    # Verify before anything from the archive is read.
    expected=$(awk -v file="$archive" '$2 == file || $2 == "*" file { print $1; exit }' "$work_dir/SHA256SUMS")
    [ -n "$expected" ] || die "SHA256SUMS of release $version does not list $archive, so it cannot be verified. Nothing was installed"
    actual=$(sha256_of "$work_dir/$archive") \
        || die "neither sha256sum nor shasum is installed, so the download cannot be verified. Nothing was installed"
    if [ "$expected" != "$actual" ]; then
        die "checksum mismatch for $archive: expected $expected, got $actual. The download is corrupt or has been tampered with. Nothing was installed"
    fi
    say "checksum verified: $actual"

    # Unpack into the temporary directory, refusing paths that would leave it.
    if tar -tzf "$work_dir/$archive" | grep -Eq '^/|(^|/)\.\.(/|$)'; then
        die "$archive holds a path outside its own directory. Nothing was installed"
    fi
    mkdir "$work_dir/unpacked"
    tar -xzf "$work_dir/$archive" -C "$work_dir/unpacked"
    unpacked="$work_dir/unpacked/openboard-${version#v}-$target"

    binaries=openboard
    if [ -z "$no_obd" ]; then
        binaries="openboard obd"
    fi
    for binary in $binaries; do
        [ -f "$unpacked/$binary" ] || die "$archive does not contain $binary. Nothing was installed"
        "$unpacked/$binary" --version >/dev/null 2>&1 \
            || die "the downloaded $binary does not run on this machine ($os $arch). Nothing was installed"
    done

    # Look for running servers before the files are replaced.
    running=
    for binary in $binaries; do
        if [ -f "$install_dir/$binary" ]; then
            pids=$(server_pids "$physical_dir/$binary" "$binary" | tr '\n' ' ')
            if [ -n "$pids" ]; then
                running="$running $install_dir/$binary (pid ${pids% })
"
            fi
        fi
    done

    for binary in $binaries; do
        old=
        if [ -f "$install_dir/$binary" ]; then
            old=$(first_line "$install_dir/$binary" --version)
            [ -n "$old" ] || old="an unknown version"
        fi
        install_binary "$unpacked/$binary" "$install_dir/$binary"
        new=$(first_line "$install_dir/$binary" --version)
        if [ -z "$old" ]; then
            say "installed $install_dir/$binary: $new"
        elif [ "$old" = "$new" ]; then
            say "reinstalled $install_dir/$binary: $new"
        else
            say "upgraded $install_dir/$binary: $old -> $new"
        fi
    done
    if [ -n "$no_obd" ] && [ -f "$install_dir/obd" ]; then
        say "left $install_dir/obd as it was (--no-obd): $(first_line "$install_dir/obd" --version)"
    fi

    if [ -n "$running" ]; then
        say ""
        say "A server is running from a file that was just replaced:"
        printf '%s' "$running"
        say "It keeps running the old version until you stop it and start it again."
        say "See https://github.com/ozten/openboard/blob/main/docs/upgrading.md"
    fi

    # PATH: is the directory on it, and does something else come first?
    # Entries are compared as physical directories, so a symlinked or
    # dot-dot spelling of the same directory counts.
    on_path=
    old_ifs=$IFS
    IFS=:
    set -f
    for entry in ${PATH:-}; do
        if [ "$(physical "$entry")" = "$physical_dir" ]; then
            on_path=1
            break
        fi
    done
    set +f
    IFS=$old_ifs
    if [ -z "$on_path" ]; then
        say ""
        warn "$install_dir is not on your PATH. Add this line to your shell's profile, then open a new shell:"
        # shellcheck disable=SC2016 # $PATH is meant to reach the profile unexpanded
        printf '  export PATH="%s:$PATH"\n' "$install_dir" >&2
    fi
    for binary in $binaries; do
        found=$(command -v "$binary" 2>/dev/null || true)
        case $found in
            /*) [ "$(physical "${found%/*}")" != "$physical_dir" ] || found= ;;
        esac
        case $found in
            '') ;;
            /*)
                say ""
                if [ -n "$on_path" ]; then
                    warn "\`$binary\` runs $found, which comes before $install_dir on your PATH, so the version just installed is shadowed: $(first_line "$found" --version)"
                else
                    warn "\`$binary\` still runs $found: $(first_line "$found" --version)"
                fi
                case $found in
                    */.cargo/bin/*)
                        if [ "$binary" = obd ]; then
                            crate=openboard-server
                        else
                            crate=openboard-cli
                        fi
                        printf '  that one came from a source build; remove it with: cargo uninstall %s\n' "$crate" >&2
                        ;;
                    *)
                        printf '  remove it, or put %s earlier on your PATH\n' "$install_dir" >&2
                        ;;
                esac
                ;;
        esac
    done

    say ""
    say "Check with: openboard --version"
    say "Agents keep a copy of the skill file. Refresh each one with: openboard skill --install <dir>"
}

main "$@"

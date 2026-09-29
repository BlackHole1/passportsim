#!/bin/sh
# Installs the PassportSim CLI on macOS (Apple silicon) from the project's GitHub Releases.
#
#   curl -fsSL https://passportsim.bugs.cc/install.sh | sh
#
# It reads the release's SHA256SUMS.txt, takes the one archive named
# passportsim-<version>-macos-arm64.tar.gz, checks its SHA-256, unpacks the package into
# ~/.local/share/passportsim/<version>/ and links ~/.local/bin/passportsim to its binary. It needs
# no administrator rights and edits no shell profile.
#
# Environment:
#   PASSPORTSIM_VERSION      the release to install, such as 0.1.0 (default: the latest release)
#   PASSPORTSIM_RELEASE_URL  for testing only: a URL holding the release's files, used instead of
#                            the GitHub download URL; needs PASSPORTSIM_VERSION
#
# Everything runs inside main, so a download cut short runs nothing.

set -eu

REPO="BlackHole1/passportsim"
ASSET_PATTERN='^passportsim-[^/]*-macos-arm64\.tar\.gz$'

say() {
    printf 'passportsim-install: %s\n' "$*"
}

fail() {
    printf 'passportsim-install: error: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || fail "\`$1\` is not on PATH; it ships with macOS, so check PATH"
}

# The Windows installer's one-liner, for the refusal on the wrong host.
WINDOWS_HINT='on Windows x64 run in PowerShell: irm https://passportsim.bugs.cc/install.ps1 | iex'

check_host() {
    os=$(uname -s)
    if [ "$os" != "Darwin" ]; then
        fail "this installer is for macOS on Apple silicon, and this system is $os; $WINDOWS_HINT; other systems are not supported (the browser version at https://passportsim.bugs.cc needs no install)"
    fi
    # `uname -m` says x86_64 in a shell running under Rosetta, so ask the hardware.
    if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" != "1" ]; then
        fail "this Mac has an Intel processor; PassportSim runs on macOS only on Apple silicon (the browser version at https://passportsim.bugs.cc needs no install)"
    fi
}

fetch() {
    if [ -n "${PASSPORTSIM_RELEASE_URL:-}" ]; then
        curl -fsSL --retry 3 -o "$2" "$1"
    else
        curl -fsSL --retry 3 --proto '=https' --tlsv1.2 -o "$2" "$1"
    fi
}

resolve_version() {
    if [ -n "${PASSPORTSIM_VERSION:-}" ]; then
        version=${PASSPORTSIM_VERSION#v}
    elif [ -n "${PASSPORTSIM_RELEASE_URL:-}" ]; then
        fail "PASSPORTSIM_RELEASE_URL needs PASSPORTSIM_VERSION, the version of the release it holds"
    else
        # The latest release page redirects to its tag; with no release it lands on /releases.
        latest=$(curl -fsSLI --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' \
            "https://github.com/$REPO/releases/latest") ||
            fail "cannot reach https://github.com/$REPO/releases/latest; check the network"
        case $latest in
            */releases/tag/v*) version=${latest##*/releases/tag/v} ;;
            *) fail "https://github.com/$REPO has no published release yet; set PASSPORTSIM_VERSION to install a pre-release" ;;
        esac
    fi
    case $version in
        '' | *[!0-9A-Za-z.+-]*) fail "\`$version\` is not a version, such as 0.1.0" ;;
    esac
}

main() {
    check_host
    need curl
    need shasum
    need tar
    resolve_version

    base=${PASSPORTSIM_RELEASE_URL:-https://github.com/$REPO/releases/download/v$version}
    base=${base%/}
    root="$HOME/.local/share/passportsim"
    dest="$root/$version"
    bin_dir="$HOME/.local/bin"
    link="$bin_dir/passportsim"

    # Never replace a passportsim this script did not install.
    if [ -e "$link" ] || [ -L "$link" ]; then
        [ -L "$link" ] || fail "$link exists and is not a link this installer made; move it away and run again"
        current=$(readlink "$link")
        case $current in
            "$root"/*) ;;
            *) fail "$link points to $current, which this installer did not install; remove the link and run again" ;;
        esac
    fi

    # Unpacked beside the destination, so the final move is a rename on one volume.
    partial="$root/.partial-$$"
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/passportsim-install.XXXXXX")
    trap 'rm -rf "$tmp" "$partial"' EXIT
    trap 'exit 1' HUP INT TERM

    say "installing PassportSim $version from $base"
    fetch "$base/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" ||
        fail "cannot download $base/SHA256SUMS.txt; is $version a published release?"

    # `sha256sum` lines: `<hex>  <name>`, or `<hex> *<name>` in binary mode.
    matches=$(tr -d '\r' <"$tmp/SHA256SUMS.txt" | awk -v pattern="$ASSET_PATTERN" '
        { name = $2; sub(/^\*/, "", name) }
        NF == 2 && $1 ~ /^[0-9A-Fa-f]+$/ && length($1) == 64 && name ~ pattern { print $1, name }')
    count=$(printf '%s' "$matches" | awk 'END { print NR }')
    [ "$count" = "1" ] ||
        fail "SHA256SUMS.txt of $version lists $count archives named passportsim-<version>-macos-arm64.tar.gz, expected one"
    expected=$(printf '%s' "${matches%% *}" | tr 'A-F' 'a-f')
    asset=${matches#* }

    fetch "$base/$asset" "$tmp/$asset" || fail "cannot download $base/$asset"
    actual=$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')
    [ "$actual" = "$expected" ] ||
        fail "$asset has SHA-256 $actual, but SHA256SUMS.txt says $expected; nothing was installed"
    say "verified $asset (sha256 $actual)"

    mkdir -p "$root"
    rm -rf "$partial"
    mkdir "$partial"
    tar -xzf "$tmp/$asset" -C "$partial" || fail "cannot unpack $asset"
    set -- "$partial"/*
    if [ "$#" -ne 1 ] || [ ! -x "$1/passportsim" ]; then
        rm -rf "$partial"
        fail "$asset does not hold one package directory with a passportsim binary"
    fi
    rm -rf "$dest"
    mv "$1" "$dest"
    rm -rf "$partial"

    mkdir -p "$bin_dir"
    ln -sfn "$dest/passportsim" "$link"

    say "installed the package in $dest"
    say "linked $link"
    "$dest/passportsim" --version || fail "the installed binary did not run: $dest/passportsim --version"

    case ":$PATH:" in
        *":$bin_dir:"*) say "run: passportsim --help" ;;
        *)
            say "$bin_dir is not on PATH. Add it for zsh, then open a new terminal:"
            # Printed literally: the user's shell expands it.
            # shellcheck disable=SC2016
            printf '\n    echo '\''export PATH="$HOME/.local/bin:$PATH"'\'' >> ~/.zshrc\n\n'
            say "until then, run $link"
            ;;
    esac

    others=$(cd "$root" && for dir in *; do
        [ -d "$dir" ] && [ "$dir" != "$version" ] && printf ' %s' "$dir"
    done || true)
    if [ -n "$others" ]; then
        say "other versions in $root:$others (remove them when you no longer need them)"
    fi
}

main "$@"

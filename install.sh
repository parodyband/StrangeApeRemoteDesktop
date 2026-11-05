#!/bin/sh
# Install (or update) Sard from the latest GitHub release:
#
#   curl -fsSL https://raw.githubusercontent.com/parodyband/StrangeApeRemoteDesktop/main/install.sh | sh
#
# Linux: installs into ~/.local (no root) with a launcher entry, then starts the
# tray app. macOS: installs Sard.app into /Applications (or ~/Applications) and
# opens it. The download is checked against the SHA-256 in the release manifest.
#
# Options (after `sh -s --` when piping):
#   --no-start    don't start Sard after installing
#   --uninstall   Linux: remove Sard again (pairings in ~/.config/sard stay)
set -eu

repo=parodyband/StrangeApeRemoteDesktop
base=${SARD_RELEASE_URL:-https://github.com/$repo/releases/latest/download}
start=yes
mode=install
for a in "$@"; do
    case "$a" in
        --no-start) start=no ;;
        --uninstall) mode=uninstall ;;
        -h|--help) sed -n '2,13p' "$0" 2>/dev/null || true; exit 0 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed"; }

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) platform=linux-x86_64 ;;
    Darwin-arm64) platform=macos-arm64 ;;
    *) die "there is no prebuilt Sard for $(uname -s) $(uname -m) yet; see https://github.com/$repo#building-from-source" ;;
esac

if [ "$mode" = uninstall ]; then
    [ "$platform" = linux-x86_64 ] || die "on macOS, quit Sard and move Sard.app to the Trash"
    pkill -f "$HOME/.local/bin/sard app" 2>/dev/null || true
    rm -f "$HOME/.local/bin/sard" "${XDG_DATA_HOME:-$HOME/.local/share}/applications/sard.desktop" \
        "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/sard.desktop"
    for s in 16 32 48 64 128 256 512; do
        rm -f "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/${s}x$s/apps/sard.png"
    done
    say "Sard removed (your pairings in ~/.config/sard are kept)."
    exit 0
fi

need curl
case "$base" in
    http://*) proto='=http,https' ;; # local testing only
    *) proto='=https' ;;
esac
fetch() { curl -fsSL --proto "$proto" --proto-redir "$proto" --connect-timeout 20 "$@"; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/sard-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch "$base/latest.txt" -o "$tmp/latest.txt" || die "could not download the release manifest from $base"
head -n 1 "$tmp/latest.txt" | grep -qx 'sard-update 1' || die "unexpected release manifest format"
version=$(sed -n 's/^version //p' "$tmp/latest.txt" | head -n 1)
line=$(grep "^asset $platform " "$tmp/latest.txt" | head -n 1) || die "the latest release has no $platform build"
# shellcheck disable=SC2086
set -- $line
sha=$3
url=$5

say "Downloading Sard $version for $platform..."
fetch "$url" -o "$tmp/download" || die "download failed: $url"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/download" | cut -d' ' -f1)
else
    actual=$(shasum -a 256 "$tmp/download" | cut -d' ' -f1)
fi
[ "$actual" = "$sha" ] || die "checksum mismatch (expected $sha, got $actual); not installing"

if [ "$platform" = linux-x86_64 ]; then
    need tar
    tar -xzf "$tmp/download" -C "$tmp"
    dir=$(find "$tmp" -mindepth 1 -maxdepth 1 -type d -name 'sard-*' | head -n 1)
    [ -n "$dir" ] && [ -x "$dir/install.sh" ] || die "unexpected archive layout"
    sh "$dir/install.sh"
    if [ "$start" = yes ] && { [ -n "${WAYLAND_DISPLAY:-}" ] || [ -n "${DISPLAY:-}" ]; }; then
        # A running copy is replaced by the new one.
        pkill -f "$HOME/.local/bin/sard app" 2>/dev/null && sleep 1 || true
        if command -v setsid >/dev/null 2>&1; then
            setsid -f "$HOME/.local/bin/sard" app >/dev/null 2>&1 </dev/null
        else
            nohup "$HOME/.local/bin/sard" app >/dev/null 2>&1 </dev/null &
        fi
        say "Sard is running: look for its icon in the system tray."
    fi
else
    need ditto
    ditto -x -k "$tmp/download" "$tmp/app"
    [ -d "$tmp/app/Sard.app" ] || die "unexpected archive layout"
    dest=${SARD_APP_DIR:-/Applications}
    [ -w "$dest" ] || { dest=$HOME/Applications; mkdir -p "$dest"; }
    pkill -f "$dest/Sard.app/Contents/MacOS/sard" 2>/dev/null && sleep 1 || true
    rm -rf "$dest/Sard.app.old"
    [ ! -e "$dest/Sard.app" ] || mv "$dest/Sard.app" "$dest/Sard.app.old"
    mv "$tmp/app/Sard.app" "$dest/Sard.app"
    rm -rf "$dest/Sard.app.old"
    # Command-line access: `sard connect <host>` etc.
    mkdir -p "$HOME/.local/bin"
    ln -sf "$dest/Sard.app/Contents/MacOS/sard" "$HOME/.local/bin/sard"
    say "Installed $dest/Sard.app (command line: ~/.local/bin/sard)."
    if [ "$start" = yes ]; then
        open "$dest/Sard.app"
        say "Sard is running: look for its icon in the menu bar."
    fi
fi

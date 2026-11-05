#!/bin/sh
# Install sard from this unpacked release into your home directory (no root):
#
#   ./install.sh               install or upgrade: ~/.local/bin/sard, menu entry, icons
#   ./install.sh --uninstall   remove them again (your pairings in ~/.config/sard stay)
#
# PREFIX=/some/dir ./install.sh installs into /some/dir/bin and /some/dir/share instead.
# Afterwards `sard update` (or the tray menu) keeps the binary up to date.
#
# From a source checkout, `packaging/linux/install.sh` installs target/release/sard
# (after `cargo build --release`). Such development builds don't update themselves.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
if [ -n "${PREFIX:-}" ]; then
    bindir=$PREFIX/bin
    datadir=$PREFIX/share
else
    bindir=$HOME/.local/bin
    datadir=${XDG_DATA_HOME:-$HOME/.local/share}
fi
sizes="16 32 48 64 128 256 512"

# Unpacked release tarball, or packaging/linux/ inside a source checkout.
bin=$here/sard
desktop=$here/share/applications/sard.desktop
icon_src() { echo "$here/share/icons/hicolor/${1}x$1/apps/sard.png"; }
if [ ! -e "$bin" ] && [ -f "$here/../../Cargo.toml" ] && [ -f "$here/sard.desktop" ]; then
    root=$(CDPATH='' cd -- "$here/../.." && pwd)
    bin=$root/target/release/sard
    desktop=$here/sard.desktop
    icon_src() { echo "$root/assets/icons/app/sard-$1.png"; }
fi

mode=install
case "${1:-}" in
    "") ;;
    --uninstall) mode=uninstall ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
esac

# Copy via a temporary file + rename, so a running sard keeps working and
# nothing ever sees a half-written file.
put() { # <mode> <src> <dest>
    mkdir -p "$(dirname "$3")"
    cp "$2" "$3.tmp.$$"
    chmod "$1" "$3.tmp.$$"
    mv -f "$3.tmp.$$" "$3"
}

refresh() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$datadir/applications" 2>/dev/null || true
    fi
    # Only refresh an icon cache that already exists: creating one in a user
    # directory would hide icons other apps install there later.
    touch "$datadir/icons/hicolor" 2>/dev/null || true
    if [ -f "$datadir/icons/hicolor/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f "$datadir/icons/hicolor" 2>/dev/null || true
    fi
}

if [ "$mode" = uninstall ]; then
    rm -f "$bindir/sard" "$datadir/applications/sard.desktop"
    for s in $sizes; do rm -f "$datadir/icons/hicolor/${s}x$s/apps/sard.png"; done
    refresh
    echo "Removed sard. Settings and pairings are kept in ${XDG_CONFIG_HOME:-$HOME/.config}/sard."
    exit 0
fi

[ -x "$bin" ] || { echo "error: $bin not found (run install.sh from the unpacked release, or cargo build --release first)" >&2; exit 1; }
"$bin" version >/dev/null || { echo "error: this sard build does not run on this system" >&2; exit 1; }

put 755 "$bin" "$bindir/sard"

# The menu entry runs the installed binary by absolute path, so it works even
# when ~/.local/bin is not on the desktop session's PATH. Paths with characters
# that would need desktop-entry quoting fall back to plain `sard`.
if [ -f "$desktop" ]; then
    case "$bindir" in
        *[!A-Za-z0-9/._+-]*) exe=sard ;;
        *) exe=$bindir/sard ;;
    esac
    mkdir -p "$datadir/applications"
    sed -e "s|^Exec=sard\$|Exec=$exe|" -e "s|^TryExec=sard\$|TryExec=$exe|" "$desktop" > "$datadir/applications/sard.desktop.tmp.$$"
    chmod 644 "$datadir/applications/sard.desktop.tmp.$$"
    mv -f "$datadir/applications/sard.desktop.tmp.$$" "$datadir/applications/sard.desktop"
fi
for s in $sizes; do
    icon=$(icon_src "$s")
    if [ -f "$icon" ]; then put 644 "$icon" "$datadir/icons/hicolor/${s}x$s/apps/sard.png"; fi
done
refresh

echo "Installed $("$bindir/sard" version) to $bindir/sard"
case ":$PATH:" in
    *":$bindir:"*) ;;
    *) echo "note: $bindir is not on your PATH; add it to use \`sard\` from a terminal." ;;
esac
echo "Start it from your application menu (Sard), or run \`sard help\`."

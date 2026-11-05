#!/bin/sh
# Build the Linux release tarball: <out>/sard-<version>-linux-<arch>.tar.gz
#
#   scripts/package-linux.sh [--no-build] [--out <dir>]
#
# Builds an official release binary (SARD_RELEASE_BUILD=1: it can update
# itself) unless --no-build, in which case target/release/sard is used as is.
# Layout of the tarball:
#
#   sard-<version>-linux-<arch>/
#     sard                                      the binary (no build-time system deps; GPU/OS libs are dlopen'd)
#     install.sh                                installs into ~/.local without root (--uninstall to remove)
#     share/applications/sard.desktop
#     share/icons/hicolor/<n>x<n>/apps/sard.png
#     LICENSE
set -eu

cd "$(dirname "$0")/.."
umask 022
build=1
out=dist
while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) build=0 ;;
        --out) out="$2"; shift ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

if [ "$build" = 1 ]; then
    SARD_RELEASE_BUILD=1 cargo build --release --locked -p sard
fi
bin=target/release/sard
[ -x "$bin" ] || { echo "missing $bin (build first)" >&2; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
reported=$("$bin" version)
[ "$reported" = "sard $version" ] || { echo "$bin reports '$reported', expected 'sard $version'" >&2; exit 1; }
arch=$(uname -m)
name=sard-$version-linux-$arch
stage=$out/$name
rm -rf "$stage" "$out/$name.tar.gz"
mkdir -p "$stage/share/applications"

install -m 755 "$bin" "$stage/sard"
install -m 755 packaging/linux/install.sh "$stage/install.sh"
install -m 644 packaging/linux/sard.desktop "$stage/share/applications/sard.desktop"
install -m 644 LICENSE "$stage/LICENSE"
missing=
for s in 16 32 48 64 128 256 512; do
    src=assets/icons/app/sard-$s.png
    if [ -f "$src" ]; then
        mkdir -p "$stage/share/icons/hicolor/${s}x$s/apps"
        install -m 644 "$src" "$stage/share/icons/hicolor/${s}x$s/apps/sard.png"
    else
        missing="$missing $s"
    fi
done
[ -z "$missing" ] || echo "warning: app icon sizes missing from assets/icons/app:$missing" >&2

# Reproducible archive: fixed owner, order and timestamps.
mtime=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$mtime" -C "$out" -cf - "$name" \
    | gzip -9n > "$out/$name.tar.gz"
rm -rf "$stage"
echo "packaged $out/$name.tar.gz"

#!/bin/sh
# Write the update manifest <dir>/latest.txt for the release packages in <dir>.
#
#   scripts/release-manifest.sh <dir> <version> <download-base-url> [<notes-url>]
#
# Lists sard-<version>-linux-x86_64.tar.gz, sard-<version>-macos-arm64.zip and
# sard-<version>-windows-x86_64.zip (whichever exist) with their SHA-256 and size. Sign it afterwards with
#   sard release-sign <key-file | -> <dir>/latest.txt      (writes latest.txt.sig)
# Format: see crates/sard/src/update.rs.
set -eu

[ $# -ge 3 ] || { sed -n '2,9p' "$0"; exit 2; }
dir=$1 version=$2 base=${3%/} notes=${4:-}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

{
    echo "sard-update 1"
    echo "version $version"
    [ -z "$notes" ] || echo "notes $notes"
    for platform in linux-x86_64 macos-arm64 windows-x86_64; do
        case $platform in
            linux-*) file=sard-$version-$platform.tar.gz ;;
            macos-* | windows-*) file=sard-$version-$platform.zip ;;
        esac
        [ -f "$dir/$file" ] || continue
        echo "asset $platform $(sha256 "$dir/$file") $(wc -c < "$dir/$file" | tr -d ' ') $base/$file"
    done
} > "$dir/latest.txt"
grep -q '^asset ' "$dir/latest.txt" || { echo "no sard-$version-* packages in $dir" >&2; rm -f "$dir/latest.txt"; exit 1; }
cat "$dir/latest.txt"

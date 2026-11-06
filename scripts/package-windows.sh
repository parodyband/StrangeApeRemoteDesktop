#!/bin/sh
# Package a Windows build as dist/sard-<version>-windows-x86_64.zip:
#
#   scripts/package-windows.sh <cargo target dir, e.g. target/release> <out dir>
#
# The zip holds one folder, sard-<version>-windows-x86_64/, with sard.exe, the
# windowless launcher sardw.exe and LICENSE (the self-updater expects exactly
# one top-level folder). Runs in Git Bash on Windows; needs 7z (GitHub's
# Windows runners have it) or zip.
set -eu

[ $# -eq 2 ] || { sed -n '2,9p' "$0"; exit 2; }
bin=$1 out=$2
version=$("$bin/sard.exe" version | tr -d '\r' | cut -d' ' -f2)
name=sard-$version-windows-x86_64
rm -rf "${out:?}/$name" "$out/$name.zip"
mkdir -p "$out/$name"
cp "$bin/sard.exe" "$bin/sardw.exe" LICENSE "$out/$name/"
if command -v 7z >/dev/null 2>&1; then
    (cd "$out" && 7z a -tzip -mx=9 "$name.zip" "$name" >/dev/null)
else
    (cd "$out" && zip -qr "$name.zip" "$name")
fi
rm -rf "${out:?}/$name"
echo "$out/$name.zip"

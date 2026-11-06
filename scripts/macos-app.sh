#!/bin/sh
# Build a minimal Sard.app bundle around the sard binary and code-sign it.
#
#   scripts/macos-app.sh [--debug] [--no-build] [--identity <name|hash|->] [--out <dir>]
#                        [--release] [--zip]
#
# macOS grants Screen Recording / Accessibility to the *signed app*, keyed by
# its signature. With a stable signing identity (an "Apple Development" or
# "Developer ID" certificate) the grants survive rebuilds. With an ad-hoc
# signature (identity "-") every rebuild looks like a new app and the grants
# must be given again. By default the first valid code-signing identity in
# the keychain is used, falling back to ad-hoc.
#
#   --release   Official release build (what CI publishes): the binary updates
#               itself (SARD_RELEASE_BUILD=1), and is signed with the hardened
#               runtime, a secure timestamp and packaging/macos/sard.entitlements
#               as notarization requires. Default identity: the first
#               "Developer ID Application" one; signing failures are fatal.
#   --zip       Also write <out>/sard-<version>-macos-<arch>.zip (ditto).
#   --no-timestamp  Skip the secure timestamp (for identities Apple's timestamp
#               service won't serve, e.g. Sard's own self-signed certificate).
#
# The app icon is built from assets/icons/app/sard-{16..1024}.png (skipped if
# they are missing). Run it afterwards with e.g.:
#   open -n dist/Sard.app --args host
#   open -n dist/Sard.app --stdout /tmp/sard.log --stderr /tmp/sard.log --args connect <host>
# or from a terminal (permissions are then attributed to the terminal app):
#   dist/Sard.app/Contents/MacOS/sard host
set -eu

cd "$(dirname "$0")/.."
profile=release
build=1
identity=auto
no_timestamp=0
out=dist
release=0
zip=0
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) profile=debug ;;
        --no-build) build=0 ;;
        --identity) identity="$2"; shift ;;
        --no-timestamp) no_timestamp=1 ;;
        --out) out="$2"; shift ;;
        --release) release=1 ;;
        --zip) zip=1 ;;
        -h|--help) sed -n '2,27p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

if [ "$(uname -s)" != Darwin ]; then
    echo "macos-app.sh must run on macOS" >&2
    exit 1
fi
if [ "$release" = 1 ] && [ "$profile" != release ]; then
    echo "--release needs a release build (drop --debug)" >&2
    exit 2
fi

if [ "$build" = 1 ]; then
    if [ "$release" = 1 ]; then export SARD_RELEASE_BUILD=1; fi
    if [ "$profile" = release ]; then cargo build --release -p sard; else cargo build -p sard; fi
fi
bin="target/$profile/sard"
[ -x "$bin" ] || { echo "missing $bin (build first)" >&2; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
app="$out/Sard.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/sard"

# App icon: assets/icons/app/sard-<px>.png → Resources/sard.icns.
icons=assets/icons/app
icon_plist=
have_icons=1
for px in 16 32 64 128 256 512 1024; do
    [ -f "$icons/sard-$px.png" ] || have_icons=0
done
if [ "$have_icons" = 1 ] && command -v iconutil >/dev/null 2>&1; then
    tmp=$(mktemp -d)
    iconset=$tmp/sard.iconset
    mkdir "$iconset"
    # iconutil names: icon_<pt>x<pt>[@2x].png
    for m in 16:16x16 32:16x16@2x 32:32x32 64:32x32@2x 128:128x128 256:128x128@2x \
             256:256x256 512:256x256@2x 512:512x512 1024:512x512@2x; do
        cp "$icons/sard-${m%%:*}.png" "$iconset/icon_${m#*:}.png"
    done
    iconutil -c icns -o "$app/Contents/Resources/sard.icns" "$iconset"
    rm -rf "$tmp"
    icon_plist='<key>CFBundleIconFile</key>              <string>sard</string>'
else
    echo "note: no app icon ($icons/sard-{16,32,64,128,256,512,1024}.png missing or no iconutil)" >&2
fi

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>            <string>com.strangeape.sard</string>
    <key>CFBundleName</key>                  <string>Sard</string>
    <key>CFBundleDisplayName</key>           <string>Sard</string>
    <key>CFBundleExecutable</key>            <string>sard</string>
    ${icon_plist}
    <key>CFBundlePackageType</key>           <string>APPL</string>
    <key>CFBundleShortVersionString</key>    <string>${version}</string>
    <key>CFBundleVersion</key>               <string>${version}</string>
    <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
    <key>LSMinimumSystemVersion</key>        <string>13.0</string>
    <key>LSApplicationCategoryType</key>     <string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key>       <true/>
    <!-- Background agent: the host has no window, so it must not sit in the
         Dock bouncing. The client switches itself to a regular app (Dock icon,
         menu) when it opens its window. -->
    <key>LSUIElement</key>                   <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key> <true/>
    <key>NSScreenCaptureUsageDescription</key>
    <string>Sard streams this Mac's screen to the remote-desktop client you paired with.</string>
    <key>NSLocalNetworkUsageDescription</key>
    <string>Sard connects directly to the paired computer on your local network.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>Sard can stream audio to the paired remote-desktop client.</string>
    <key>NSAppleEventsUsageDescription</key>
    <string>Sard forwards keyboard and mouse input from the paired remote-desktop client.</string>
</dict>
</plist>
EOF
plutil -lint -s "$app/Contents/Info.plist"

if [ "$identity" = auto ]; then
    if [ "$release" = 1 ]; then
        identity=$(security find-identity -v -p codesigning 2>/dev/null | awk -F'"' '/Developer ID Application/ {print $2; exit}')
        [ -n "$identity" ] || { echo "--release: no Developer ID Application identity (pass --identity, or - for ad-hoc)" >&2; exit 1; }
    else
        identity=$(security find-identity -v -p codesigning 2>/dev/null | awk 'NR==1 && /\)/ {print $2}')
        [ -n "$identity" ] || identity=-
    fi
fi
sign() {
    if [ "$release" = 1 ]; then
        # Hardened runtime + secure timestamp: required for notarization.
        ts=--timestamp
        { [ "$1" = - ] || [ "$no_timestamp" = 1 ]; } && ts=--timestamp=none
        codesign --force --sign "$1" --identifier com.strangeape.sard --options runtime "$ts" \
            --entitlements packaging/macos/sard.entitlements "$app"
    else
        codesign --force --sign "$1" --identifier com.strangeape.sard --timestamp=none "$app"
    fi
}
if [ "$identity" != - ]; then
    echo "signing with identity $identity"
    if ! sign "$identity"; then
        if [ "$release" = 1 ]; then
            echo "error: signing with $identity failed" >&2
            exit 1
        fi
        # Typically errSecInternalComponent: the keychain is locked (e.g. an
        # SSH session). Run once from Terminal, or `security unlock-keychain`.
        echo "warning: signing with $identity failed (locked keychain?); falling back to ad-hoc" >&2
        identity=-
    fi
fi
if [ "$identity" = - ]; then
    echo "signing ad-hoc (permission grants will reset on every rebuild)"
    sign -
fi
codesign --verify --strict "$app"
echo "built $app (sard $version)"

if [ "$zip" = 1 ]; then
    arch=$(lipo -archs "$bin" 2>/dev/null || uname -m)
    zipfile="$out/sard-$version-macos-$arch.zip"
    rm -f "$zipfile"
    ditto -c -k --keepParent "$app" "$zipfile"
    echo "packaged $zipfile"
fi

#!/bin/sh
# Build a minimal Sard.app bundle around the sard binary and code-sign it.
#
#   scripts/macos-app.sh [--debug] [--no-build] [--identity <name|hash|->] [--out <dir>]
#
# macOS grants Screen Recording / Accessibility to the *signed app*, keyed by
# its signature. With a stable signing identity (an "Apple Development" or
# "Developer ID" certificate) the grants survive rebuilds. With an ad-hoc
# signature (identity "-") every rebuild looks like a new app and the grants
# must be given again. By default the first valid code-signing identity in
# the keychain is used, falling back to ad-hoc.
#
# Run it afterwards with e.g.:
#   open -n dist/Sard.app --args host
#   open -n dist/Sard.app --stdout /tmp/sard.log --stderr /tmp/sard.log --args connect <host>
# or from a terminal (permissions are then attributed to the terminal app):
#   dist/Sard.app/Contents/MacOS/sard host
set -eu

cd "$(dirname "$0")/.."
profile=release
build=1
identity=auto
out=dist
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) profile=debug ;;
        --no-build) build=0 ;;
        --identity) identity="$2"; shift ;;
        --out) out="$2"; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

if [ "$(uname -s)" != Darwin ]; then
    echo "macos-app.sh must run on macOS" >&2
    exit 1
fi

if [ "$build" = 1 ]; then
    if [ "$profile" = release ]; then cargo build --release -p sard; else cargo build -p sard; fi
fi
bin="target/$profile/sard"
[ -x "$bin" ] || { echo "missing $bin (build first)" >&2; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
app="$out/Sard.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/sard"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>            <string>com.strangeape.sard</string>
    <key>CFBundleName</key>                  <string>Sard</string>
    <key>CFBundleDisplayName</key>           <string>Sard</string>
    <key>CFBundleExecutable</key>            <string>sard</string>
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

if [ "$identity" = auto ]; then
    identity=$(security find-identity -v -p codesigning 2>/dev/null | awk 'NR==1 && /\)/ {print $2}')
    [ -n "$identity" ] || identity=-
fi
sign() {
    codesign --force --sign "$1" --identifier com.strangeape.sard --timestamp=none "$app"
}
if [ "$identity" != - ]; then
    echo "signing with identity $identity"
    if ! sign "$identity"; then
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
echo "built $app"

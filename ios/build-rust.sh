#!/bin/sh
# Xcode build phase: build the Rust core (crates/mobile) as a static library
# for the platform being built, where LIBRARY_SEARCH_PATHS looks for it.
set -eu
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
case "${PLATFORM_NAME:-iphonesimulator}" in
    iphoneos) TARGET=aarch64-apple-ios ;;
    iphonesimulator) TARGET=aarch64-apple-ios-sim ;;
    *) echo "error: unsupported platform ${PLATFORM_NAME}" >&2; exit 1 ;;
esac
if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found; install Rust from https://rustup.rs" >&2
    exit 1
fi
rustup target add "$TARGET" >/dev/null 2>&1 || true
PROFILE=release
[ "${CONFIGURATION:-Release}" = Debug ] && PROFILE=dev
OUT_PROFILE=$([ "$PROFILE" = dev ] && echo debug || echo release)
# Xcode's environment targets the iOS SDK; build scripts of dependencies run
# on this Mac, so don't let it leak into cargo.
env -u SDKROOT -u LIBRARY_PATH -u CPATH -u ARCHS -u PLATFORM_NAME \
    cargo build --manifest-path "$ROOT/Cargo.toml" -p sard-mobile --target "$TARGET" --profile "$PROFILE"
DEST="${PROJECT_DIR:-$ROOT/ios}/build/rust/${PLATFORM_NAME:-iphonesimulator}/${CONFIGURATION:-Release}"
mkdir -p "$DEST"
cp "$ROOT/target/$TARGET/$OUT_PROFILE/libsard_mobile.a" "$DEST/libsard_mobile.a"

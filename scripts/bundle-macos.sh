#!/usr/bin/env bash
# Builds a universal (arm64 + x86_64) SoundSync.app, ad-hoc signs it and zips it to
# dist/SoundSync-macos-universal.zip. Runs on macOS with the Xcode command line tools and
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/dist/macos/SoundSync.app"
ZIP="$ROOT/dist/SoundSync-macos-universal.zip"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

cd "$ROOT"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release -p soundsync --target "$target"
done

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
BIN="$APP/Contents/MacOS/soundsync"
lipo -create -output "$BIN" \
  target/aarch64-apple-darwin/release/soundsync \
  target/x86_64-apple-darwin/release/soundsync
lipo -info "$BIN"

# Info.plist and the icon come from the binary itself, so they always match this build.
"$BIN" gen-icon --png "$TMP/1024.png" --plist "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"

ICONSET="$TMP/SoundSync.iconset"
mkdir "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$TMP/1024.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$TMP/1024.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/SoundSync.icns"

codesign --force --deep -s - "$APP"
codesign --verify --verbose "$APP"

rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
echo "Built $ZIP"

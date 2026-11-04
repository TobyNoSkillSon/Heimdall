#!/bin/bash
# Floating Islands (night 1022): build islands/ and put the helper at Contents/Helpers/FloatingIslands.app and the
# `islands` CLI at Contents/MacOS/islands inside the given Heimdall.app, each signed with the given identity, inner
# code first. scripts/bundle.sh calls it before it signs the outer bundle; never installs or launches anything.
# Usage: scripts/bundle_islands.sh <Heimdall.app> <signing identity> <swift scratch folder> [app bundle id]
# The helper's ID follows the app's: dev.heimdall.floatingislands for the personal app, <app id>.floatingislands else.
# ISLANDS_HELPER=rust (default since night 1023: crates/islands, the same app in Rust) or ISLANDS_HELPER=swift (the
# rollback: islands/ built with SwiftPM). Both land at the same paths with the same bundle id, flags and file protocol.
set -euo pipefail
[[ $# -eq 3 || $# -eq 4 ]] || { echo 'Usage: scripts/bundle_islands.sh <Heimdall.app> <identity> <scratch> [app id]' >&2; exit 1; }
BUNDLE="$1"
IDENTITY="$2"
SCRATCH="$3"
APP_ID="${4:-dev.heimdall.app}"
if [[ "$APP_ID" == dev.heimdall.app ]]; then HELPER_ID=dev.heimdall.floatingislands; else HELPER_ID="$APP_ID.floatingislands"; fi
KIND="${ISLANDS_HELPER:-rust}"
cd "$(dirname "$0")/.."
case "$KIND" in
  rust)
    export PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
    CC="${CC:-/usr/bin/clang}" nice -n 19 cargo build --release -p islands --bins -j 2
    BIN="$CARGO_TARGET_DIR/release"
    ;;
  swift)
    cd islands
    nice -n 19 swift build -c release -j 2 --scratch-path "$SCRATCH"
    BIN="$(swift build -c release --scratch-path "$SCRATCH" --show-bin-path)"
    cd ..
    ;;
  *) echo "ISLANDS_HELPER must be rust or swift, not $KIND" >&2; exit 1 ;;
esac
echo "islands helper: $KIND build from $BIN"
HELPER="$BUNDLE/Contents/Helpers/FloatingIslands.app"
rm -rf "$HELPER"
mkdir -p "$HELPER/Contents/MacOS" "$BUNDLE/Contents/MacOS"
cp "$BIN/FloatingIslands" "$HELPER/Contents/MacOS/FloatingIslands"
cp "$BIN/islands" "$BUNDLE/Contents/MacOS/islands"
cat > "$HELPER/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>$HELPER_ID</string>
<key>CFBundleName</key><string>Floating Islands</string>
<key>CFBundleExecutable</key><string>FloatingIslands</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>LSUIElement</key><true/>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
plutil -lint "$HELPER/Contents/Info.plist"
# The helper is its own bundle (its identifier matches its Info.plist); the CLI is plain nested code like
# heimdall-speakers-worker. scripts/verify_release_candidate.py checks both.
codesign --force --sign "$IDENTITY" --identifier "$HELPER_ID" "$HELPER"
codesign --verify --strict "$HELPER"
codesign --force --sign "$IDENTITY" --identifier "$APP_ID" "$BUNDLE/Contents/MacOS/islands"
codesign --verify --strict "$BUNDLE/Contents/MacOS/islands"

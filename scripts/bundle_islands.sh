#!/bin/bash
# Floating Islands (night 1022): build crates/islands and put the helper at Contents/Helpers/FloatingIslands.app and
# the `islands` CLI at Contents/MacOS/islands inside the given Heimdall.app, each signed with the given identity, inner
# code first. scripts/bundle.sh calls it before it signs the outer bundle; never installs or launches anything.
# Usage: scripts/bundle_islands.sh <Heimdall.app> <signing identity> [app bundle id]
# The helper's ID follows the app's: dev.heimdall.floatingislands for the personal app, <app id>.floatingislands else.
# Built without the `lab` feature: the lab harnesses (`--selftest`, `--render`, …) stay out of the shipped helper.
set -euo pipefail
[[ $# -eq 2 || $# -eq 3 ]] || { echo 'Usage: scripts/bundle_islands.sh <Heimdall.app> <identity> [app id]' >&2; exit 1; }
BUNDLE="$1"
IDENTITY="$2"
APP_ID="${3:-dev.heimdall.app}"
if [[ "$APP_ID" == dev.heimdall.app ]]; then HELPER_ID=dev.heimdall.floatingislands; else HELPER_ID="$APP_ID.floatingislands"; fi
cd "$(dirname "$0")/.."
# scripts/bundle.sh builds both binaries in its one cargo call and names the folder (ISLANDS_BIN); a separate build
# here would resolve features alone and recompile the shared dependencies.
if [[ -n "${ISLANDS_BIN:-}" ]]; then BIN="$ISLANDS_BIN"
else
  export PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
  CC="${CC:-/usr/bin/clang}" nice -n 19 cargo build --release -p islands --bins -j "${CARGO_BUILD_JOBS:-2}"
  BIN="$CARGO_TARGET_DIR/release"
fi
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

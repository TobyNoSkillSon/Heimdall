#!/bin/zsh
# Build build/FloatingIslands.app (agent app: no Dock icon, no menu bar) and build/bin/islands.
set -euo pipefail
cd "${0:A:h}/.."
if ! LOG=$(nice -n 19 swift build -c release -j 2 2>&1); then print -r -- "$LOG" | grep -E "error" | head -20; exit 1; fi
BIN=$(swift build -c release --show-bin-path)
APP=build/FloatingIslands.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" build/bin
cp "$BIN/FloatingIslands" "$APP/Contents/MacOS/FloatingIslands"
cp "$BIN/islands" build/bin/islands
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.vault.floatingislands</string>
  <key>CFBundleName</key><string>Floating Islands</string>
  <key>CFBundleExecutable</key><string>FloatingIslands</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign -s - --force "$APP" >/dev/null 2>&1 || true
echo "built $APP and build/bin/islands"

#!/bin/bash
# The built-in engine (docs/splash-native.md): Splash's native engine, pinned by scripts/splash-engine.lock.json, and
# heimdall-splash, the Rust server that drives it. No Python ships.
#
#   scripts/bundle_splash.sh <Heimdall.app> <signing identity> <heimdall-splash binary> [helper id prefix]
#
# The prefix is the edition's (scripts/bundle.sh HELPER_ID): dev.heimdall for the personal app, the public app's own ID
# for the public one.
#
# The engine files come from a verified cache (~/Library/Caches/Heimdall/splash-engine/<version>), filled from the
# local Homebrew Splash of the same version when its files match the pins, else from the pinned release tarball.
set -euo pipefail
APP=$1 IDENTITY=$2 SERVER=$3 HELPER_ID=${4:-dev.heimdall}
cd "$(dirname "$0")/.."
LOCK=scripts/splash-engine.lock.json
VERSION=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["version"])' "$LOCK")
CACHE="$HOME/Library/Caches/Heimdall/splash-engine/$VERSION"
# Every pinned file under $1 matches its hash.
verify() {
  python3 - "$LOCK" "$1" <<'PY'
import hashlib, json, sys
from pathlib import Path
lock, root = json.load(open(sys.argv[1])), Path(sys.argv[2])
for name, digest in lock["files"].items():
    path = root / name
    if not path.is_file() or hashlib.file_digest(path.open("rb"), "sha256").hexdigest() != digest:
        sys.exit(f"{path}: missing or not the pinned file")
PY
}
fill() {
  local stage
  stage="$(mktemp -d "${TMPDIR:-/tmp}/splash-engine.XXXXXX")"
  local cellar="/opt/homebrew/Cellar/splash/$VERSION"
  if [[ -d "$cellar/libexec/engine" ]]; then
    mkdir -p "$stage/engine"
    cp "$cellar/libexec/engine/"{splash,splash.metallib,host-step.metallib} "$stage/engine/"
    cp "$cellar/libexec/THIRD_PARTY_NOTICES" "$stage/"
    cp "$cellar/LICENSE" "$stage/" 2>/dev/null || cp "$cellar/libexec/LICENSE" "$stage/"
  fi
  if ! verify "$stage" 2>/dev/null; then
    rm -rf "$stage"; stage="$(mktemp -d "${TMPDIR:-/tmp}/splash-engine.XXXXXX")"
    local url sha
    url=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["url"])' "$LOCK")
    sha=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["tarball_sha256"])' "$LOCK")
    curl --fail --location --silent --show-error -o "$stage/release.tar.gz" "$url"
    echo "$sha  $stage/release.tar.gz" | shasum -a 256 -c - >/dev/null
    # Homebrew strips the release's one top directory, if it has one; so does this.
    mkdir "$stage/x" && tar -xzf "$stage/release.tar.gz" -C "$stage/x" && rm "$stage/release.tar.gz"
    local top="$stage/x"
    [[ -d "$top/engine" ]] || top="$(dirname "$(find "$stage/x" -maxdepth 3 -type d -name engine | head -1)")"
    mkdir -p "$stage/engine"
    mv "$top/engine/"{splash,splash.metallib,host-step.metallib} "$stage/engine/"
    mv "$top/THIRD_PARTY_NOTICES" "$top/LICENSE" "$stage/"
    rm -rf "$stage/x"
  fi
  verify "$stage"
  mkdir -p "$(dirname "$CACHE")"
  rm -rf "$CACHE"
  mv "$stage" "$CACHE"
}
verify "$CACHE" 2>/dev/null || fill
ENGINE="$APP/Contents/Resources/splash"
mkdir -p "$ENGINE/engine" "$APP/Contents/Resources/licenses"
cp "$CACHE/engine/"{splash,splash.metallib,host-step.metallib} "$ENGINE/engine/"
cp "$CACHE/LICENSE" "$APP/Contents/Resources/licenses/splash-LICENSE.txt"
cp "$CACHE/THIRD_PARTY_NOTICES" "$APP/Contents/Resources/licenses/splash-THIRD_PARTY_NOTICES.txt"
cp crates/splash/NOTICE "$APP/Contents/Resources/licenses/splash-NOTICE.txt"
cp "$SERVER" "$APP/Contents/MacOS/heimdall-splash"
# Release engines are ad-hoc signed; the app's identity signs both so the bundle verifies strictly.
codesign --force --sign "$IDENTITY" --identifier "$HELPER_ID.splash-engine" "$ENGINE/engine/splash"
codesign --force --sign "$IDENTITY" --identifier "$HELPER_ID.splash" "$APP/Contents/MacOS/heimdall-splash"
codesign --verify --strict "$ENGINE/engine/splash"
codesign --verify --strict "$APP/Contents/MacOS/heimdall-splash"

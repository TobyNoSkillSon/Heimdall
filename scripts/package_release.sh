#!/bin/bash
# Local release staging only: zips, checksums and signs a built Heimdall Alpha. Uploads, publishes and installs nothing.
#
#   HEIMDALL_UPDATE_SIGNING_KEY=<secret key file> scripts/package_release.sh ["dist/public/Heimdall Alpha.app"]
#
# Output (HEIMDALL_RELEASE_OUTPUT_DIR, default dist/releases/<version>): Heimdall-Alpha-<version>-arm64.zip,
# SHA256SUMS and SHA256SUMS.sig (Ed25519 over SHA256SUMS; docs/updates.md). These three files are the release's
# assets, under the tag v<version>. The signing key is read from the given path and never copied; the signature is
# checked against the public key compiled into this build's updater, so a release the app would refuse is never staged.
# Prints the zip's SHA-256 for the cask (packaging/homebrew/heimdall-alpha.rb).
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH" CC=/usr/bin/clang CXX=/usr/bin/clang++
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target/release-candidate}"
APP="${1:-dist/public/Heimdall Alpha.app}"
KEY="${HEIMDALL_UPDATE_SIGNING_KEY:-}"
[[ -n "$KEY" && -f "$KEY" ]] || { echo 'Set HEIMDALL_UPDATE_SIGNING_KEY to the release signing key file; nothing staged.' >&2; exit 1; }
[[ "$(stat -f '%Lp' "$KEY")" == 600 ]] || { echo "Signing key $KEY must be mode 600; nothing staged." >&2; exit 1; }
[[ -d "$APP" && "$(basename "$APP")" == "Heimdall Alpha.app" ]] || { echo "No Heimdall Alpha.app at $APP" >&2; exit 1; }
plist() { /usr/libexec/PlistBuddy -c "Print :$1" "$APP/Contents/Info.plist"; }
VERSION="$(plist CFBundleShortVersionString)"
WORKSPACE="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
[[ "$VERSION" == "$WORKSPACE" ]] || { echo "App version $VERSION differs from the workspace's $WORKSPACE" >&2; exit 1; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "Invalid version: $VERSION" >&2; exit 1; }
[[ "$(plist CFBundleIdentifier)" == com.tobynoskillson.heimdall ]] || { echo 'Not the public edition (bundle ID)' >&2; exit 1; }
[[ "$(plist HeimdallEdition)" == public ]] || { echo 'Not the public edition' >&2; exit 1; }
# A test fixture (scripts/bundle.sh --test-fixture: ad-hoc, no models) is never a release (VM finding P-05).
! plist HeimdallTestFixture >/dev/null 2>&1 || { echo 'This is a test fixture (ad-hoc signed, no models), not a release; nothing staged.' >&2; exit 1; }
SIGNATURE="$(codesign -dvv "$APP" 2>&1 || true)"   # captured first: grep -q under pipefail could SIGPIPE codesign
[[ $'\n'"$SIGNATURE" == *$'\n'Signature=adhoc* ]] && { echo 'Ad-hoc signed: a release needs the public signing certificate; nothing staged.' >&2; exit 1; }
[[ "$(python3 -c 'import json,sys;print(len(json.load(open(sys.argv[1]))))' "$APP/Contents/Resources/models/manifest.json" 2>/dev/null || echo 0)" -gt 0 ]] || { echo 'No bundled models: a release carries the pinned public model assets; nothing staged.' >&2; exit 1; }
for f in MacOS/Heimdall MacOS/heimdall-speakers-worker MacOS/heimdall-clip MacOS/heimdall-emergency Helpers/heimdall-install-tool; do
  [[ -x "$APP/Contents/$f" ]] || { echo "Bundle lacks Contents/$f" >&2; exit 1; }
done
codesign --verify --deep --strict "$APP"
OUT="${HEIMDALL_RELEASE_OUTPUT_DIR:-$PWD/dist/releases/$VERSION}"
[[ ! -e "$OUT" ]] || { echo "Output already exists; preserving it: $OUT" >&2; exit 1; }
cargo build --release -p heimdall-update --bin heimdall-update-sign
SIGNER="$CARGO_TARGET_DIR/release/heimdall-update-sign"
mkdir -p dist
STAGE="$(mktemp -d "$PWD/dist/.package-stage.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
ZIP="Heimdall-Alpha-$VERSION-arm64.zip"
ditto -c -k --norsrc --noextattr --noqtn --keepParent "$APP" "$STAGE/$ZIP"
# The updater's archive rule (crates/update/src/release.rs): only Heimdall Alpha.app/ entries, no .., no backslash,
# no AppleDouble or __MACOSX.
zipinfo -1 "$STAGE/$ZIP" | awk '
  !/^Heimdall Alpha\.app(\/|$)/ || /\\/ || /(^|\/)\.\.(\/|$)/ || /(^|\/)\._/ || /(^|\/)__MACOSX(\/|$)/ { bad = 1; print "unsafe entry: " $0 > "/dev/stderr" }
  END { exit bad }'
(cd "$STAGE" && shasum -a 256 "$ZIP") > "$STAGE/SHA256SUMS"
"$SIGNER" sign --key "$KEY" "$STAGE/SHA256SUMS"
"$SIGNER" verify "$STAGE/SHA256SUMS" "$STAGE/SHA256SUMS.sig"
SHA="$(cut -d' ' -f1 "$STAGE/SHA256SUMS")"
mkdir -p "$(dirname "$OUT")"
mkdir "$OUT"
mv "$STAGE/$ZIP" "$STAGE/SHA256SUMS" "$STAGE/SHA256SUMS.sig" "$OUT/"
echo "Packaged locally (not published or installed): $OUT"
echo "Release tag: v$VERSION; assets: $ZIP, SHA256SUMS, SHA256SUMS.sig"
echo "Cask lines: version \"$VERSION\" and sha256 \"$SHA\""

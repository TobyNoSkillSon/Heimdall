#!/bin/bash
# Build/sign only. Never installs, launches, resets TCC or requests permission.
#
#   scripts/bundle.sh [--edition public|personal|both] [--probe]
#
# Editions (docs/editions.md): public, the default, is the clean build for other people: dist/public/Heimdall.app,
# Phonon S as its only voice, no Drama 3, Supertonic, Jarvis or the user's tuned Smart Turn, and no Python.
# personal is the user's install, built by the night pass (scripts/check.sh): dist/Heimdall.app, as before.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
export CC=/usr/bin/clang CXX=/usr/bin/clang++
# No build-machine paths in shipped code (VM finding P-03, 28 Oct): Rust panic locations and C/C++ __FILE__ strings
# name the source and the cargo registry under the builder's home folder; both are rewritten to /build.
# scripts/check_public_bundle.py refuses a public bundle that still names the builder's home.
# The checkout itself becomes /build/heimdall (its folder names are the builder's too); the most specific map is
# given last and wins in rustc; clang picks the longest matching prefix, whatever the order (both checked, 28 Oct).
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$HOME=/build --remap-path-prefix=$PWD=/build/heimdall"
export CFLAGS="${CFLAGS:+$CFLAGS }-ffile-prefix-map=$PWD=/build/heimdall -ffile-prefix-map=$HOME=/build"
export CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-ffile-prefix-map=$PWD=/build/heimdall -ffile-prefix-map=$HOME=/build"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target/release-candidate}"
# The signing identity comes from the build machine, never from this script: HEIMDALL_SIGN_IDENTITY, else this
# repository's git config `heimdall.signingIdentity` (`git config heimdall.signingIdentity <SHA-1>`), as the SHA-1
# of one valid code-signing identity in the login keychain. Losing or rotating it is an explicit identity change; no
# ad-hoc fallback. The public edition may use its own certificate: HEIMDALL_PUBLIC_SIGN_IDENTITY or
# `heimdall.publicSigningIdentity`, else the same one.
signing_identity() {
  local id="${!1:-$(git config --get "$2" || true)}"
  [[ "$id" =~ ^[0-9A-Fa-f]{40}$ ]] || { echo "No signing identity: set $1 or git config $2 to a certificate's SHA-1; bundle unchanged." >&2; return 1; }
  security find-identity -v -p codesigning | grep -qiF "$id" || { echo "Signing identity $id is not a valid code-signing identity on this Mac; bundle unchanged." >&2; return 1; }
  printf '%s' "$id"
}
MODE="" EDITION="" STAGE="" FIXTURE="" ADHOC=""
usage() { echo 'Usage: scripts/bundle.sh [--edition public|personal|both] [--probe] | --test-fixture | --adhoc' >&2; exit 1; }
while [[ $# -gt 0 ]]; do
  case "$1" in
    --probe) MODE=--probe; shift ;;
    --edition) EDITION="${2:-}"; shift 2 || usage ;;
    # A TEST FIXTURE, never a release (VM finding P-05, 28 Oct): the public edition, ad-hoc signed, with NO models,
    # into dist/test-fixture/. It needs no signing certificate and no models folder, so a clean public export can
    # build what the VM tests install. Info.plist says HeimdallTestFixture; package_release.sh refuses it.
    --test-fixture) FIXTURE=1; shift ;;
    # The public alpha release (4 Nov): the public edition with its models, ad-hoc signed, so no certificate and no
    # developer email is in the signature. macOS treats it as unsigned: right-click > Open the first time, and
    # permissions may need granting again after an update.
    --adhoc) ADHOC=1; shift ;;
    *) usage ;;
  esac
done
if [[ -n "$FIXTURE" ]]; then
  [[ -z "$MODE" && "${EDITION:-public}" == public ]] || { echo '--test-fixture builds the public edition only.' >&2; exit 1; }
  EDITION=public PERSONAL_IDENTITY=- PUBLIC_IDENTITY=-
elif [[ -n "$ADHOC" ]]; then
  [[ -z "$MODE" && "${EDITION:-public}" == public ]] || { echo '--adhoc builds the public edition only.' >&2; exit 1; }
  EDITION=public PERSONAL_IDENTITY=- PUBLIC_IDENTITY=-
else
PERSONAL_IDENTITY="$(signing_identity HEIMDALL_SIGN_IDENTITY heimdall.signingIdentity)" || exit 1
PUBLIC_IDENTITY="$(HEIMDALL_PUBLIC_SIGN_IDENTITY="${HEIMDALL_PUBLIC_SIGN_IDENTITY:-$(git config --get heimdall.publicSigningIdentity || printf '%s' "$PERSONAL_IDENTITY")}" \
  signing_identity HEIMDALL_PUBLIC_SIGN_IDENTITY heimdall.publicSigningIdentity)" || exit 1
fi
# One version for both editions: the workspace's (Cargo.toml [workspace.package]).
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
[[ -n "$VERSION" ]] || { echo 'No workspace version in Cargo.toml.' >&2; exit 1; }
# The capture probe replaces the user's app for a TCC test (docs/capture.md): personal unless asked otherwise.
[[ -n "$EDITION" ]] || { [[ "$MODE" == --probe ]] && EDITION=personal || EDITION=public; }
case "$EDITION" in public) EDITIONS=(public) ;; personal) EDITIONS=(personal) ;; both) EDITIONS=(personal public) ;; *) usage ;; esac
if [[ -n "$(git status --porcelain -- crates/ui)" ]]; then
  echo "UI worker has uncommitted changes; candidate build deferred." >&2
  exit 1
fi
git diff --quiet -- crates scripts islands skills docs Cargo.toml Cargo.lock && git diff --cached --quiet -- crates scripts islands skills docs Cargo.toml Cargo.lock || { echo "Commit runtime sources before building a candidate." >&2; exit 1; }
SOURCE_COMMIT="$(git rev-parse HEAD)"
# One cargo call for every shipped binary except Splash, and only the binaries the bundle ships (lab bins stay with the
# test build): separate calls resolved features per call and recompiled agent, archive and emergency (cleanup 14 Nov).
# The public app binary follows with its feature (crates/app/src/edition.rs). Each app binary is copied out at once:
# both build to the same path. Splash builds alone: its serde_json preserve_order must not reach Heimdall's JSON.
SHIPPED=(-p speakers --bin heimdall-speakers-worker -p agent --bin heimdall-agent-runtime -p heimdall-archive --bin heimdall-clip
  -p heimdall-emergency --bin heimdall-emergency -p heimdall-update --bin heimdall-install-tool)   # install-tool: docs/updates.md
[[ -n "$FIXTURE" ]] || SHIPPED+=(-p app --bin heimdall)
[[ "$MODE" != --probe ]] || SHIPPED+=(-p capture --bin heimdall-capture-probe)
# Floating Islands (scripts/bundle_islands.sh packages what this call builds).
SHIPPED+=(-p islands --bin FloatingIslands --bin islands); export ISLANDS_BIN="$CARGO_TARGET_DIR/release"
cargo build --release "${SHIPPED[@]}"
cargo build --release -p splash --bin heimdall-splash
mkdir -p dist
[[ -e dist/.gitignore ]] || printf '*\n' > dist/.gitignore
BINARIES="$(mktemp -d "$PWD/dist/.heimdall-binaries.XXXXXX")"
trap 'rm -rf "$BINARIES"' EXIT
[[ -n "$FIXTURE" ]] || cp "$CARGO_TARGET_DIR/release/heimdall" "$BINARIES/heimdall-personal"
# Approval grants are the personal edition's (docs/harness-autonomy.md): each edition's CLI says which it is.
cp "$CARGO_TARGET_DIR/release/heimdall-emergency" "$BINARIES/heimdall-emergency-personal"
if [[ " ${EDITIONS[*]} " == *" public "* && "$MODE" != --probe ]]; then
  cargo build --release -p app --bin heimdall --no-default-features --features app/public
  cp "$CARGO_TARGET_DIR/release/heimdall" "$BINARIES/heimdall-public"
  cargo build --release -p heimdall-emergency --bin heimdall-emergency --features heimdall-emergency/public
  cp "$CARGO_TARGET_DIR/release/heimdall-emergency" "$BINARIES/heimdall-emergency-public"
fi
[[ -z "$(git status --porcelain -- crates/ui)" && "$(git rev-parse HEAD)" == "$SOURCE_COMMIT" ]] && git diff --quiet -- crates scripts islands skills docs Cargo.toml Cargo.lock && git diff --cached --quiet -- crates scripts islands skills docs Cargo.toml Cargo.lock || { echo "Source changed during build; candidate deferred." >&2; exit 1; }
bundle_edition() {
EDITION=$1
# The personal app keeps its ID, name and permission grants; the public one has its own (docs/editions.md).
if [[ "$EDITION" == public ]]; then
  APP="$PWD/dist/$([[ -n "$FIXTURE" ]] && echo test-fixture || echo public)/Heimdall Alpha.app" BUNDLE_ID=com.tobynoskillson.heimdall APP_NAME="Heimdall Alpha" IDENTITY="$PUBLIC_IDENTITY" HELPER_ID=com.tobynoskillson.heimdall
else
  APP="$PWD/dist/Heimdall.app" BUNDLE_ID=dev.heimdall.app APP_NAME=Heimdall IDENTITY="$PERSONAL_IDENTITY" HELPER_ID=dev.heimdall
fi
mkdir -p "$(dirname "$APP")"
STAGE="$(mktemp -d "$PWD/dist/.heimdall-bundle.XXXXXX")"
trap 'rm -rf "$BINARIES" "$STAGE"' EXIT
BUNDLE="$STAGE/Heimdall.app"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
if [[ "$MODE" == --probe ]]; then
  cp "$CARGO_TARGET_DIR/release/heimdall-capture-probe" "$BUNDLE/Contents/MacOS/Heimdall"
else
  cp "$BINARIES/heimdall-$EDITION" "$BUNDLE/Contents/MacOS/Heimdall"
  cp "$CARGO_TARGET_DIR/release/heimdall-speakers-worker" "$BUNDLE/Contents/MacOS/heimdall-speakers-worker"
  codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$BUNDLE/Contents/MacOS/heimdall-speakers-worker"
  codesign --verify --strict "$BUNDLE/Contents/MacOS/heimdall-speakers-worker"
  # Archive clips for tasks (docs/archive-clip.md): a stable path inside the installed app.
  cp "$CARGO_TARGET_DIR/release/heimdall-clip" "$BUNDLE/Contents/MacOS/heimdall-clip"
  codesign --force --sign "$IDENTITY" --identifier "$HELPER_ID.clip" "$BUNDLE/Contents/MacOS/heimdall-clip"
  codesign --verify --strict "$BUNDLE/Contents/MacOS/heimdall-clip"
  # Emergency mode (docs/emergency.md): the cover agent's CLI beside Heimdall's binary; the protocol it reads.
  cp "$BINARIES/heimdall-emergency-$EDITION" "$BUNDLE/Contents/MacOS/heimdall-emergency"
  codesign --force --sign "$IDENTITY" --identifier "$HELPER_ID.emergency" "$BUNDLE/Contents/MacOS/heimdall-emergency"
  codesign --verify --strict "$BUNDLE/Contents/MacOS/heimdall-emergency"
  if [[ "$EDITION" == public ]]; then
    # The public relay reads its protocol without bug notes (docs/personal/bugs.md) and without the owner's name.
    sed -e '/[Bb]ug note/d' -e "s/Toby's/the user's/g" -e 's/Toby/the user/g' docs/emergency.md > "$BUNDLE/Contents/Resources/emergency.md"
  else
    cp docs/emergency.md "$BUNDLE/Contents/Resources/emergency.md"
  fi
  # The built-in engine (docs/inference-engine.md): heimdall-splash and Splash's native engine with its notices.
  scripts/bundle_splash.sh "$BUNDLE" "$IDENTITY" "$CARGO_TARGET_DIR/release/heimdall-splash" "$HELPER_ID"
  # The in-app updater's install tool (docs/updates.md), public edition only: the personal app installs through the
  # night pass.
  if [[ "$EDITION" == public ]]; then
    mkdir -p "$BUNDLE/Contents/Helpers"
    cp "$CARGO_TARGET_DIR/release/heimdall-install-tool" "$BUNDLE/Contents/Helpers/heimdall-install-tool"
    codesign --force --sign "$IDENTITY" --identifier "$HELPER_ID.install-tool" "$BUNDLE/Contents/Helpers/heimdall-install-tool"
    codesign --verify --strict "$BUNDLE/Contents/Helpers/heimdall-install-tool"
  fi
fi
cat > "$BUNDLE/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>BUNDLE_ID</string>
<key>CFBundleName</key><string>APP_NAME</string>
<key>CFBundleDisplayName</key><string>APP_NAME</string>
<key>CFBundleExecutable</key><string>Heimdall</string>
<key>CFBundleIconFile</key><string>Heimdall</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>VERSION</string>
<key>CFBundleVersion</key><string>1</string>
<key>HeimdallEdition</key><string>EDITION</string>
<key>LSMinimumSystemVersion</key><string>14.4</string>
<key>LSUIElement</key><true/>
<key>NSMicrophoneUsageDescription</key><string>Heimdall listens to your microphones and keeps a local audio archive.</string>
<key>NSAudioCaptureUsageDescription</key><string>Heimdall listens to each app's audio separately for local transcription and archiving.</string>
<key>NSScreenCaptureUsageDescription</key><string>Heimdall keeps a short, local screen buffer to attach the view you refer to in voice commands.</string>
<key>NSAccessibilityUsageDescription</key><string>Heimdall reads Discord names and speaking indicators to help identify voices.</string>
</dict></plist>
PLIST
sed -i '' -e "s/<string>EDITION<\/string>/<string>$EDITION<\/string>/" -e "s/<string>BUNDLE_ID<\/string>/<string>$BUNDLE_ID<\/string>/" \
  -e "s/<string>APP_NAME<\/string>/<string>$APP_NAME<\/string>/" -e "s/<string>VERSION<\/string>/<string>$VERSION<\/string>/" "$BUNDLE/Contents/Info.plist"
[[ -z "$FIXTURE" ]] || /usr/libexec/PlistBuddy -c 'Add :HeimdallTestFixture string ad-hoc signed, no models; not a release' "$BUNDLE/Contents/Info.plist"
# The app icon, the white helmet on a graphite tile (Finder, Spotlight, alerts; masters in assets/icon/). Rendered by `heimdall-ui-demo --offscreen <dir> --icns <file>`.
cp crates/app/config/inference-engine.json "$BUNDLE/Contents/Resources/inference-engine.json"
cp crates/ui/assets/Heimdall.icns "$BUNDLE/Contents/Resources/Heimdall.icns"
# Heimdall's docs as committed at this build's source (docs/README.md indexes them), so the installed agent reads its own
# build's: docs/ with the crate READMEs and islands/ docs beside it at their repository paths
# (Resources/docs/crates/agent/README.md). The general prompt names the folder in both editions (`docs_dir` in
# crates/agent/src/harness.rs), so both carry it. The public edition leaves out the personal pages, history and drafts
# (docs/personal, docs/archive, docs/briefs, docs/drafts), which its tree does not have anyway.
DOCS_TREE="$STAGE/docs-tree"
mkdir -p "$DOCS_TREE"
DOCS_LEFT_OUT=()
if [[ "$EDITION" == public ]]; then
  DOCS_LEFT_OUT=(':(exclude)docs/personal' ':(exclude)docs/archive' ':(exclude)docs/briefs' ':(exclude)docs/drafts')
fi
git archive --format=tar "$SOURCE_COMMIT" docs ':(glob)crates/**/README.md' ':(exclude,glob)crates/**/vendor/**' \
  islands/README.md islands/docs ${DOCS_LEFT_OUT[@]+"${DOCS_LEFT_OUT[@]}"} | tar -x -C "$DOCS_TREE"   # empty list safe under set -u (bash 3.2)
mv "$DOCS_TREE/docs" "$BUNDLE/Contents/Resources/docs"
mv "$DOCS_TREE/crates" "$DOCS_TREE/islands" "$BUNDLE/Contents/Resources/docs/"
[[ -f "$BUNDLE/Contents/Resources/docs/README.md" && -f "$BUNDLE/Contents/Resources/docs/connectors.md" \
  && -f "$BUNDLE/Contents/Resources/docs/crates/agent/README.md" ]] || { echo "Bundled docs are incomplete." >&2; exit 1; }
# Heimdall's bundled skills (docs/skills.md, "Two layers"): the general sets, the same in every edition, copied once
# into a fresh profile's skill folder (`crates/agent/src/skills.rs`). A user's own skills are never bundled: they live
# in their data folder. The split check refuses a set or a skill that is not public.
python3 -I scripts/check_public_split.py
mkdir -p "$BUNDLE/Contents/Resources/skills"
for set in public shared; do cp -R "skills/$set" "$BUNDLE/Contents/Resources/skills/$set"; done
cp crates/capture/vendor/flexaudio/LICENSE "$BUNDLE/Contents/Resources/flexaudio-LICENSE.txt"
cp crates/capture/vendor/flexaudio/NOTICE.md "$BUNDLE/Contents/Resources/flexaudio-NOTICE.md"
if [[ "$MODE" != --probe ]]; then
  mkdir -p "$BUNDLE/Contents/Resources/licenses"
  # Heimdall's own licence and the third-party credits (NOTICE, THIRD_PARTY_NOTICES.md, full texts in licenses/).
  cp LICENSE NOTICE THIRD_PARTY_NOTICES.md "$BUNDLE/Contents/Resources/"
  cp licenses/* "$BUNDLE/Contents/Resources/licenses/"
  cp crates/speakers/vendor/nemo-speech/LICENSE "$BUNDLE/Contents/Resources/licenses/nemo-LICENSE.txt"
  cp crates/speakers/vendor/nemo-speech/NOTICE "$BUNDLE/Contents/Resources/licenses/nemo-NOTICE.txt"
  cp crates/speakers/vendor/nemo-speech/THIRD_PARTY_NOTICES.md "$BUNDLE/Contents/Resources/licenses/nemo-third-party.md"
  cp crates/speakers/vendor/ggml/LICENSE "$BUNDLE/Contents/Resources/licenses/ggml-LICENSE.txt"
  cp crates/speakers/vendor/ReDimNet-LICENSE "$BUNDLE/Contents/Resources/licenses/ReDimNet-LICENSE.txt"
  cp crates/speakers/vendor/opus/COPYING "$BUNDLE/Contents/Resources/licenses/opus-LICENSE.txt"
  cp crates/listen/src/metal/Vella-LICENSE "$BUNDLE/Contents/Resources/licenses/Vella-LICENSE.txt"
  cp crates/listen/THIRD_PARTY.md "$BUNDLE/Contents/Resources/licenses/listen-third-party.md"
  # Sign nested Mach-O code (model dylibs, the agent runtime's node) with the stable app identity; prints each signed
  # path, NUL-terminated. Source hashes precede signing; sealed hashes follow it because codesign changes Mach-O bytes.
  sign_macho() {
    local library
    while IFS= read -r -d '' library; do
      codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$library" >&2
      codesign --verify --strict "$library" >&2
      printf '%s\0' "$library"
    done < <(python3 - "$@" <<'PYCODE'
from pathlib import Path
import sys
for file in (f for root in sys.argv[1:] for f in Path(root).rglob('*')):
    if file.is_file():
        with file.open('rb') as f: magic=f.read(4)
        if magic in [b'\xcf\xfa\xed\xfe',b'\xfe\xed\xfa\xcf',b'\xca\xfe\xba\xbe',b'\xbe\xba\xfe\xca',b'\xca\xfe\xba\xbf',b'\xbf\xba\xfe\xca']:
            sys.stdout.buffer.write(str(file).encode()+b'\0')
PYCODE
    )
  }
  # The agent runtime ships complete (crates/agent/src/bin/heimdall-agent-runtime.rs): pinned Node and Pi, read-only
  # at run time. Built once into target/agent-runtime from the committed pins; rebuilt offline when Heimdall's own
  # runtime files change. Signed, sealed and verified here, before the models go in.
  RUNTIME_TOOL="$CARGO_TARGET_DIR/release/heimdall-agent-runtime"
  RUNTIME="$BUNDLE/Contents/Resources/agent-runtime"
  AGENT_RUNTIME="$("$RUNTIME_TOOL" build "$PWD/target/agent-runtime")"
  ditto "$AGENT_RUNTIME" "$RUNTIME"
  sign_macho "$RUNTIME" >/dev/null
  "$RUNTIME_TOOL" seal "$RUNTIME"
  # Verify runs Node under the process guardian, which disclaims responsibility (crates/agent/runtime/tool-guardian.c),
  # so the first launch of a fresh bundle's node makes macOS scan the whole bundle (Gatekeeper and XProtect): 23 s with
  # the models in, over verify's 10 s deadline, so the first verify of every build failed (31 Oct to 13 Nov). A plain
  # `node --version` never triggered the scan. Launch node the same way once, unbounded, while the bundle is still
  # small: 1.5 s, and adding the models later does not repeat it (measured 13 Nov, lab notes in
  # Assistant/Research/cleanup-night-2026-11-14/build-timings/notes.md).
  /usr/bin/time -p python3 - "$RUNTIME/node/bin/node" --version >/dev/null <<'PYDISCLAIM'
import ctypes, os, sys
libc = ctypes.CDLL(None)
attr = ctypes.c_void_p()
assert libc.posix_spawnattr_init(ctypes.byref(attr)) == 0
disclaim = getattr(libc, 'responsibility_spawnattrs_setdisclaim', None)   # the guardian runs without it too
assert disclaim is None or disclaim(ctypes.byref(attr), 1) == 0
args = [a.encode() for a in sys.argv[1:]]
env = [f'{k}={v}'.encode() for k, v in os.environ.items()]
pid = ctypes.c_int()
assert libc.posix_spawn(ctypes.byref(pid), args[0], None, ctypes.byref(attr), (ctypes.c_char_p * (len(args) + 1))(*args, None),
                        (ctypes.c_char_p * (len(env) + 1))(*env, None)) == 0
sys.exit(os.waitstatus_to_exitcode(os.waitpid(pid.value, 0)[1]))
PYDISCLAIM
  if ! "$RUNTIME_TOOL" verify "$RUNTIME"; then
    echo "agent runtime verify failed once; retrying" >&2
    "$RUNTIME_TOOL" verify "$RUNTIME" || { echo "agent runtime verify failed twice; kept dist/.verify-failed" >&2; rm -rf dist/.verify-failed; cp -cR "$BUNDLE" dist/.verify-failed; exit 1; }
  fi
  rm -rf dist/.verify-failed   # an earlier build's evidence clone pins a whole bundle (19 GB)
  if [[ -n "$FIXTURE" ]]; then
    # NO MODELS: the fixture carries an empty manifest. Listening, the decider, speakers, memory search and the voice
    # cannot run; a release needs scripts/bundle_models.py's pinned assets.
    mkdir -p "$BUNDLE/Contents/Resources/models" && printf '[]\n' > "$BUNDLE/Contents/Resources/models/manifest.json"
  else
    python3 -B scripts/bundle_models.py --edition "$EDITION" "$BUNDLE/Contents/Resources/models"
  fi
  cp crates/night/third-party/* "$BUNDLE/Contents/Resources/licenses/"
  sign_macho "$BUNDLE/Contents/Resources/models" > "$STAGE/signed-models"
  # Sealed hashes: signing changed only the files it signed; every other file keeps the hash bundle_models.py checked
  # on the staged copy itself. scripts/verify_release_candidate.py re-hashes every row independently.
  python3 - "$BUNDLE/Contents/Resources/models" "$STAGE/signed-models" <<'PYMANIFEST'
import hashlib,json,sys
from pathlib import Path
root=Path(sys.argv[1]);path=root/'manifest.json';rows=json.loads(path.read_text())
signed={Path(p.decode()) for p in Path(sys.argv[2]).read_bytes().split(b'\0') if p}
for row in rows:
    file=root/row['path']
    row['sealed_sha256']=hashlib.file_digest(file.open('rb'),'sha256').hexdigest() if file in signed else row['source_sha256']
    row['bytes']=file.stat().st_size
path.write_text(json.dumps(rows,indent=2)+'\n')
PYMANIFEST
  # Floating Islands (night 1022): the helper app and the islands CLI from crates/islands, signed inner-first here,
  # before the outer bundle below.
  scripts/bundle_islands.sh "$BUNDLE" "$IDENTITY" "$BUNDLE_ID"
  printf '%s\n' "$SOURCE_COMMIT" > "$BUNDLE/Contents/Resources/source-commit.txt"
  # The public edition carries no Python and nothing of the user's: refuse the bundle otherwise.
  if [[ "$EDITION" == public ]]; then python3 scripts/check_public_bundle.py --source-root "$PWD" "$BUNDLE"; fi
fi
plutil -lint "$BUNDLE/Contents/Info.plist"
codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$BUNDLE"
codesign --verify --deep --strict "$BUNDLE"
codesign -d -r- "$BUNDLE" > "$STAGE/new-requirement" 2>&1
sed -n 's/^designated => //p' "$STAGE/new-requirement" > "$STAGE/new-dr"
# An ad-hoc test fixture has no certificate requirement to keep (codesign prints it as a comment).
[[ -s "$STAGE/new-dr" || -n "$FIXTURE" || -n "$ADHOC" ]] || { echo 'No designated requirement; bundle unchanged.' >&2; exit 1; }
INSTALLED="$HOME/Applications/Heimdall.app"
# Only the personal build replaces the installed app; the public one has its own ID and never checks against it.
if [[ "$EDITION" == personal && -d "$INSTALLED" ]]; then
  codesign -d -r- "$INSTALLED" > "$STAGE/installed-requirement" 2>&1
  sed -n 's/^designated => //p' "$STAGE/installed-requirement" > "$STAGE/installed-dr"
  cmp -s "$STAGE/installed-dr" "$STAGE/new-dr" || { echo "Installed designated requirement differs; candidate deferred." >&2; exit 1; }
fi
if [[ -d "$APP" && ( -n "$FIXTURE" || -n "$ADHOC" ) ]]; then
  rm -rf "$APP"   # an ad-hoc signature has no stable designated requirement to keep
elif [[ -d "$APP" ]]; then
  codesign --verify --deep --strict "$APP"
  codesign -d -r- "$APP" > "$STAGE/old-requirement" 2>&1
  sed -n 's/^designated => //p' "$STAGE/old-requirement" > "$STAGE/old-dr"
  if ! cmp -s "$STAGE/old-dr" "$STAGE/new-dr"; then
    echo 'Designated requirement changed; refusing to replace Heimdall.app and its TCC identity.' >&2
    exit 1
  fi
  # dist/Heimdall.app is only the build output; the one rollback is kept at install time
  # (docs/install.md), never per build.
  rm -rf "$APP"
fi
mv "$BUNDLE" "$APP"
codesign -dv "$APP"
printf 'Signed bundle (%s): %s\n' "$EDITION" "$APP"
rm -rf "$STAGE"
}
for edition in "${EDITIONS[@]}"; do bundle_edition "$edition"; done

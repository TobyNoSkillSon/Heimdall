#!/bin/bash
# The VM-only parts of UPD-T01..T22 (docs/updates.md). Runs only inside a macOS VM guest; it installs into the
# guest's /Applications and uses the guest user's profile, Keychain and TCC. Never run it on a real Mac.
#
#   upd_vm.sh <test>...        tests: t02 t05 t11 t14 t15 t16 t20 t21 t22 (or: all)
#
# Inputs (environment):
#   OLD_APP     a signed "Heimdall Alpha.app" (the version installed first)
#   NEW_APP     a signed "Heimdall Alpha.app", newer, same certificate
#   FAIL_APP    t16: a signed newer build whose app exits at launch (or never writes ready.json)
#   OTHER_IDENTITY  t11: SHA-1 of a second code-signing certificate in the guest keychain
#   TOOL        heimdall-install-tool (default: OLD_APP's Contents/Helpers copy)
#   READY_SECONDS   readiness timeout for the tool (default 180)
#   AX_TOOL     the guest AX helper (axtool, built from axtool.swift beside this script on the host): Quit is pressed in
#               the app's own status menu through Accessibility. Apple Events would raise an Automation prompt in a
#               clean guest and hang the run (VM finding T-01, 28 Oct).
# Each test prints PASS or FAIL lines; CHECK lines need a look (logs, prompts). Exit status: failures counted.
set -uo pipefail
[[ "$(sysctl -n kern.hv_vmm_present 2>/dev/null)" == 1 ]] || { echo 'upd_vm.sh runs only in a VM guest; nothing done.' >&2; exit 2; }
: "${OLD_APP:?set OLD_APP}" "${NEW_APP:?set NEW_APP}"
ID=com.tobynoskillson.heimdall
DEST="/Applications/Heimdall Alpha.app"
TXN="/Applications/.heimdall-update"
STATE="$HOME/Library/Application Support/$ID/updates"
DATA="$HOME/.heimdall"
TOOL="${TOOL:-$OLD_APP/Contents/Helpers/heimdall-install-tool}"
export HEIMDALL_UPDATE_READY_SECONDS="${READY_SECONDS:-180}"
FAILS=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; FAILS=$((FAILS + 1)); }
check() { echo "CHECK $*"; }
version() { /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$1/Contents/Info.plist"; }
requirement() { codesign -d -r- "$1" 2>&1 | sed -n 's/^designated => //p'; }
: "${AX_TOOL:?set AX_TOOL to the guest axtool (crates/update/tests/vm/axtool.swift)}"
# The menu's last row: "Quit Heimdall Alpha" for the default name (ui::identity::app_title).
QUIT_TITLE="${QUIT_TITLE:-Quit Heimdall Alpha}"
running_pid() { pgrep -f "^$1/Contents/MacOS/Heimdall\$" | head -1; }
# Quits the copy at $1 (default: the installed app) through its own status menu, as a user would.
quit_app() {
  local app="${1:-$DEST}" pid; pid="$(running_pid "$app")"
  [[ -n "$pid" ]] || return 0
  "$AX_TOOL" menu "$pid" "$QUIT_TITLE" >/dev/null 2>&1
  for _ in $(seq 60); do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; done; return 1
}
# The tool's append-only log: tests read what the tool said during their own run, never a result file the
# relaunched app may already have consumed (VM finding T-16).
LOG="$STATE/update.log"
log_mark() { wc -l < "$LOG" 2>/dev/null | tr -d ' ' || echo 0; }
# The log restarts past 1 MB: a mark beyond its end means everything is new.
log_since() { local n; n="$(log_mark)"; (( $1 > n )) && set -- 0; tail -n +"$(( $1 + 1 ))" "$LOG" 2>/dev/null; }
# Captured first, then matched: grep -q in a pipe under pipefail can SIGPIPE the writer.
said_since() { local text; text="$(log_since "$1")"; [[ "$text" == *"$2"* ]]; }

install_old() {
  quit_app || true
  rm -rf "$DEST" "$TXN"
  ditto "$OLD_APP" "$DEST"
  xattr -dr com.apple.quarantine "$DEST"
  open "$DEST"; sleep 20
}

# Stages APP like the app's prepare step (zip, unpack, quarantine cleared) and writes a plan for the tool.
plan_for() {
  local app=$1 work; work="$(mktemp -d "${TMPDIR:-/tmp}/heimdall-update.XXXXXX")"
  ditto -c -k --norsrc --noextattr --noqtn --keepParent "$app" "$work/r.zip"
  mkdir "$work/unpacked" && ditto -x -k "$work/r.zip" "$work/unpacked"
  xattr -dr com.apple.quarantine "$work/unpacked/Heimdall Alpha.app"
  local pid; pid="$(running_pid "$DEST")"
  cat > "$work/plan.json" <<JSON
{"staged":{"version":"$(version "$app")","directory":"$work","app":"$work/unpacked/Heimdall Alpha.app","sha256":""},
 "destination":"$DEST","from":"$(version "$DEST")","wait_for_pid":${pid:-null},"state_dir":"$STATE","allow_downgrade":false}
JSON
  echo "$work/plan.json"
}

# The app's hand-off: start the tool, quit the app through its own Quit, wait for the tool.
update_to() {
  local plan mark; plan="$(plan_for "$1")"; mark="$(log_mark)"
  "$TOOL" update --plan "$plan" & local tool=$!
  # A refusal after the app exited reopens the installed app, so "still running" here is not a failure by itself;
  # the tool's own words decide (VM finding T-11).
  quit_app || true
  wait "$tool"; local status=$?
  said_since "$mark" "did not quit" && fail "the app did not quit"
  return $status
}

plant() {
  mkdir -p "$DATA/models/UPD-SENTINEL" "$DATA/splash/prefix-cache" "$DATA/agent/skills/upd-custom"
  echo weights > "$DATA/models/UPD-SENTINEL/weights.bin"
  echo cache > "$DATA/splash/prefix-cache/UPD-SENTINEL"
  [[ -f "$DATA/splash/engine.json" ]] || echo '{}' > "$DATA/splash/engine.json"
  echo custom-v1 > "$DATA/agent/skills/upd-custom/SKILL.md"
  (cd "$DATA" && find models/UPD-SENTINEL splash agent/skills/upd-custom settings.json -type f -print0 2>/dev/null | xargs -0 shasum -a 256) > /tmp/upd-sentinels.sha
}
sentinels_same() { (cd "$DATA" && shasum -a 256 -c /tmp/upd-sentinels.sha >/dev/null) && pass "$1: sentinels byte-identical" || fail "$1: a sentinel changed"; }

t02() {
  install_old; plant; update_to "$NEW_APP"
  [[ "$(version "$DEST")" == "$(version "$NEW_APP")" ]] && pass "t02: new version installed" || fail "t02: not installed"
  sentinels_same t02
  check "t02: the new build's log mentions the splash engine config from $DATA/splash/engine.json ($DATA/logs/heimdall.log)"
}

t05() {
  local kc="$HOME/Library/Keychains/upd-sentinel.keychain-db"
  security create-keychain -p upd "$kc" 2>/dev/null; security unlock-keychain -p upd "$kc"
  security list-keychains -d user -s "$kc" $(security list-keychains -d user | tr -d '"')
  security add-generic-password -U -s "Heimdall Alpha API provider: UPD-SENTINEL" -a upd -w sentinel-not-a-secret "$kc"
  install_old; local before; before="$(requirement "$DEST")"
  update_to "$NEW_APP"
  security find-generic-password -s "Heimdall Alpha API provider: UPD-SENTINEL" "$kc" >/dev/null && pass "t05: keychain item exists" || fail "t05: keychain item gone"
  [[ "$(requirement "$DEST")" == "$before" ]] && pass "t05: designated requirement unchanged" || fail "t05: requirement changed"
  check "t05: no Keychain access prompt appeared for the updated app"
}

t11() {
  : "${OTHER_IDENTITY:?t11 needs OTHER_IDENTITY}"
  install_old; local other; other="$(mktemp -d)/Heimdall Alpha.app"
  ditto "$NEW_APP" "$other"; codesign --force --deep --sign "$OTHER_IDENTITY" --identifier "$ID" "$other"
  local before; before="$(shasum -a 256 "$DEST/Contents/MacOS/Heimdall")"
  update_to "$other" && fail "t11: another identity was installed" || pass "t11: another identity refused"
  [[ "$(shasum -a 256 "$DEST/Contents/MacOS/Heimdall")" == "$before" ]] && pass "t11: installed app unchanged" || fail "t11: installed app changed"
  [[ ! -e "$TXN/incoming" && ! -e "$TXN/previous" ]] && pass "t11: nothing staged beside the app" || fail "t11: leftovers in $TXN"
}

t14() {
  # A volume just big enough for the installed copy, then filled. Its own name, so nothing else is on it; the copy the
  # tool reopens after refusing is quit and the volume detached, so t15 never meets a second Heimdall (VM finding T-14b).
  quit_app || true
  local img vol mb; img="$(mktemp -d)/small.dmg"; vol="updsmall-$$"; mb=$(( $(du -sm "$OLD_APP" | cut -f1) + 64 ))
  hdiutil create -size "${mb}m" -fs APFS -volname "$vol" "$img" >/dev/null && hdiutil attach "$img" >/dev/null
  local apps="/Volumes/$vol/Applications" dest="/Volumes/$vol/Applications/Heimdall Alpha.app"
  mkdir -p "$apps"; ditto "$OLD_APP" "$dest" || fail "t14: OLD_APP did not fit on a ${mb} MB volume"
  dd if=/dev/zero of="/Volumes/$vol/fill" bs=1m count=100000 2>/dev/null
  local plan mark; plan="$(plan_for "$NEW_APP")"; mark="$(log_mark)"
  sed -i '' "s#\"destination\":\"$DEST\"#\"destination\":\"$dest\"#; s#\"wait_for_pid\":[0-9a-z]*#\"wait_for_pid\":null#" "$plan"
  "$TOOL" update --plan "$plan" && fail "t14: update succeeded on a full disk" || pass "t14: refused on ENOSPC"
  [[ "$(version "$dest")" == "$(version "$OLD_APP")" ]] && pass "t14: installed app unchanged" || fail "t14: installed app changed"
  [[ ! -e "/Volumes/$vol/Applications/.heimdall-update/incoming/Heimdall Alpha.app" ]] && pass "t14: partial copy removed" || fail "t14: partial copy left"
  said_since "$mark" "could not copy" && pass "t14: the tool says why" || fail "t14: no reason in $LOG"
  sleep 5; quit_app "$dest" || fail "t14: the reopened copy on the test volume did not quit"
  local detached=""
  for _ in 1 2 3 4 5; do hdiutil detach "/Volumes/$vol" >/dev/null 2>&1 && { detached=1; break; }; sleep 3; done
  [[ -n "$detached" ]] && pass "t14: test volume detached" || { fail "t14: test volume busy"; hdiutil detach -force "/Volumes/$vol" >/dev/null 2>&1; }
  rm -f "$img"
}

t15() {
  install_old; plant
  # A real SIGKILL while the tool waits for readiness (after both renames): the next start keeps the new version.
  local plan; plan="$(plan_for "$NEW_APP")"
  "$TOOL" update --plan "$plan" & local tool=$!
  quit_app
  for _ in $(seq 120); do [[ -f "$TXN/journal.json" ]] && grep -q awaiting-ready "$TXN/journal.json" && break; sleep 0.5; done
  kill -9 "$tool"; quit_app || true
  "$TOOL" recover --destination "$DEST" --state "$STATE"
  [[ "$(version "$DEST")" == "$(version "$NEW_APP")" && -d "$TXN/rollback/Heimdall Alpha.app" ]] && pass "t15: rolled forward after SIGKILL" || fail "t15: not recovered forward"
  # The between-renames state (first rename done): recovery restores the previous app.
  mkdir -p "$TXN/previous"; mv "$DEST" "$TXN/previous/"
  printf '{"step":"swapping","from":"%s","to":"x","destination":"%s"}' "$(version "$TXN/previous/Heimdall Alpha.app")" "$DEST" > "$TXN/journal.json"
  "$TOOL" recover --destination "$DEST" --state "$STATE"
  [[ -d "$DEST" && ! -e "$TXN/journal.json" ]] && pass "t15: restored between the renames" || fail "t15: not restored"
  sentinels_same t15
}

t16() {
  : "${FAIL_APP:?t16 needs FAIL_APP}"
  install_old; plant
  local mark start; mark="$(log_mark)"; start=$(date +%s)
  update_to "$FAIL_APP" && fail "t16: a failing build was kept" || pass "t16: failing build refused"
  # Bounded: a build that reports another version fails after the settle time, not the 15-min load timeout (P-09).
  local took=$(( $(date +%s) - start ))
  (( took <= HEIMDALL_UPDATE_READY_SECONDS + 180 )) && pass "t16: rolled back in ${took} s" || fail "t16: rollback took ${took} s"
  [[ "$(version "$DEST")" == "$(version "$OLD_APP")" ]] && pass "t16: previous version restored" || fail "t16: previous not restored"
  sleep 15; [[ -n "$(running_pid "$DEST")" ]] && pass "t16: previous version running" || fail "t16: previous not running"
  said_since "$mark" "did not become ready" && pass "t16: the tool says why" || fail "t16: no reason in $LOG"
  sentinels_same t16
}

t20() {
  install_old; plant; local before; before="$(requirement "$DEST")"
  check "t20: before the update, grant Microphone and Accessibility and turn on Launch at Login in the old app"
  update_to "$NEW_APP" && pass "t20: update reported ready" || fail "t20: update failed: $(cat "$STATE/update-result.json" 2>/dev/null)"
  [[ "$(requirement "$DEST")" == "$before" ]] && pass "t20: designated requirement equal" || fail "t20: requirement changed"
  [[ "$(version "$TXN/rollback/Heimdall Alpha.app")" == "$(version "$OLD_APP")" && ! -e "$TXN/previous" ]] && pass "t20: one rollback kept" || fail "t20: rollback slot wrong"
  sentinels_same t20
  check "t20: no permission prompt at the new version's start; Launch at Login still on"
  quit_app && pass "t20: quit through the app's own Quit" || fail "t20: did not quit"
}

t21() {
  t20 >/dev/null
  open "$DEST"; sleep 20; quit_app
  "$TOOL" rollback --destination "$DEST" --state "$STATE" && pass "t21: rolled back" || fail "t21: rollback refused"
  [[ "$(version "$DEST")" == "$(version "$OLD_APP")" ]] && pass "t21: old version in place" || fail "t21: wrong version"
  open "$DEST"; sleep 20; pgrep -f "$DEST/Contents/MacOS/Heimdall" >/dev/null && pass "t21: old build starts" || fail "t21: old build did not start"
  check "t21: settings keys written by the newer build are still in $DATA/settings.json after the old build saved a setting"
  check "t21: the old build opens the archive or refuses it with a clear message ($DATA/logs/heimdall.log)"
}

t22() {
  install_old
  check "t22: in the old app, turn on Launch at Login and grant Microphone, Screen Recording and Accessibility"
  local before; before="$(sfltool dumpbtm 2>/dev/null | grep -c "$ID")"
  update_to "$NEW_APP"
  [[ "$(sfltool dumpbtm 2>/dev/null | grep -c "$ID")" -ge 1 && "$before" -ge 1 ]] && pass "t22: login item still registered" || check "t22: compare 'sfltool dumpbtm' before and after"
  check "t22: no TCC prompt for a granted permission at the new version's start; no tccutil reset was run"
}

[[ $# -gt 0 ]] || { sed -n 2,14p "$0"; exit 2; }
[[ "$1" == all ]] && set -- t02 t05 t11 t14 t15 t16 t20 t21 t22
for t in "$@"; do "$t"; done
echo "failures: $FAILS"
exit "$FAILS"

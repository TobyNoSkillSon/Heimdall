#!/bin/zsh
# Scenario tests for scripts/install_watchdog.sh. Nothing live: a fake app bundle and repo in a temp dir, and fake
# pgrep/pkill/osascript/open on PATH that only read and write a state file. The real Heimdall is never looked at.
# Each scenario runs the watchdog with a 1 s step and a few seconds of lifetime, in its own folder; the five run at
# once (25 s one after another, about 7 s together).
set -u
setopt NO_BG_NICE   # under check.sh's nice 19, zsh's default renice of background jobs fails ("nice(5) failed")
HERE=$(cd "$(dirname "$0")" && pwd)
WD="$HERE/install_watchdog.sh"
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/heimdall-watchdog-test.XXXXXX")
trap 'rm -rf "$ROOT"' EXIT
failures=0

# Fakes: "running" is the file $ROOT/state/running. The menu Quit (osascript) and pkill remove it; open creates it.
mkdir -p "$ROOT/bin"
cat > "$ROOT/bin/pgrep" <<'EOF'
#!/bin/sh
[ -f "$FAKE_STATE/running" ]
EOF
cat > "$ROOT/bin/pkill" <<'EOF'
#!/bin/sh
rm -f "$FAKE_STATE/running"
EOF
cat > "$ROOT/bin/osascript" <<'EOF'
#!/bin/sh
cat >/dev/null
echo quit >> "$FAKE_STATE/menu-quits"
rm -f "$FAKE_STATE/running"
EOF
cat > "$ROOT/bin/open" <<'EOF'
#!/bin/sh
echo "$1" >> "$FAKE_STATE/opened"
touch "$FAKE_STATE/running"
EOF
chmod +x "$ROOT/bin/"*

# One scenario: fresh dirs, a "new" app installed and running, a "previous" app in dist/rollback/previous.
setup() {
  S="$ROOT/$1"
  mkdir -p "$S/repo/dist/rollback/previous" "$S/Applications/Heimdall.app" "$S/temp" "$S/state"
  echo new > "$S/Applications/Heimdall.app/build"
  echo previous > "$S/repo/dist/rollback/previous/build"
  touch "$S/state/running"
  MARK="$S/heimdall-night-restarted"; OK="$S/temp/heimdall-handshake-ok"; ROLLED="$S/temp/heimdall-rolled-back"
}
stale() { touch -t 202611120106 "$1"; }   # last night's leftover, as in the 12 Nov incident

# Runs the watchdog in the background; the caller may act on the marks, then calls finish.
start_watchdog() {
  env PATH="$ROOT/bin:$PATH" FAKE_STATE="$S/state" HEIMDALL_REPO="$S/repo" HEIMDALL_APP="$S/Applications/Heimdall.app" \
    WATCHDOG_TEMP="$S/temp" WATCHDOG_MARK="$MARK" WATCHDOG_PIDFILE="$S/pid" WATCHDOG_LIFETIME_S=${LIFE:-6} \
    WATCHDOG_HANDSHAKE_S=${HS:-2} WATCHDOG_GRACE_S=60 WATCHDOG_STEP_S=1 WATCHDOG_QUIT_WAIT_S=2 WATCHDOG_SETTLE_S=0 \
    zsh "$WD" > "$S/log" 2>&1 &
  PID=$!
}
finish() { wait $PID; CODE=$?; }

check() {  # check <description> <condition...>
  local what=$1; shift
  if "$@"; then print -r -- "ok   $NAME: $what"
  else print -r -- "FAIL $NAME: $what"; sed 's/^/     | /' "$S/log"; failures=$((failures + 1)); fi
}
not_rolled_back() { [[ ! -e $ROLLED && "$(cat "$S/Applications/Heimdall.app/build")" == new && -d "$S/repo/dist/rollback/previous" && ! -e "$S/state/menu-quits" ]]; }
rolled_back() { [[ -f $ROLLED && "$(cat "$S/Applications/Heimdall.app/build")" == previous && ! -e "$S/repo/dist/rollback/previous" && -f "$S/state/opened" ]]; }

# 1. The incident: a stale restart mark from last night, no handshake. Must never roll back.
scenario_1() {
  NAME=stale-restart-mark; setup $NAME; stale "$MARK"
  start_watchdog; finish
  check "exits at its lifetime" [ $CODE -eq 0 ]
  check "no rollback, app untouched, no menu quit" not_rolled_back
  check "logs the stale mark it ignored" grep -q "ignoring stale mark $MARK" "$S/log"
  check "never saw a restart" eval '! grep -q "restart seen" "$S/log"'
}

# 2. Stale restart mark and stale handshake (both leftovers, as at 15:47): no rollback, and the stale handshake does
#    not end the watch early either.
scenario_2() {
  NAME=stale-both; setup $NAME; stale "$MARK"; stale "$OK"
  start_watchdog; finish
  check "no rollback" not_rolled_back
  check "stale handshake did not end the watch" eval '! grep -q "handshake file present" "$S/log" && grep -q "lifetime reached" "$S/log"'
}

# 3. A stale mark that the installer then touches (the real swap): counts from the touch, and with no handshake the
#    rollback still works.
scenario_3() {
  NAME=stale-then-fresh-restart; setup $NAME; stale "$MARK"
  start_watchdog; sleep 1; touch "$MARK"; finish
  check "rolls back after the fresh restart (exit 1)" [ $CODE -eq 1 ]
  check "previous app restored and relaunched, failed build kept" eval 'rolled_back && ls -d "$S"/repo/dist/rollback/failed-* >/dev/null'
  check "quit through the menu" [ -f "$S/state/menu-quits" ]
}

# 4. A stale handshake does not cover a fresh restart that never shakes hands.
scenario_4() {
  NAME=stale-handshake-fresh-restart; setup $NAME; stale "$OK"
  start_watchdog; sleep 1; touch "$MARK"; finish
  check "rolls back" eval '[ $CODE -eq 1 ] && rolled_back'
}

# 5. The normal good install: fresh restart, then a fresh handshake.
scenario_5() {
  NAME=fresh-handshake; setup $NAME; stale "$MARK"; stale "$OK"
  LIFE=10 HS=5 start_watchdog; sleep 1; touch "$MARK"; sleep 1; touch "$OK"; finish
  check "exits OK on the fresh handshake" eval '[ $CODE -eq 0 ] && grep -q "handshake file present" "$S/log"'
  check "no rollback" not_rolled_back
}

# Every scenario in its own subshell at once; each exits with its failure count, printed in order afterwards.
pids=()
for n in 1 2 3 4 5; do ( scenario_$n; exit $failures ) > "$ROOT/scenario-$n.out" 2>&1 & pids+=($!); done
for n in 1 2 3 4 5; do
  wait ${pids[$n]}; failures=$((failures + $?))
  cat "$ROOT/scenario-$n.out"
done

if (( failures )); then print -r -- "install_watchdog: $failures check(s) failed"; exit 1; fi
print -r -- "install_watchdog: all scenarios passed"

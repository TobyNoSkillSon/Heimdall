#!/bin/zsh
# Install watchdog for a night-pass restart of Heimdall (docs/nightly-build.md, night 1022 handshake).
# Start it detached BEFORE swapping the bundle:
#   nohup scripts/install_watchdog.sh >/tmp/heimdall-watchdog.log 2>&1 &!
# It lives at most 40 minutes. After the installer touches $MARK (the new build was launched), it waits for
# $OK (written by the restarted Heimdall's agent). No $OK within 25 minutes of the restart, or Heimdall not running
# for three checks in a row after a 90 s grace, and it quits the new build, restores dist/rollback/previous,
# relaunches it and writes $ROLLED with the reason.
set -u
H=${HEIMDALL_REPO:-$(cd "$(dirname "$0")/.." && pwd)}
APP=${HEIMDALL_APP:-$HOME/Applications/Heimdall.app}
ROLLBACK="$H/dist/rollback/previous"
# The agent writes the handshake into its home's Assistant/Temp, two levels above the checkout (Projects/Heimdall).
TEMP=${WATCHDOG_TEMP:-$(cd "$H/../.." && pwd)/Assistant/Temp}
OK="$TEMP/heimdall-handshake-ok"
ROLLED="$TEMP/heimdall-rolled-back"
MARK=${WATCHDOG_MARK:-/tmp/heimdall-night-restarted}
LIFETIME=${WATCHDOG_LIFETIME_S:-2400}     # 40 min hard lifetime
HANDSHAKE=${WATCHDOG_HANDSHAKE_S:-1500}   # 25 min after the restart
GRACE=${WATCHDOG_GRACE_S:-90}
STEP=${WATCHDOG_STEP_S:-10}
PROC=${WATCHDOG_PROC:-Heimdall}          # tests use a stand-in process name
QUIT_WAIT=${WATCHDOG_QUIT_WAIT_S:-60}

log() { print -r -- "$(date '+%H:%M:%S') watchdog: $*"; }
start=$(date +%s)
echo $$ > ${WATCHDOG_PIDFILE:-/tmp/heimdall-watchdog.pid}
log "started (pid $$), lifetime ${LIFETIME}s, handshake ${HANDSHAKE}s"

# The main app only: a running Heimdall also has `Heimdall --heimdall-guardian-root …` children that outlive it.
running() {
  if [[ $PROC == Heimdall ]]; then pgrep -fx "$APP/Contents/MacOS/Heimdall" >/dev/null 2>&1
  else pgrep -x "$PROC" >/dev/null 2>&1; fi
}

quit_heimdall() {
  running || return 0
  [[ $PROC == Heimdall ]] && osascript <<'EOF' >/dev/null 2>&1
tell application "System Events" to tell process "Heimdall"
  repeat with b in menu bars
    try
      click menu bar item 1 of b
      delay 0.4
      click (first menu item of menu 1 of menu bar item 1 of b whose name begins with "Quit ")
      exit repeat
    end try
  end repeat
end tell
EOF
  [[ $PROC == Heimdall ]] || pkill -TERM -x "$PROC"
  for i in $(seq 1 $QUIT_WAIT); do running || return 0; sleep 1; done
  # Last resort only: the build being rolled back is unresponsive to its own Quit.
  log "orderly quit failed after 60 s; sending SIGTERM"
  if [[ $PROC == Heimdall ]]; then pkill -TERM -fx "$APP/Contents/MacOS/Heimdall"; else pkill -TERM -x "$PROC"; fi
  for i in $(seq 1 30); do running || return 0; sleep 1; done
  return 1
}

rollback() {
  local reason=$1
  log "ROLLING BACK: $reason"
  if [[ ! -d "$ROLLBACK" ]]; then
    log "no rollback at $ROLLBACK; leaving the current app"
    print -r -- "$(date '+%F %T') rollback impossible (no $ROLLBACK): $reason" > "$ROLLED"
    exit 2
  fi
  quit_heimdall || log "Heimdall still running after quit attempts"
  local failed="$H/dist/rollback/failed-$(date +%Y%m%dT%H%M%S)"
  [[ -d "$APP" ]] && mv "$APP" "$failed"
  mv "$ROLLBACK" "$APP"
  ${WATCHDOG_OPEN:-open} "$APP"
  sleep ${WATCHDOG_SETTLE_S:-20}
  local up=no; running && up=yes
  print -r -- "$(date '+%F %T') rolled back to the previous Heimdall: $reason. Previous app relaunched: $up. Failed build kept at $failed." > "$ROLLED"
  log "rolled back; relaunched=$up"
  exit 1
}

restarted=0; down=0
while true; do
  now=$(date +%s)
  (( now - start > LIFETIME )) && { log "lifetime reached; exiting"; exit 0; }
  [[ -f "$OK" ]] && { log "handshake file present; exiting OK"; exit 0; }
  if (( restarted == 0 )) && [[ -f "$MARK" ]]; then restarted=$(stat -f %m "$MARK"); log "restart seen at $restarted"; fi
  if (( restarted > 0 )); then
    (( now - restarted > HANDSHAKE )) && rollback "no handshake within $((HANDSHAKE / 60)) minutes of the restart"
    if (( now - restarted > GRACE )); then
      if running; then down=0; else down=$((down + 1)); log "Heimdall not running ($down)"; fi
      (( down >= 3 )) && rollback "Heimdall was not running after the restart"
    fi
  elif (( now - start > 600 )); then
    # Installer never marked a restart within 10 minutes: make sure something is running.
    running || rollback "no restart mark after 10 minutes and Heimdall not running"
  fi
  sleep $STEP
done

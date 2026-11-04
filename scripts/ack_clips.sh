#!/bin/bash
# Generate the instant-acknowledgement clips (every current ack in acks.json, English and Polish, plain lines and
# delivery variants) in Jarvis's voice with Drama 3 into a
# lab bank, then import them into Heimdall's data directory. Dry-run import by default; --apply writes.
# Install time only: refuses while the installed Heimdall runs (it holds Jarvis and would unload him for RAM).
#
#   scripts/ack_clips.sh --lab DIR --into HEIMDALL_DATA [--apply] [--assets DRAMA3_PREPARED] [--takes N]
#                        [--max-rejects N] [--plan]
#
# --lab: a lab directory (the bank goes to DIR/bank; rerunning resumes it). --into: the data directory, e.g.
# "$HOME/Library/Application Support/Heimdall". --assets: the prepared Drama 3 runtime (default
# <target>/drama3-prepared, made by the bundle step). --plan: print the checks and cost, then stop.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LAB="" INTO="" APPLY=0 ASSETS="" TAKES=2 MAX_REJECTS=3 PLAN=0
# The installed app's processes (main binary, its Drama 3 sidecar). Overridable for the script's own test.
APP_PATTERN=${ACK_CLIPS_APP_PATTERN:-Heimdall\.app/Contents/}
# Current acks (retired ones, with replaced_by, get no takes).
ACKS=$(python3 -I -c 'import json,sys;print(sum("replaced_by" not in a for a in json.load(open(sys.argv[1]))))' "$ROOT/crates/voicebank/acks.json")
GIB=$((1024 * 1024 * 1024))
# docs/drama3-integration.md: ~27 GB Drama 3 resident (measured 26.1 GB footprint), 2 GB for the ASR check,
# 2 GiB margin (the app's own admission). RTF ~0.48 (0.34-0.48 measured), load ~8.4 s.
RESIDENT_GIB=29 ADMISSION_GIB=31 RAM_CEILING_GB=100

die() { echo "ack_clips: $*" >&2; exit 2; }
while [ $# -gt 0 ]; do
  case "$1" in
    --lab) LAB=${2:?--lab needs a directory}; shift 2 ;;
    --into) INTO=${2:?--into needs the Heimdall data directory}; shift 2 ;;
    --assets) ASSETS=${2:?--assets needs a directory}; shift 2 ;;
    --takes) TAKES=${2:?}; shift 2 ;;
    --max-rejects) MAX_REJECTS=${2:?}; shift 2 ;;
    --apply) APPLY=1; shift ;;
    --dry-run) APPLY=0; shift ;;
    --plan) PLAN=1; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) die "unknown flag $1" ;;
  esac
done
[ -n "$LAB" ] || die "--lab DIR required"
[ -n "$INTO" ] || die "--into HEIMDALL_DATA required"
[[ "$TAKES" =~ ^[1-9][0-9]?$ ]] || die "--takes must be 1..99"
[[ "$MAX_REJECTS" =~ ^[0-9]+$ ]] || die "--max-rejects must be a number"

heimdall_running() { pgrep -f "$APP_PATTERN" >/dev/null 2>&1; }
if heimdall_running; then
  die "Heimdall is running (it holds Jarvis and would unload him when RAM runs short). Quit Heimdall first; nothing was done."
fi

CLIPS=$((ACKS * TAKES))
cat <<EOF
ack_clips: ${ACKS} acks x ${TAKES} takes = ${CLIPS} Jarvis clips (Drama 3), each checked by ASR in its own language.
ack_clips: cost: about ${RESIDENT_GIB} GB RAM while it runs (Drama 3 ~27 GB + ASR ~2 GB); needs ${ADMISSION_GIB} GiB free to load
ack_clips:       and the Mac under ${RAM_CEILING_GB} GB in use; GPU busy throughout; about 5-15 minutes (estimate:
ack_clips:       ~10 s load, ~0.5 s render per short line at RTF 0.48, plus a few seconds of ASR start per take, unmeasured).
ack_clips: disk: about 0.2 MB per take (~$((CLIPS / 5 + 1)) MB). Import: $([ $APPLY = 1 ] && echo apply || echo dry run).
EOF
[ $PLAN = 1 ] && exit 0

export PATH="$HOME/.cargo/bin:$PATH" CC=/usr/bin/clang CXX=/usr/bin/clang++
TARGET=$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')
ASSETS=${ASSETS:-$TARGET/drama3-prepared}
[ -x "$ASSETS/runtime/bin/python3" ] && [ -f "$ASSETS/sidecar.py" ] \
  || die "no prepared Drama 3 runtime at $ASSETS (build the bundle, or python3 scripts/drama3_prepare.py --destination DIR, and pass --assets DIR)"
mkdir -p "$LAB"
LAB=$(cd "$LAB" && pwd)
BANK=$LAB/bank

echo "ack_clips: building heimdall-voicebank and the ASR probe (release, low priority)"
(cd "$ROOT" && nice -n 19 cargo build --release -j2 -p voicebank --bin heimdall-voicebank \
  -p listen --bin heimdall-listen-opt-probe)
BIN=$TARGET/release

# RAM check just before loading. Available = what the system can hand out now (psutil, as the app's admission).
read -r AVAILABLE TOTAL < <("$ASSETS/runtime/bin/python3" -I -B -c 'import psutil;m=psutil.virtual_memory();print(m.available,m.total)')
NEED=$((ADMISSION_GIB * GIB))
CEILING_NEED=$((TOTAL - RAM_CEILING_GB * 1000 * 1000 * 1000 + RESIDENT_GIB * GIB))
[ $CEILING_NEED -gt $NEED ] && NEED=$CEILING_NEED
echo "ack_clips: free RAM $((AVAILABLE / GIB)) GiB, need $((NEED / GIB)) GiB; $(sysctl -n vm.swapusage)"
[ "$AVAILABLE" -ge "$NEED" ] || die "not enough free RAM to load Drama 3; close something and rerun (the bank resumes)."
heimdall_running && die "Heimdall started meanwhile; quit it and rerun."

echo "ack_clips: generating into $BANK (log: $LAB/fill.jsonl, $LAB/fill.stderr)"
set +e
/usr/bin/lockf -k -t 600 "$TARGET/.gpu-measure.lock" \
  "$BIN/heimdall-voicebank" fill --backend drama3 --set acks --models "$ASSETS" \
  --asr-probe "$BIN/heimdall-listen-opt-probe" --bank "$BANK" --takes "$TAKES" \
  --lines $((CLIPS * (MAX_REJECTS + 1) + 1)) --max-rejects "$MAX_REJECTS" \
  > >(tee -a "$LAB/fill.jsonl") 2>>"$LAB/fill.stderr"
FILL=$?
set -e
case $FILL in
  0) echo "ack_clips: every ack has its takes" ;;
  3) echo "ack_clips: some acks were given up after $MAX_REJECTS rejected takes (see the gave_up list above); importing the rest" ;;
  *) die "fill failed (exit $FILL); see $LAB/fill.stderr. Rerun to resume." ;;
esac

MODE=--dry-run; [ $APPLY = 1 ] && MODE=--apply
# The fill takes minutes: Heimdall must still be quit when its store is written.
heimdall_running && die "Heimdall started during the fill; quit it and rerun (the lab bank is kept and resumes)."
"$BIN/heimdall-voicebank" import --from "$BANK" --into "$INTO" $MODE

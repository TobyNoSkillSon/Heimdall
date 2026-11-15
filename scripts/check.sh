#!/bin/bash
# Heimdall's one check. Never installs, never launches the installed app, never requests permissions.
#
#   scripts/check.sh --change [BASE]   check my change: the tests of every crate the change can reach (scripts/
#                                      affected_crates.py; BASE defaults to main), their doctests, the script tests
#   scripts/check.sh --tests           every workspace test, doctest and gate; works on an uncommitted tree
#   scripts/check.sh                   ready to install: every test above, the signed personal candidate and its verify,
#                                      then the model tests one at a time under the GPU lock (scripts/model_tests.py);
#                                      sources must be committed (scripts/bundle.sh)
#   scripts/check.sh --status          the run in progress in this worktree, or the last one's result
#
# One run per worktree at a time: a second one stops at once and names the running one and its log (no waiting).
# Each run writes its own folder, target/check/<time>-<mode>/ (target/check/latest points at the newest), with one log
# per step, `times`, `failed`, each nextest run's JUnit report (<step>.junit.xml, every test's time) and `result`;
# nothing is overwritten or truncated.
#
# Every step runs at nice 19 with two build jobs in all (the user's rule; one per lane when the full run builds the candidate
# beside the tests); when the Mac is on power and nobody has touched it for 10 minutes, builds use every core instead
# (nice 19 changes nothing on an idle Mac), and the public edition's build and the doctests run beside the suite.
# HEIMDALL_JOBS overrides.
#
# Tests run under cargo-nextest (all test binaries at once, one process per test) with TMPDIR on a private RAM disk:
# the archive journal commits with F_FULLFSYNC, which costs milliseconds per commit on the SSD and, with every test
# process flushing at once, turned 1-5 s tests into 50-120 s ones and timeouts (1 Nov). Tests, code under test and
# its SQLite durability settings are unchanged. The candidate build and its verify run beside the tests in their own
# target directory. Both target directories persist between runs: they are the build cache.
set -euo pipefail
cd "$(dirname "$0")/.."
SELF="$PWD/scripts/check.sh"
export PATH="$HOME/.cargo/bin:$PATH" CC=/usr/bin/clang CXX=/usr/bin/clang++
# The selected Xcode (xcode-select), never the Command Line Tools by default: the speakers compiler lock
# (crates/speakers/native/build-lock.json) names Xcode's clang, and xcrun metal is Xcode's (2 Nov).
MODE="${1:-full}" BASE="${2:-main}"
case "$MODE" in full|--tests|--change|--status) ;; *) echo 'Usage: scripts/check.sh [--change [BASE] | --tests | --status]' >&2; exit 2 ;; esac
LOCK="${HEIMDALL_CHECK_LOCK:-$(git rev-parse --absolute-git-dir)/heimdall-check.lock}"   # override: tests only

if [[ "$MODE" == --status ]]; then
  if [[ -f "$LOCK.info" ]] && ! lockf -k -s -t 0 "$LOCK" true 2>/dev/null; then echo "running: $(cat "$LOCK.info")"; exit 0; fi
  if [[ -e target/check/latest ]]; then echo "last: $(cat target/check/latest/result 2>/dev/null || echo 'no result (stopped early)') ($(readlink target/check/latest))"; else echo 'no run yet'; fi
  exit 0
fi
# Take this worktree's lock (released by the kernel when the run ends, however it ends), at nice 19.
if [[ -z "${HEIMDALL_CHECK_LOCKED:-}" ]]; then
  set +e
  lockf -k -s -t 0 "$LOCK" env HEIMDALL_CHECK_LOCKED=1 nice -n 19 "$SELF" "$@"
  status=$?
  if (( status == 75 )); then
    echo "Another check is running in this worktree: $(cat "$LOCK.info" 2>/dev/null). Wait for it to finish; its result (scripts/check.sh --status) names its mode and commit, and only a full run on the commit you install clears an install." >&2
  fi
  exit $status
fi
command -v cargo-nextest >/dev/null || { echo 'cargo-nextest is missing (docs/contributing.md).' >&2; exit 1; }

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RUN="$PWD/target/check/$STAMP-${MODE#--}"
mkdir -p "$RUN"; : >"$RUN/failed"
ln -sfn "$RUN" target/check/latest
echo "pid $$, ${MODE#--} since $(date '+%H:%M:%S'), log $RUN" > "$LOCK.info"
trap 'rm -f "$LOCK.info"' EXIT
COMMIT="$(git rev-parse HEAD)" DIRTY=""
[[ -z "$(git status --porcelain --untracked-files=no)" ]] || DIRTY=" with uncommitted changes"
# Keep the ten newest run folders (logs and the verify replay's lab data).
ls -1d "$PWD"/target/check/*/ | grep -v '/latest/$' | sort -r | tail -n +11 | while read -r old; do rm -rf "$old"; done
TEST_TARGET="${TEST_TARGET:-${CARGO_TARGET_DIR:-$PWD/target/test}}"
CANDIDATE_TARGET="${CARGO_TARGET_DIR:-$PWD/target/release-candidate}"
unset CARGO_TARGET_DIR
# Incremental caches pile up, one per feature set (the workspace, the public edition, each --change subset): drop the
# ones no build has touched for a week (13 Nov: 44 GB of incremental caches in main's test target, 12 GB of them older
# than 7 days). A dropped set's next build compiles its own crates afresh, once.
if [[ -d "$TEST_TARGET/debug/incremental" ]]; then
  find "$TEST_TARGET/debug/incremental" -mindepth 1 -maxdepth 1 -type d -mtime +7 -exec rm -rf {} +
fi
# Object files left in deps/ by test builds with line tables (before 14 Nov; the test profile has no debug info now):
# 217,564 in main's on 13 Nov, and CoreText lists that folder in every test that first draws text (1.5 s per test
# process at that size, 0.1 s at 24,000 files, measured). Nothing reads them any more.
if [[ -d "$TEST_TARGET/debug/deps" ]]; then find "$TEST_TARGET/debug/deps" -maxdepth 1 -name '*.o' -delete; fi
# A worktree's first run starts from a copy-on-write clone of main's worktree test target (no disk until files change): the third-party crates and MLX's CMake build are reused, only Heimdall's own crates rebuild (cold
# 746 s -> 454 s at two jobs, 1 Nov). Cargo relocates the cloned build-script outputs to the new folder. Heimdall's
# own crates always rebuild: their fingerprints are removed, because their test binaries carry absolute paths into
# the target they were built in (CARGO_BIN_EXE_*) and cargo would not notice the folder moved; their build-script
# folders go too (their CMake caches name the old folder).
if [[ ! -e "$TEST_TARGET" ]]; then
  SEED="$(git worktree list --porcelain | awk '/^worktree /{w=$2} /^branch refs\/heads\/main$/ && !n {print w; n=1}')/target/test"
  if [[ -d "$SEED/debug" && "$SEED" != "$TEST_TARGET" ]]; then
    # Not the object files in deps/ (they belong to Heimdall's own test binaries, which rebuild here anyway) and not the
    # incremental caches (rustc cannot reuse them from another checkout's source paths: no faster rebuild, measured
    # 13 Nov). Together they were 342,000 of the 350,000 files to clone, and the clone took 13 minutes.
    seeded=1
    mkdir -p "$TEST_TARGET/debug/deps"
    while IFS= read -r -d '' entry; do
      case "$entry" in "$SEED/debug"|"$SEED/debug/deps"|"$SEED/debug/incremental") continue ;; esac
      cp -c -R "$entry" "$TEST_TARGET/${entry#"$SEED"/}" || seeded=0
    done < <(find "$SEED" "$SEED/debug" -mindepth 1 -maxdepth 1 -print0)
    find "$SEED/debug/deps" -maxdepth 1 -type f ! -name '*.o' -exec sh -c 'exec cp -c "$@" "$0"' "$TEST_TARGET/debug/deps/" {} + || seeded=0
    if (( seeded )); then
      OWN="$(cargo metadata --no-deps --offline --format-version 1 | python3 -c 'import json,sys; print(" ".join(p["name"] for p in json.load(sys.stdin)["packages"]))')"
      [[ -n "$OWN" ]] || { rm -rf "$TEST_TARGET"; echo "cargo metadata gave no packages; the seed would test stale binaries" >&2; exit 1; }
      for name in $OWN; do
        rm -rf "$TEST_TARGET"/debug/.fingerprint/"$name"-???????????????? "$TEST_TARGET"/debug/build/"$name"-????????????????
      done
      # CMake refuses a build folder that moved; a third-party build script that ever reruns configures afresh.
      find "$TEST_TARGET/debug/build" -name CMakeCache.txt -delete
      echo "test target cloned from $SEED; Heimdall's crates rebuild" >>"$RUN/times"
    else rm -rf "$TEST_TARGET"; fi
  fi
fi

# Build jobs: two while the user may be using the Mac; every core when it is on power and idle for 10 minutes.
# (Pipes read to the end: an early-exiting reader would kill the writer with SIGPIPE, and pipefail would stop the run.)
idle_s="$(ioreg -c IOHIDSystem | awk '/HIDIdleTime/ && !n {print int($NF/1000000000); n=1}')"
if [[ -n "${HEIMDALL_JOBS:-}" ]]; then JOBS="$HEIMDALL_JOBS" WHY="HEIMDALL_JOBS"
elif [[ "$(pmset -g batt)" == *"AC Power"* ]] && (( ${idle_s:-0} >= 600 )); then JOBS="$(sysctl -n hw.ncpu)" WHY="on power, idle ${idle_s} s"
else JOBS=2 WHY="Mac in use or on battery"; fi
# Two jobs are two in all: the full run's test and candidate lanes build at the same time, so each gets one.
LANE_JOBS=$JOBS
if [[ "$MODE" == full ]] && (( JOBS <= 2 )); then LANE_JOBS=1; fi
export CARGO_BUILD_JOBS="$LANE_JOBS"
echo "jobs: $JOBS ($WHY), $LANE_JOBS per lane; nice: $(ps -o nice= -p $$ | tr -d ' ')" >> "$RUN/times"

now() { python3 -c 'import time; print(time.time())'; }
since() { python3 -c "print(f'{$(now) - $1:.0f} s')"; }
# step NAME COMMAND...: log to $RUN/NAME.log, record its wall time or its failure.
step() {
  local name=$1 start; shift; start=$(now)
  if "$@" >"$RUN/$name.log" 2>&1; then echo "$name: $(since "$start")" >>"$RUN/times"
  else echo "$name: $(since "$start") FAILED" >>"$RUN/times"; echo "$name" >>"$RUN/failed"; return 1; fi
}
# nextest NAME ARGS...: one nextest run, its JUnit report kept beside its log. nextest writes the report into its
# store, target/nextest/ under the checkout whatever CARGO_TARGET_DIR says (.config/nextest.toml), and each run
# replaces it; nextest runs one after another here, so each report is moved before the next starts.
JUNIT="$PWD/target/nextest/default/junit.xml"
nextest() {
  local name=$1; shift
  local status=0
  rm -f "$JUNIT"
  step "$name" "${TESTENV[@]}" cargo nextest run "$@" || status=1
  if [[ -f "$JUNIT" ]]; then mv "$JUNIT" "$RUN/$name.junit.xml"; fi
  return $status
}

# Private RAM disk for test temp files: 20 GiB of address space; memory is used only
# as tests write (about 0.5 GB for the suite) and freed on detach. Hidden from Finder,
# detached on every exit path.
RAM_DEV="" RAM_MNT="" GPU_HELD=0
cleanup() {
  if [[ -n "$RAM_MNT" ]]; then diskutil unmount force "$RAM_MNT" >/dev/null 2>&1 || true; rmdir "$RAM_MNT" 2>/dev/null || true; fi
  if [[ -n "$RAM_DEV" ]]; then diskutil eject "$RAM_DEV" >/dev/null 2>&1 || true; fi
  if (( GPU_HELD )) && [[ "$(cat "$GPU_LOCK" 2>/dev/null)" == *" pid $$ "* ]]; then rm -f "$GPU_LOCK"; fi
  rm -f "$LOCK.info"
}
trap cleanup EXIT
RAM_DEV="$(diskutil image attach --noMount ram://41943040 | awk 'NR==1{print $1}')"
newfs_apfs -v HeimdallCheck "$RAM_DEV" >/dev/null
VOLUME=""
for _ in $(seq 50); do
  VOLUME="$(diskutil list | awk -v d="${RAM_DEV#/dev/}" '/^\/dev\//{f=0} /Physical Store/ && $NF==d {f=1} f && /APFS Volume/ && !n {print $NF; n=1}')"
  [[ -n "$VOLUME" ]] && break
  sleep 0.2
done
[[ -n "$VOLUME" ]] || { echo "RAM disk volume did not appear on $RAM_DEV" >&2; exit 1; }
RAM_MNT="$(mktemp -d /tmp/heimdall-check-ram.XXXXXX)"
diskutil mount -mountOptions nobrowse -mountPoint "$RAM_MNT" "$VOLUME" >/dev/null
mkdir -p "$RAM_MNT/tmp"
TESTENV=(env TMPDIR="$RAM_MNT/tmp/" CARGO_TARGET_DIR="$TEST_TARGET")


# The Python tests of the build, release and voice scripts: seconds, so every mode runs them all.
script_tests() {
  local status=0
  step ack-clips python3 -B scripts/test_ack_clips.py || status=1   # install-time ack generation (voice lane, 22 Oct)
  step release-docs python3 scripts/test_release_docs.py || status=1
  step release-verify-rules python3 -B scripts/test_verify_release_candidate.py || status=1   # Floating Islands bundle content (night 1022)
  step public-bundle-rules python3 -B scripts/test_check_public_bundle.py || status=1   # no build-machine paths (VM finding P-03)
  step public-split-rules python3 -B scripts/test_check_public_split.py || status=1   # private text never in the public layer (14 Nov)
  step affected-crates python3 -B scripts/test_affected_crates.py || status=1
  step dialog-watch python3 -B scripts/test_dialog_watch.py || status=1   # system dialogs during unattended runs (14 Nov)
  step check-lock python3 -B scripts/test_check_lock.py || status=1
  step install-watchdog zsh scripts/test_install_watchdog.sh || status=1   # stale marks never roll back (incident 12 Nov 15:47)
  step model-test-selection python3 -B scripts/test_model_tests.py || status=1   # the full check's model step
  step bundle-models python3 -B scripts/test_bundle_models.py || status=1   # clones and the Drama engine stamp (14 Nov)
  return $status
}
# The public edition's gates, built with its feature (docs/editions.md); the suite is the personal edition. t3 for its
# task default, which names no vendor in the public edition (`workers::catalog_default`).
PUBLIC=(-p app -p agent -p listen -p t3 --no-default-features --features app/public)
PUBLIC_TESTS='test(edition) | test(the_public) | test(the_prompt_s_worked) | test(the_prompt_names) | test(installed_tests) | test(relay_prompt) | test(one_general_prompt) | test(every_skill_in_the_repository) | test(the_release_check) | test(no_general_skill_names)'
# With spare cores (more than two jobs), the doctests and the public edition's build run while the suite runs: they
# start once the suite's nextest has built and starts its tests (cargo locks the target folder, so earlier they would
# only queue behind it, or hold it back), and the public build takes half the jobs. A step this skipped (the suite
# ended without starting) runs after the suite as before. Arguments: 1 to build the public edition, then the doctests'
# -p list (none: no doctests).
beside_suite() {
  local public=$1 status=0; shift
  until grep -q '^ *Starting ' "$RUN/tests.log" 2>/dev/null; do
    grep -q '^tests: ' "$RUN/times" && return 0
    sleep 1
  done
  if (( $# )); then step doctests "${TESTENV[@]}" cargo test "$@" --doc || status=1; fi
  if (( public )); then
    CARGO_BUILD_JOBS=$(( JOBS > 3 ? JOBS / 2 : 1 )) step public-build "${TESTENV[@]}" cargo nextest run "${PUBLIC[@]}" --no-run || status=1
  fi
  return $status
}
ran() { grep -q "^$1: " "$RUN/times"; }
# Every step runs even after another fails, so one run reports every failure. The script tests need no build and
# run from the start.
run_tests() {
  local status=0 scripts beside=""
  script_tests & scripts=$!
  # Splash (crates/splash) turns on serde_json's preserve_order (it and llguidance need template key order). Built in
  # one invocation with the rest, that feature would reorder every JSON object Heimdall writes, so Splash is tested on
  # its own, as scripts/bundle.sh builds it (`serde_order_tests` in crates/app guards the rest).
  step test-build "${TESTENV[@]}" cargo nextest run --workspace --exclude splash --no-run || status=1
  if (( JOBS > 2 )); then beside_suite 1 --workspace --exclude splash & beside=$!; fi
  nextest tests --workspace --exclude splash --no-fail-fast --final-status-level slow || status=1
  nextest splash-tests -p splash --no-fail-fast --final-status-level slow || status=1
  if [[ -n "$beside" ]]; then wait "$beside" || status=1; fi
  if ! ran doctests; then   # nextest skips doctests
    step doctests "${TESTENV[@]}" cargo test --workspace --exclude splash --doc || status=1
  fi
  nextest public-edition "${PUBLIC[@]}" -E "$PUBLIC_TESTS" || status=1
  wait "$scripts" || status=1
  return $status
}
# The day loop: only the crates the change can reach. The full run above still runs before every install.
run_change() {
  local status=0 crates scripts beside=""
  script_tests & scripts=$!
  crates="$(python3 -B scripts/affected_crates.py "$BASE" 2>"$RUN/affected.log")" || { echo affected-selection >>"$RUN/failed"; wait "$scripts"; return 1; }
  echo "affected: $(echo $crates)" >>"$RUN/times"
  if [[ "$crates" == ALL ]]; then wait "$scripts" || status=1; run_tests || status=1; return $status; fi
  if [[ -n "$crates" ]]; then
    local packages=() rest=() splash=0 name public=0
    for name in $crates; do if [[ "$name" == splash ]]; then splash=1; else packages+=(-p "$name"); rest+=("$name"); fi; done
    [[ " ${rest[*]:-} " =~ \ (app|agent|listen|t3)\  ]] && public=1
    # Doctests exist only in libraries (app is a binary).
    local libs=() lib
    for lib in $(cargo metadata --no-deps --offline --format-version 1 | python3 -c 'import json,sys; print(" ".join(p["name"] for p in json.load(sys.stdin)["packages"] if any("lib" in t["kind"] for t in p["targets"])))'); do
      [[ " ${rest[*]:-} " == *" $lib "* ]] && libs+=(-p "$lib")
    done
    if (( ${#packages[@]} )); then
      if (( JOBS > 2 )); then beside_suite "$public" ${libs[@]+"${libs[@]}"} & beside=$!; fi
      nextest tests "${packages[@]}" --no-fail-fast --final-status-level slow || status=1
      if [[ -n "$beside" ]]; then wait "$beside" || status=1; fi
      if (( ${#libs[@]} )) && ! ran doctests; then step doctests "${TESTENV[@]}" cargo test "${libs[@]}" --doc || status=1; fi
    fi
    if (( splash )); then nextest splash-tests -p splash --no-fail-fast --final-status-level slow || status=1; fi
    if (( public )); then nextest public-edition "${PUBLIC[@]}" -E "$PUBLIC_TESTS" || status=1; fi
  fi
  wait "$scripts" || status=1
  return $status
}
# The model tests (`#[ignore = "model: ..."]`, scripts/model_tests.py): the default suite runs no model inference, and
# the full check runs every one of them, after both lanes, one at a time, under the GPU lock every agent on this Mac
# shares (Assistant/Temp/gpu-lock: created with the holder's name, polled every 2 minutes while someone else holds it).
# Stopped if swap grows by more than 512 MB (the user's rules: one heavy GPU job at a time; no swap growth).
VAULT_TEMP="$(cd "$(git rev-parse --git-common-dir)/.." && pwd)/../../Assistant/Temp"
[[ -d "$VAULT_TEMP" ]] || VAULT_TEMP="$PWD/target"   # a checkout outside the user's Vault
GPU_LOCK="${HEIMDALL_GPU_LOCK_FILE:-$VAULT_TEMP/gpu-lock}"
swap_mb() { sysctl -n vm.swapusage | awk '{sub("M", "", $6); print int($6)}'; }
kill_tree() { local child; for child in $(pgrep -P "$1"); do kill_tree "$child"; done; kill -TERM "$1" 2>/dev/null || true; }
model_tests() {
  local filter splash waited=0 status=0
  filter="$(python3 -B scripts/model_tests.py)" && splash="$(python3 -B scripts/model_tests.py --splash)" \
    || { echo model-tests >>"$RUN/failed"; return 1; }
  if [[ -z "$filter$splash" ]]; then echo "model-tests: none marked" >>"$RUN/times"; return 0; fi
  until ( set -o noclobber; echo "check.sh pid $$ ($PWD, $(date '+%H:%M:%S'), $RUN)" > "$GPU_LOCK" ) 2>/dev/null; do
    if (( waited >= ${HEIMDALL_GPU_WAIT_S:-3600} )); then
      echo "model-tests: GPU lock $GPU_LOCK still held after $waited s by: $(cat "$GPU_LOCK" 2>/dev/null)" >>"$RUN/times"
      echo model-tests >>"$RUN/failed"; return 1
    fi
    sleep 120; waited=$((waited + 120))
  done
  GPU_HELD=1
  local swap0 job name args
  swap0=$(swap_mb)
  for name in model-tests model-tests-splash; do
    if [[ "$name" == model-tests ]]; then [[ -n "$filter" ]] || continue; args=(--workspace --exclude splash -E "$filter")
    else [[ -n "$splash" ]] || continue; args=(-p splash -E "$splash"); fi
    nextest "$name" "${args[@]}" --run-ignored only --test-threads 1 --no-tests warn --no-fail-fast --final-status-level slow & job=$!
    while kill -0 "$job" 2>/dev/null; do
      if (( $(swap_mb) > swap0 + 512 )); then
        echo "$name: stopped, swap grew from $swap0 MB to $(swap_mb) MB" >>"$RUN/times"; kill_tree "$job"; break
      fi
      sleep 2
    done
    wait "$job" || status=1
  done
  rm -f "$GPU_LOCK"; GPU_HELD=0
  return $status
}
run_candidate() {
  step candidate-build env CARGO_TARGET_DIR="$CANDIDATE_TARGET" scripts/bundle.sh --edition personal &&   # The user's install (docs/editions.md)
  step candidate-verify python3 scripts/verify_release_candidate.py \
    --evidence "dist/check-$(git rev-parse --short HEAD)-$STAMP" --lab "$RUN/candidate-lab"
}

START=$(now)
if [[ "$MODE" == --change ]]; then run_change & TESTS=$!; else run_tests & TESTS=$!; fi
CANDIDATE=""
if [[ "$MODE" == full ]]; then run_candidate & CANDIDATE=$!; fi
STATUS=0
wait "$TESTS" || STATUS=1
if [[ -n "$CANDIDATE" ]]; then wait "$CANDIDATE" || STATUS=1; fi
if [[ "$MODE" == full ]]; then model_tests || STATUS=1; fi

cat "$RUN/times"
grep -h -E '^ +Summary' "$RUN"/*tests.log "$RUN"/public-edition.log "$RUN"/model-tests*.log 2>/dev/null || true
if [[ -s "$RUN/candidate-verify.log" ]]; then
  tail -1 "$RUN/candidate-verify.log" | python3 -c 'import json,sys; r=json.loads(sys.stdin.read()); print("candidate", r["source_commit"][:7], "verified; tree sha256", r["bundle_tree_sha256"][:16])' 2>/dev/null || true
fi
echo "total: $(since "$START") (logs: $RUN)" | tee -a "$RUN/times"
if (( STATUS )) || [[ -s "$RUN/failed" ]]; then
  while read -r name; do echo "--- $name failed; end of $RUN/$name.log"; tail -30 "$RUN/$name.log"; done <"$RUN/failed"
  echo "FAILED: $(tr '\n' ' ' <"$RUN/failed" 2>/dev/null) (${MODE#--}, $COMMIT$DIRTY)" >"$RUN/result"
  echo 'CHECK FAILED' >&2
  exit 1
fi
echo "PASSED (${MODE#--}, $COMMIT$DIRTY)" >"$RUN/result"
echo 'CHECK PASSED'

#!/bin/bash
# Heimdall's one check. Never installs, never launches the installed app, never requests permissions.
#
#   scripts/check.sh --change [BASE]   check my change: the tests of every crate the change can reach (scripts/
#                                      affected_crates.py; BASE defaults to main), their doctests, the script tests
#   scripts/check.sh --tests           every workspace test, doctest and gate; works on an uncommitted tree
#   scripts/check.sh                   ready to install: every test above, plus the signed personal candidate and its
#                                      verify; sources must be committed (scripts/bundle.sh)
#   scripts/check.sh --status          the run in progress in this worktree, or the last one's result
#
# One run per worktree at a time: a second one stops at once and names the running one and its log (no waiting).
# Each run writes its own folder, target/check/<time>-<mode>/ (target/check/latest points at the newest), with one log
# per step, `times`, `failed`, nextest's JUnit report and `result`; nothing is overwritten or truncated.
#
# Every step runs at nice 19 with two build jobs (the user's rule); when the Mac is on power and nobody has touched it for
# 10 minutes, builds use every core instead (nice 19 changes nothing on an idle Mac). HEIMDALL_JOBS overrides.
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
command -v cargo-nextest >/dev/null || { echo 'cargo-nextest is missing (docs/install.md, "Full check").' >&2; exit 1; }

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
# A worktree's first run starts from a copy-on-write clone of main's worktree test target (seconds, no disk until
# files change): the third-party crates and MLX's CMake build are reused, only Heimdall's own crates rebuild (cold
# 746 s -> 454 s at two jobs, 1 Nov). Cargo relocates the cloned build-script outputs to the new folder. Heimdall's
# own crates always rebuild: their fingerprints are removed, because their test binaries carry absolute paths into
# the target they were built in (CARGO_BIN_EXE_*) and cargo would not notice the folder moved; their build-script
# folders go too (their CMake caches name the old folder).
if [[ ! -e "$TEST_TARGET" ]]; then
  SEED="$(git worktree list --porcelain | awk '/^worktree /{w=$2} /^branch refs\/heads\/main$/ && !n {print w; n=1}')/target/test"
  if [[ -d "$SEED/debug" && "$SEED" != "$TEST_TARGET" ]]; then
    mkdir -p "$(dirname "$TEST_TARGET")"
    if cp -c -R "$SEED" "$TEST_TARGET"; then
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
export CARGO_BUILD_JOBS="$JOBS"
echo "jobs: $JOBS ($WHY); nice: $(ps -o nice= -p $$ | tr -d ' ')" >> "$RUN/times"

now() { python3 -c 'import time; print(time.time())'; }
since() { python3 -c "print(f'{$(now) - $1:.0f} s')"; }
# step NAME COMMAND...: log to $RUN/NAME.log, record its wall time or its failure.
step() {
  local name=$1 start; shift; start=$(now)
  if "$@" >"$RUN/$name.log" 2>&1; then echo "$name: $(since "$start")" >>"$RUN/times"
  else echo "$name: $(since "$start") FAILED" >>"$RUN/times"; echo "$name" >>"$RUN/failed"; return 1; fi
}
# nextest NAME ARGS...: one nextest run, its JUnit report kept beside its log.
nextest() {
  local name=$1; shift
  local status=0
  step "$name" "${TESTENV[@]}" cargo nextest run "$@" || status=1
  [[ -f "$TEST_TARGET/nextest/default/junit.xml" ]] && mv "$TEST_TARGET/nextest/default/junit.xml" "$RUN/$name.junit.xml"
  return $status
}

# Private RAM disk for test temp files: 20 GiB of address space; memory is used only
# as tests write (about 0.5 GB for the suite) and freed on detach. Hidden from Finder,
# detached on every exit path.
RAM_DEV="" RAM_MNT=""
cleanup() {
  if [[ -n "$RAM_MNT" ]]; then diskutil unmount force "$RAM_MNT" >/dev/null 2>&1 || true; rmdir "$RAM_MNT" 2>/dev/null || true; fi
  if [[ -n "$RAM_DEV" ]]; then diskutil eject "$RAM_DEV" >/dev/null 2>&1 || true; fi
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
  step affected-crates python3 -B scripts/test_affected_crates.py || status=1
  step check-lock python3 -B scripts/test_check_lock.py || status=1
  return $status
}
# Every step runs even after another fails, so one run reports every failure.
run_tests() {
  local status=0
  # Splash (crates/splash) turns on serde_json's preserve_order (it and llguidance need template key order). Built in
  # one invocation with the rest, that feature would reorder every JSON object Heimdall writes, so Splash is tested on
  # its own, as scripts/bundle.sh builds it (`serde_order_tests` in crates/app guards the rest).
  step test-build "${TESTENV[@]}" cargo nextest run --workspace --exclude splash --no-run || status=1
  nextest tests --workspace --exclude splash --no-fail-fast --final-status-level slow || status=1
  nextest splash-tests -p splash --no-fail-fast --final-status-level slow || status=1
  step doctests "${TESTENV[@]}" cargo test --workspace --exclude splash --doc || status=1   # nextest skips doctests
  script_tests || status=1
  # The public edition's gates, built with its feature (docs/editions.md); the suite above is the personal edition.
  nextest public-edition -p app -p agent -p listen --no-default-features --features app/public \
    -E 'test(edition) | test(the_public) | test(the_prompt_s_worked) | test(the_prompt_names) | test(installed_tests) | test(relay_prompt)' || status=1
  return $status
}
# The day loop: only the crates the change can reach. The full run above still runs before every install.
run_change() {
  local status=0 crates
  crates="$(python3 -B scripts/affected_crates.py "$BASE" 2>"$RUN/affected.log")" || { echo affected-selection >>"$RUN/failed"; return 1; }
  echo "affected: $(echo $crates)" >>"$RUN/times"
  if [[ "$crates" == ALL ]]; then run_tests; return; fi
  if [[ -n "$crates" ]]; then
    local packages=() rest=() splash=0 name
    for name in $crates; do if [[ "$name" == splash ]]; then splash=1; else packages+=(-p "$name"); rest+=("$name"); fi; done
    if (( ${#packages[@]} )); then
      nextest tests "${packages[@]}" --no-fail-fast --final-status-level slow || status=1
      # Doctests exist only in libraries (app and ming-arsenal-runner are binaries).
      local libs=() lib
      for lib in $(cargo metadata --no-deps --offline --format-version 1 | python3 -c 'import json,sys; print(" ".join(p["name"] for p in json.load(sys.stdin)["packages"] if any("lib" in t["kind"] for t in p["targets"])))'); do
        [[ " ${rest[*]} " == *" $lib "* ]] && libs+=(-p "$lib")
      done
      if (( ${#libs[@]} )); then step doctests "${TESTENV[@]}" cargo test "${libs[@]}" --doc || status=1; fi
    fi
    if (( splash )); then nextest splash-tests -p splash --no-fail-fast --final-status-level slow || status=1; fi
    if [[ " ${rest[*]:-} " =~ \ (app|agent|listen)\  ]]; then
      nextest public-edition -p app -p agent -p listen --no-default-features --features app/public \
        -E 'test(edition) | test(the_public) | test(the_prompt_s_worked) | test(the_prompt_names) | test(installed_tests) | test(relay_prompt)' || status=1
    fi
  fi
  script_tests || status=1
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

cat "$RUN/times"
grep -h -E '^ +Summary' "$RUN"/*tests.log "$RUN"/public-edition.log 2>/dev/null || true
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

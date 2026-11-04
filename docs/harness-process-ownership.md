# Pi process ownership

Heimdall launches the pinned Pi through its own signed main executable in `--heimdall-guardian` mode. The
first operation in `main` dispatches that mode before app, UI, audio or model initialization; the guardian calls
no AppKit APIs. It does not extract or launch a separate helper. Cargo debug test harnesses use the existing `heimdall-agent-runtime` lab executable because they bypass the app's
`main`; that executable dispatches the same guardian and Rust cleanup. Release bundles contain only the main
executable's linked guardian code. Workspace tests build the lab executable automatically; package-only app/unit
runs need that workspace lab target built first.

When available, the guardian becomes its own macOS process-responsibility root by re-executing the same
main executable with `--heimdall-guardian-root`. It forks Pi and blocks execution until Heimdall has persisted the guardian's PID and
microsecond kernel birth time. It holds no session-lock descriptor after admission. It closes its copies of
Pi's stdin/stdout/stderr, so Pi EOF remains observable. It waits in kqueue for Pi exit or EOF on a private
supervisor-lifetime socket; neither Pi nor tools inherit that socket. After Pi exits it stays asleep until cleanup
completes or Heimdall dies. Supervisor EOF runs the same birth-checked descendant cleanup in the guardian,
commits retirement, then exits; no later Heimdall launch is needed. Pi never assumes it leads a process group. A separate inherited socket
reports Pi exit even if a tool keeps both output pipes open; Pi/tools never inherit the guardian end of that socket.
The same guardian and supervisor-lifetime socket wrap preparation commands; execution and pipe draining share their
original absolute deadline. Before releasing a live Pi, Heimdall installs the cancellation watcher with the startup
request's original permit. Every preparation/metadata/bridge await checks that permit; Off→On cannot replace it.
Preparation's temporary ownership files need no durable recovery receipt. Pi admission permits 15 seconds for
cold signed-executable launch validation; preparation retains its caller's original deadline. Guardian death
is checked throughout the admission wait.

Each generation also has a random `HEIMDALL_AGENT_OWNER` token. Pi's preload puts it back into explicit child
spawn environments. Cleanup enumerates only the current user's processes, matches exact environment tokens through
`KERN_PROCARGS2`, follows the parent tree, and includes processes whose macOS responsibility belongs to the guardian.
Responsibility is needed because this Mac hides protected Apple executables' environments from external inspection,
although those executables still inherit them. `setsid`, double fork, reparenting and environment scrubbing alone
do not escape the living guardian's responsibility tree.

Every STOP/KILL checks the current kernel PID/start-time identity. No process-group number is retained or signalled.
Cleanup freezes owners to prevent further forks, kills descendants, verifies that none remain, then retires the
guardian. Replacement launch cleans the persisted prior generations before releasing any new tool execution.
Old Off watchers and destructors retain only their own generation's authority. Retirement is committed before
killing the guardian. If it dies independently before that receipt exists, cleanup reports that ownership cannot
be verified and blocks replacement; it does not silently assume protected orphan processes are gone.
The one exception is a reboot: a receipt from an earlier boot session (`kern.bootsessionuuid`; a receipt without it:
a guardian born before `kern.boottime`) counts as retired, since the reboot ended everything it owned. A dead guardian
from this boot stays blocked even if the wall clock was stepped.

The native responsibility functions are Apple-private APIs. Both symbols are resolved dynamically, so missing
symbols do not prevent the executable from loading. Missing or failed disclaimer/re-exec, or a failed attribution
check at admission, reduces cleanup to the inherited owner token, parent tree and birth-checked signals. It never
refuses agent execution for this reason. The guardian writes `responsibility.status`; the durable owner receipt
records `cleanup_limited` before releasing execution. Each launched Pi generation writes an
`agent_process_ownership` event to the evidence journal. Heimdall shows one quiet island notice per app process,
"Agent cleanup is limited". The guardian-death verification rule above still applies with reduced coverage.
When attribution is unavailable, protected/reparented executables whose environments macOS hides can escape;
this is reduced cleanup coverage, not evidence that every orphan has been found.

The guardian executable path is exactly the running app's main executable. In the signed candidate this is
`Heimdall.app/Contents/MacOS/Heimdall`, with the app's `dev.heimdall.app` signature identifier, Apple Development
signing identity and team. Its responsibility root therefore has Heimdall's code identity rather than an unknown
helper's identity. Reinstalls keep the existing bundle identifier and designated signing requirement.

**Live TCC behaviour was not exercised.** A candidate signature comparison establishes code identity, not whether
macOS actually reuses each privacy grant. At first installed live use, check that standard agent tools using screen
capture, Automation, microphone and protected folders reuse Heimdall's existing grants, produce the expected result,
and show neither a new helper permission prompt nor a silent permission denial. The lab never invokes those tools,
never launches the user's installed app, never installs a candidate and never changes TCC. Screenshot/audio/UI actions
through Heimdall's existing MCP host keep their existing path.

A process that deliberately both disclaims its responsibility and removes the inherited token after reparenting
can escape these mechanisms. LaunchServices apps started via `open` belong to their own launch tree and must not
be killed. Cleanup does not infer ownership from command-line resemblance. Privileged user changes or a hostile
process outside the owner's user are outside this boundary.

Primary mechanism references:
- https://lldb.llvm.org/cpp_reference/PosixSpawnResponsible_8h_source.html
- https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/sys/proc_info.h
- https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/sys/proc_info_private.h

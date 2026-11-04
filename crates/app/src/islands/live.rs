//! Floating Islands in the live app (night 1022): `start` once at launch (main.rs), `binding_loop` with each agent
//! start (bridge/supervisor.rs, beside the T3 watcher), `shutdown` on the orderly quit (live_loop.rs).
//! Replays, lab modes and tests never call these.

use super::{files::Root, supervisor, watch, Class, Host};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct Running {
    watcher: Option<watch::Watcher>,
    /// The guarantee and proposal sweep's stop flag (`guarantee::run`).
    sweep: Arc<std::sync::atomic::AtomicBool>,
    helper: Option<Arc<supervisor::Supervisor<supervisor::Native>>>,
}
static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// The islands folder for a Heimdall data folder: `FLOATING_ISLANDS_DIR` when set; else the shared default for
/// Heimdall's default data folder; else (a lab run on its own `--data-dir`) a folder inside it, so a lab Heimdall
/// never takes over the user's islands. Returns the folder and the `FLOATING_ISLANDS_DIR` the helper needs.
pub fn root_for(data: &Path, default_data: Option<&Path>) -> (Root, Option<PathBuf>) {
    if std::env::var_os("FLOATING_ISLANDS_DIR").is_some_and(|d| !d.is_empty()) {
        let r = Root::from_env();
        let dir = r.0.clone();
        return (r, Some(dir));
    }
    if default_data.is_some_and(|d| d == data) { return (Root::from_env(), None) }
    let dir = data.join("FloatingIslands");
    (Root(dir.clone()), Some(dir))
}

/// The edition's own islands folder, exported as `FLOATING_ISLANDS_DIR` at start (`home_change::start`) so the
/// helper, the `islands` CLI the agent runs and this process agree: the public edition keeps everything in its data
/// folder (`<data>/floating-islands`) and never shares the personal app's islands. The personal edition: None (the
/// helper's shared folder, as before).
pub fn edition_dir(public: bool, data: &Path) -> Option<PathBuf> {
    public.then(|| data.join("floating-islands"))
}

/// The line's outbox id: its class prefix and a fresh id (each click is its own notice).
pub fn notice_id(class: Class) -> String { format!("{}{}", class.prefix(), uuid::Uuid::new_v4()) }

/// Start the islands host: the watcher always (the agent's tools and the user's clicks need it), the helper when it is
/// bundled and not switched off (`floating_islands_off`); `hidden` is Show Islands off, written before the helper starts.
pub fn start(data: &Path, helper_off: bool, hidden: bool, tools: &Arc<crate::bridge::Tools>) {
    let default = crate::settings::profile_data();
    let (root, dir) = root_for(data, default.as_deref());
    let host = Arc::new(Host::new(root.clone(), Some(data.join("islands-bindings.json"))));
    let weak = Arc::downgrade(tools);
    // A thread's open request is answered on the notch card (binding.rs `card`), so the notch gets its updates.
    let notch = weak.clone();
    host.bindings.set_card_sink(Arc::new(move |update| {
        if let Some(tools) = notch.upgrade() { tools.send_notch(update) }
    }));
    host.set_deliver(Arc::new(move |class, text| {
        let Some(tools) = weak.upgrade() else { return };
        let Ok(actions) = tools.mcp_actions.actions.as_ref() else { eprintln!("islands: not delivered (no action store): {text}"); return };
        let Ok(permit) = tools.agent_enabled.permit() else { eprintln!("islands: not delivered (Agent off): {text}"); return };
        // Like an `ask` click: into the durable outbox, then delivered at once when the agent is attached (the
        // T3 watcher's pass drains it otherwise).
        if let Err(e) = actions.enqueue(&notice_id(class), text, &permit) { eprintln!("islands: not queued: {e:#}"); return }
        if let Err(e) = actions.drain(&permit) { eprintln!("islands: queued, delivery waits: {}", super::short_error(&e)) }
    }));
    if !super::install(host.clone()) { return }
    // The clean slate (`docs/one-system.md`), once, before anything else writes the lists: the old lists
    // into the record, the three lists empty, the old goals proposed again for his yes.
    match super::slate::ensure(&root, crate::islands::tools::now_s() as i64, !cfg!(feature = "public")) {
        Ok(did) => for d in did { eprintln!("Clean slate: {d}") },
        Err(e) => eprintln!("Clean slate: not done (the lists are as they were): {e:#}"),
    }
    // Goals in To do's place before anything draws (no To do list), whether Background Tasks ▸
    // Goals is on or not: the seeds on a first run, todo.json migrated once (`goals::ops::ensure`).
    match crate::goals::ops::ensure(&root, crate::islands::tools::now_s() as i64, !cfg!(feature = "public")) {
        Ok(did) => for d in did { eprintln!("Goals: {d}") },
        Err(e) => eprintln!("Goals: To do not migrated: {e:#}"),
    }
    super::visibility::set(hidden);
    // Show Islands on Streams, in the folder before the helper starts (it reads it at launch).
    super::streams::sync();
    // What waits on the user (the guarantee line) and the two-day archive of proposals, every minute.
    let sweep = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (h, stop) = (host.clone(), sweep.clone());
        if let Err(e) = std::thread::Builder::new().name("islands-sweep".into()).spawn(move || super::guarantee::run(h, stop)) {
            eprintln!("islands: sweep not started: {e}");
        }
    }
    let watcher = match watch::Watcher::spawn(host, Arc::new(super::publish)) {
        Ok(w) => Some(w),
        Err(e) => { eprintln!("islands: watcher not started: {e:#}"); None }
    };
    let helper = if helper_off {
        eprintln!("islands: helper switched off in settings");
        None
    } else {
        match std::env::current_exe().ok().and_then(|exe| supervisor::bundled_helper(&exe)) {
            None => { eprintln!("islands: no bundled helper beside this build; not launching it"); None }
            Some(exe) => {
                let s = supervisor::Supervisor::new(supervisor::Native::new(exe, dir), root);
                let run = s.clone();
                match std::thread::Builder::new().name("islands-helper".into()).spawn(move || run.run()) {
                    Ok(_) => Some(s),
                    Err(e) => { eprintln!("islands: supervisor thread not started: {e}"); None }
                }
            }
        }
    };
    *RUNNING.lock().unwrap_or_else(|e| e.into_inner()) = Some(Running { watcher, sweep, helper });
}

/// The orderly quit: the helper is asked to quit (no relaunch) and the watcher stops.
pub fn shutdown() {
    let Some(mut r) = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    r.sweep.store(true, std::sync::atomic::Ordering::Release);
    if let Some(h) = r.helper.take() { h.quit() }
    if let Some(mut w) = r.watcher.take() { w.stop() }
}

/// The thread-binding loop for one agent start, beside the T3 watcher; ends with `stop`. Nothing when the live
/// host is not running (replays, tests).
pub fn binding_loop(workers: Arc<t3::Workers>, stop: tokio::sync::watch::Receiver<bool>) -> Option<tokio::task::JoinHandle<()>> {
    let host = super::host()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    host.set_t3(Some(tx));
    // Every task Heimdall starts gets its job island before the tool call returns (night 1023).
    let h = host.clone();
    workers.on_task_started(Arc::new(move |thread, title, brief| super::binding::task_island(&h, thread, title, brief)));
    let passes = workers.passes();
    let src: Arc<dyn super::binding::ThreadSource> = Arc::new(super::binding::T3Source::new(workers));
    Some(tokio::spawn(super::binding::run(host, src, passes, stop, rx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lab_data_folder_gets_its_own_islands_and_the_default_shares_owner_s() {
        if std::env::var_os("FLOATING_ISLANDS_DIR").is_some() { return } // the explicit folder wins; covered by Root::from_env
        let default = PathBuf::from("/Users/x/Library/Application Support/Heimdall");
        let (r, dir) = root_for(&default, Some(&default));
        assert!(r.0.ends_with("Library/Application Support/FloatingIslands"));
        assert_eq!(dir, None);
        let lab = PathBuf::from("/tmp/lab-data");
        let (r, dir) = root_for(&lab, Some(&default));
        assert_eq!(r.0, lab.join("FloatingIslands"));
        assert_eq!(dir, Some(lab.join("FloatingIslands")));
    }

    #[test]
    fn the_public_edition_keeps_its_islands_in_its_data_folder() {
        let data = PathBuf::from("/Users/ana/Notes/.heimdall");
        assert_eq!(edition_dir(true, &data), Some(data.join("floating-islands")));
        assert_eq!(edition_dir(false, &data), None);
    }

    #[test]
    fn notice_ids_carry_their_class() {
        assert!(notice_id(Class::Answer).starts_with("islands-answer:"));
        assert!(notice_id(Class::Context).starts_with("islands-context:"));
        assert_ne!(notice_id(Class::Answer), notice_id(Class::Answer));
    }
}

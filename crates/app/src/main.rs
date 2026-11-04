mod audio;
mod api_providers;
mod drama3_verify;
mod edition;
mod friends;
mod archive;
mod embeddings;
mod bridge;
mod bugs;
mod context_policy;
mod health;
mod diagnostics;
mod inference_engine;
mod index;
mod mcp_timeline;
mod background_probe;
// The lab comparison harness reads lab/timeline-diet/ (not in the public source): `--features lab-timeline`.
#[cfg(all(test, feature = "lab-timeline"))]
mod mcp_timeline_live;
mod mcp_frames;
mod mcp_clip;
mod mcp_actions;
mod fast_read;
mod latency;
mod night_runtime;
mod speech_split;
mod night_qa;
mod metrics;
mod pipeline;
mod settings;
mod identity; // Personalise: the name (the wake word too), the user's name and the mark
mod settings_tool; // The agent's settings tool: every menu setting, through the menu's own actions
mod speakers_runtime;
mod voice_bank;
// Speaking ▸ the voice: Local (Drama 3, Supertonic, Phonon S) or API (ElevenLabs) (night 1022 menu).
mod speech_voice;
mod own_voice;
mod watchdog;
mod hang;
mod preflight;
mod drawing;
mod drops;
mod vella; // Vella's dictation state: dictated words never wake the agent (docs/vella.md)
mod lab_run;
mod island_agent;
mod islands;
mod chat_history;
mod plan_refresher;
mod pi_session; // the Pi session as Pi builds context: the plan refresher and the Goals shadow read it
mod goals; // Background Tasks ▸ Goals: the shadow that keeps Goals true (docs/goals.md)
mod voice_namer;
mod voice_suggest;
mod suggester; // the in-conversation suggester: one card, Haiku, the log
mod suggester_replay; // its offline replay and label export (lab)
mod background_model; // Background Tasks ▸ Model and its toggles
mod background_fallback; // a background job moves to the next backend while its pick is spent or down
mod background_gate; // Agent off stops every background model request
mod background_call; // the one Messages API path of every background job: prompt cache, $0.50 a run, spend
mod day_timeline; // Background Tasks ▸ Day Timeline: the left island of where the day went
mod setup_wizard; // Setup's actions: first launch, home folder, providers, T3 Code consent and install
mod emergency_recovery; // The recovery cards: what can take over when the agent's account fails, and the offer to go back
mod emergency_local; // Nothing can cover: the island offers the local engine (docs/claude-relay.md)
mod relay_catalog; // Agent ▸ Model ▸ Claude Code subscription's models and accounts, from T3's live catalog
mod emergency; // Emergency mode: the agent failed, a T3 thread covers (docs/emergency.md)
mod harness_autonomy; // every agent harness back in its no-prompt mode at each start (docs/harness-autonomy.md)
mod resource_watch; // swap past 2 GB tells the agent once; GPU time per part of Heimdall in a `gpu:` line
mod home_change; // the home folder: fixed at start, changed by the setup wizard (docs/home.md)
mod updates; // in-app updates, public edition only (docs/updates.md)
#[cfg(test)]
mod serde_order_tests {
    /// The app's JSON objects keep sorted keys: the decider's questions and tool results are byte-for-byte goldens.
    /// Fails when serde_json's preserve_order reaches this build (Splash turns it on): build and test Splash on its own.
    #[test]
    fn json_objects_keep_sorted_keys_in_the_apps_build() {
        let v = serde_json::json!({"b": 1, "a": 2});
        assert_eq!(v.to_string(), r#"{"a":2,"b":1}"#, "serde_json preserve_order leaked into the app's build");
    }
}
use anyhow::{bail, ensure, Context, Result};
use crossbeam_channel::{bounded, select, unbounded};
use settings::Settings;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};


mod live_loop;
mod vella_asr; // Vella borrows the loaded Parakeet Ultra over a Unix socket (docs/vella.md, Channel 2)
mod replay_modes;
use self::{live_loop::*, replay_modes::*};

#[derive(Default)]
struct Args {
    replay: Option<PathBuf>,
    ui: bool,
    models: Option<PathBuf>,
    data: Option<PathBuf>,
    idle: Option<u64>,
    source: String,
    settings_only: bool,
    set_dry: Option<bool>,
    format: Option<settings::ArchiveFormat>,
    retention: Option<u32>,
    floor: Option<u64>,
    realtime: bool,
    streams: usize,
    macmon: Option<PathBuf>,
    hotswap_smoke: bool,
    night_replay_smoke: bool,
    night_lab_worker: bool,
    lab_scenario: Option<PathBuf>,
}
fn args() -> Result<Args> {
    let mut a = Args {
        source: "mic".into(),
        streams: 1,
        ..Default::default()
    };
    let mut it = std::env::args().skip(1);
    while let Some(v) = it.next() {
        match v.as_str() {
            "--night-replay-smoke" => a.night_replay_smoke = true,
            "--night-lab-worker" => a.night_lab_worker = true,
            "--hotswap-smoke" => a.hotswap_smoke = true,
            "--replay" => {
                a.replay = Some(it.next().context("--replay needs WAV or directory")?.into())
            }
            "--lab-scenario" => {
                a.lab_scenario = Some(it.next().context("--lab-scenario needs a scenario file")?.into())
            }
            "--ui" => a.ui = true,
            "--models" => a.models = Some(it.next().context("--models needs directory")?.into()),
            "--data-dir" => a.data = Some(it.next().context("--data-dir needs directory")?.into()),
            "--idle-seconds" => {
                a.idle = Some(it.next().context("--idle-seconds needs value")?.parse()?)
            }
            "--realtime" => a.realtime = true,
            "--streams" => {
                a.streams = it.next().context("streams missing")?.parse()?;
                ensure!((1..=8).contains(&a.streams), "streams must be 1..8");
            }
            "--macmon" => a.macmon = Some(it.next().context("macmon path missing")?.into()),
            "--source" => {
                a.source = it.next().context("--source needs mic, discord or app")?;
                ensure!(
                    matches!(a.source.as_str(), "mic" | "discord" | "app" | "mixed"),
                    "invalid fake source"
                );
            }
            "--set-dry-run" => {
                a.set_dry = Some(it.next().context("dry-run needs true/false")?.parse()?);
                a.settings_only = true;
            }
            "--archive-format" => {
                a.format = Some(
                    match it.next().context("archive format missing")?.as_str() {
                        "native_lossless" => settings::ArchiveFormat::NativeLossless,
                        "flac24" => settings::ArchiveFormat::Flac24,
                        "flac16k_mono" => settings::ArchiveFormat::Flac16kMono,
                        _ => bail!("unknown archive format"),
                    },
                );
                a.settings_only = true;
            }
            "--retention-days" => {
                a.retention = Some(it.next().context("retention missing")?.parse()?);
                a.settings_only = true;
            }
            "--free-space-floor-gib" => {
                a.floor = Some(
                    it.next()
                        .context("floor missing")?
                        .parse::<u64>()?
                        .checked_mul(1073741824)
                        .context("floor overflow")?,
                );
                a.settings_only = true;
            }
            "--help" => {
                println!("Heimdall: menu-bar app (dry-run by default).\n  --replay WAV_OR_DIR [--source mic|discord|app|mixed] [--ui] [--realtime] [--streams 1..8] [--macmon PATH] [--data-dir DIR]\n  --idle-seconds 10..60 [--data-dir DIR] (no capture or permissions)\n  --models DIR\n  --set-dry-run true|false | --archive-format native_lossless|flac24|flac16k_mono\n  --retention-days DAYS (0=keep) | --free-space-floor-gib N (minimum 1)\nReplay always forces dry-run and an isolated data directory.");
                std::process::exit(0);
            }
            _ => bail!("unknown argument: {v}"),
        }
    }
    ensure!(
        !(a.replay.is_some() && a.idle.is_some()),
        "replay and idle are separate modes"
    );
    Ok(a)
}
fn models() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let bundled = exe
        .parent()
        .context("binary parent")?
        .join("../Resources/models");
    if bundled.is_dir() {
        Ok(bundled)
    } else {
        let local = PathBuf::from("models");
        ensure!(local.is_dir(), "Runtime models unavailable; use --models for an offline build");
        Ok(local)
    }
}
fn main() -> Result<()> {
    if let Some(status) = agent::guardian_mode() { std::process::exit(status); }
    // This edition's pointer and profile, for this process and every child (docs/home.md); still single-threaded.
    std::env::set_var("HEIMDALL_BUNDLE_ID", edition::BUNDLE_ID);
    diagnostics::raise_open_file_limit();
    // Pi's per-request key command for a Console sign-in: prints the current token and exits, nothing else.
    if let Some(status) = agent::console::token_mode() { std::process::exit(status); }
    if night_runtime::child_mode()? { return Ok(()); }
    // Scheduled wakes from local agents outside the app (agent::wakes, selfwake night 21 Oct): writes one wake into the
    // data dir's schedule, prints one line and exits; the running app fires it within one heartbeat (docs/mcp.md).
    if std::env::args().nth(1).as_deref() == Some("--schedule-wake") {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        match agent::wakes::cli(&rest, settings::default_data) {
            Ok(line) => { println!("{line}"); return Ok(()); }
            Err(e) => { eprintln!("{e:#}"); std::process::exit(2); }
        }
    }
    // The taste ledger from the command line (agent::taste): reviews of a date range, adding entries, search.
    if std::env::args().nth(1).as_deref() == Some("--taste") {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        match agent::taste::cli(&rest, settings::default_data, &mut std::io::stdin()) {
            Ok(text) => { println!("{text}"); return Ok(()); }
            Err(e) => { eprintln!("{e:#}"); std::process::exit(2); }
        }
    }
    // The in-conversation suggester offline (lab): a replay over archived talk (read-only archive text in,
    // a JSON file and an HTML page out), and the decider's labels exported from the suggestion log.
    if std::env::args().nth(1).as_deref() == Some("--suggest-replay") {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        match suggester_replay::cli(&rest) {
            Ok(line) => { println!("{line}"); return Ok(()); }
            Err(e) => { eprintln!("{e:#}"); std::process::exit(2); }
        }
    }
    // One small real call on the background model (background_probe.rs).
    if std::env::args().nth(1).as_deref() == Some("--background-probe") {
        let raw = std::env::args().nth(2).as_deref() == Some("raw");
        match settings::default_data().and_then(|d| if raw { background_probe::raw(&d) } else { background_probe::run(&d) }) {
            Ok(line) => { println!("{line}"); return Ok(()); }
            Err(e) => { eprintln!("background probe: {e:#}"); std::process::exit(2); }
        }
    }
    if std::env::args().nth(1).as_deref() == Some("--suggest-export") {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        match suggester_replay::export_cli(&rest, settings::default_data) {
            Ok(line) => { println!("{line}"); return Ok(()); }
            Err(e) => { eprintln!("{e:#}"); std::process::exit(2); }
        }
    }
    if std::env::current_exe()?.file_stem().and_then(|s| s.to_str()) == Some("speech-split-replay") { return pipeline::speech_split::replay(); }
    let a = args()?;
    // The app on a profile: a home move asked for last run happens now, before anything opens the data folder, and
    // the folders go to every child. Offline modes keep their isolated folders and never move anything.
    let live_app = a.replay.is_none() && a.idle.is_none() && !a.settings_only && !a.hotswap_smoke && !a.night_replay_smoke
        && !a.night_lab_worker && a.lab_scenario.is_none();
    let home_moved = if live_app { home_change::start(a.data.as_deref()) } else { None };
    // The post-install check's lab instance (lab/postinstall/): the real UI driven by a scenario on an isolated
    // profile; nothing else starts.
    if let Some(file) = a.lab_scenario.clone() {
        ensure!(a.data.is_some(), "--lab-scenario needs an explicit isolated --data-dir");
        let data = isolated_data(a.data, "lab")?;
        return lab_run::run(&file, data);
    }
    diagnostics::startup_record(&a)?;
    if live_app {
        home_change::log_start(home_moved);
        // The install tool waits for this launch's readiness (public edition from its bundle; a no-op otherwise).
        updates::report(heimdall_update::readiness::Phase::Starting, "");
    }
    let model_root = a.models.clone().map(Ok).unwrap_or_else(models)?;
    if a.night_replay_smoke || a.night_lab_worker {
        ensure!(a.data.is_some(), "night diagnostic requires an explicit isolated --data-dir");
        let data=isolated_data(a.data,"night")?;
        let wav=a.replay.context("night diagnostic needs --replay WAV")?;
        return if a.night_lab_worker {night_qa::worker(&data,&model_root,&wav)} else {night_qa::replay(data,model_root,&wav)};
    }
    if a.hotswap_smoke {
        ensure!(a.data.is_some(), "hot-swap smoke needs an isolated --data-dir");
        let data=isolated_data(a.data,"hotswap")?;
        return match a.replay { Some(wav)=>hotswap_replay_smoke(data,model_root,&wav), None=>hotswap_smoke(data,model_root) };
    }
    if a.settings_only {
        ensure!(
            a.replay.is_none() && a.idle.is_none(),
            "settings changes cannot be combined with replay/idle"
        );
        let data = a.data.unwrap_or(settings::default_data()?);
        let path = data.join("settings.json");
        let mut s = Settings::load(&path)?;
        if let Some(b) = a.set_dry {
            s.dry_run = b;
        }
        if let Some(f) = a.format {
            s.archive_format = f;
            s.archive_choice_pending = false;
        }
        if let Some(d) = a.retention {
            s.retention_days = d;
        }
        if let Some(n) = a.floor {
            ensure!(n >= 1073741824, "floor must be at least 1 GiB");
            s.free_space_floor_bytes = n;
        }
        s.normalize_archive();
        s.save(&path)?;
        println!("Settings saved; the running app applies valid edits live. dry_run={} archive={:?} choice_pending={} retention_days={} floor_bytes={}",s.dry_run,s.archive_format,s.archive_choice_pending,s.retention_days,s.free_space_floor_bytes);
        return Ok(());
    }
    if let Some(secs) = a.idle {
        return idle(secs, a.data, model_root);
    }
    if let Some(p) = a.replay {
        let data = isolated_data(a.data, "replay")?;
        if a.ui {
            let u = ui::Ui::handle();
            std::thread::spawn(move || {
                match replay(
                    &p,
                    &data,
                    &model_root,
                    &a.source,
                    Some(u),
                    a.realtime,
                    a.streams,
                    a.macmon.as_deref(),
                ) {
                    Ok(()) => {}
                    Err(e) => eprintln!("replay failed: {e:#}"),
                }
                u.send(ui::UiUpdate::Terminate);
            });
            ui::run(
                ui::Options {
                    fake_permissions: true,
                    ..Default::default()
                },
                |_| {},
            );
        }
        return replay(
            &p,
            &data,
            &model_root,
            &a.source,
            None,
            a.realtime,
            a.streams,
            a.macmon.as_deref(),
        );
    }
    let data = a.data.unwrap_or(settings::default_data()?);
    let settings = Settings::load_and_migrate(&data.join("settings.json"))?;
    // The names before anything reads them: the decider, the agent's first prompt, the recognizer, the UI.
    identity::publish(&settings.identity);
    // The agent runtime now ships inside the app; earlier builds' downloaded copies go once.
    match agent::runtime::retire_downloaded(&data) {
        Ok((removed, bytes)) if !removed.is_empty() => eprintln!("Removed the downloaded agent runtime ({} MB): {}", bytes / 1_000_000, removed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")),
        Ok(_) => {}
        Err(e) => eprintln!("Could not remove the downloaded agent runtime: {e:#}"),
    }
    let u = ui::Ui::handle();
    // ---- Night 1022 [notch]: a floating island that newly needs the user nudges the notch (the host's watcher publishes
    // `IslandsEvent::Changed`; islands/nudge.rs diffs it).
    islands::nudge::listen(move |question| u.send(ui::UiUpdate::FloatingNudge { question }));
    // ---- end night 1022 [notch]
    // The conversation the pulled-down notch shows, read from the agent's session file and the
    // archive's transcript on its own thread.
    chat_history::start(&data, u);
    // Friends (personal edition): the node starts on its own thread; nothing waits for it.
    friends::start(&data, u);
    let (tx, rx) = unbounded();
    // Share the exact tools before starting the pipeline: Off cannot wait for it.
    let tools = Arc::new(bridge::Tools::new(&model_root, &data));
    tools.initialize_delivery(&settings);
    // In-app updates (public edition only): an interrupted update repaired, the menu row, the launch check.
    updates::launch(u, &tools);
    // Floating islands (night 1022): the watcher, the agent's island tools and the bundled helper's supervisor.
    // Show Islands on Streams, known before the host writes the helper's streams.json and launches it.
    islands::streams::set(settings.islands_on_streams);
    islands::live::start(&data, settings.floating_islands_off, settings.islands_hidden, &tools);
    // A first provider connection selects its default model through the same queue as the user's clicks.
    tools.api.set_actions(tx.clone());
    // Swap past 2 GB tells the agent once; the GPU time of each part of Heimdall, every 10 busy minutes.
    resource_watch::start(tools.clone());
    // Claude Code, T3, Codex, OpenCode and Grok in their no-prompt modes; whatever an update changed is set back.
    harness_autonomy::start(&data, settings.harness_autonomy, true);
    let pipeline_tools = tools.clone();
    // The top of the recovery ladder: a real freeze of the live loop or the UI thread relaunches Heimdall, and Quit
    // always ends (watchdog.rs).
    let beats = Arc::new(watchdog::Beats::default());
    watchdog::spawn(
        beats.clone(),
        move |b: Arc<watchdog::Beats>| u.on_main(move || b.ui_beat()),
        watchdog::Relauncher { ui: u, data: data.clone(), tools: tools.clone(), showing_problem: false },
    )?;
    // Stacks of a UI thread or live loop silent past 2 s, in logs/hangs/ (hang.rs): evidence, not recovery.
    if let Err(e) = hang::spawn(beats.clone(), move |f| u.on_main(f), data.join("logs")) {
        eprintln!("Hang sampler did not start: {e}");
    }
    let error_tools = tools.clone();
    let (live_beats, error_data, callback_data) = (beats.clone(), data.clone(), data.clone());
    std::thread::Builder::new()
        .name("heimdall-app".into())
        .spawn(move || {
            if let Err(e) = live(data, model_root, settings, u, rx, pipeline_tools, live_beats.clone()) {
                eprintln!("Heimdall stopped: {e:#}");
                if let Err(log) = watchdog::RecoveryLog::record_error(&error_data, &format!("{e:#}")) {
                    eprintln!("Could not save Heimdall error: {log:#}");
                }
                // Quit asked to end: a teardown error is logged, never answered with a relaunch.
                if live_beats.is_quitting() { return; }
                u.send(ui::UiUpdate::Listen(ui::ListenState::Failed(e.to_string())));
                stopped_on_error(&error_data, &error_tools, u, &format!("{e:#}"));
            }
        })?;
    // The Agent window shows attachment images from the archive by id (read off the main thread).
    let archive_root = callback_data.join("archive");
    ui::set_attachment_loader(move |id| {
        let db = heimdall_archive::store::open(&archive_root).ok()?;
        heimdall_archive::records::image_bytes(&db, &archive_root, id).ok().map(|(_, bytes)| bytes)
    });
    // Drawing mode's Send captures through Heimdall's own Screen Recording path; the system profile lives with the
    // rest of Heimdall's data (measured on a build's first launch).
    drawing::install_grabber(tools.clone());
    // The agent's `settings` tool changes a setting through the menu's own callback, as a click (settings_tool.rs).
    *tools.mcp_actions.menu.lock().unwrap() = Some(Arc::new(ui_action_callback(tools.clone(), tx.clone(), u)));
    let options = ui::Options { system_profile: Some(callback_data.join("system.json")), friends: friends::ENABLED, ..ui::Options::default() };
    ui::run(options, live_action_callback(tools, tx, u, beats, callback_data));
}
/// The live loop ended on an error: nothing captures or answers any more, though the menu still works. The same
/// rung as a freeze: relaunch within the bound, else a red pill that relaunches on click. A damaged archive store
/// is never relaunched into (the next launch opens the same file): the pill says so and shows the archive.
fn stopped_on_error(data: &Path, tools: &bridge::Tools, u: ui::Ui, error: &str) {
    tools.record_diagnostic("health", serde_json::json!({"component":"heimdall", "state":"failed", "reason":error}));
    if pipeline::archive_damaged(error) {
        u.send(ui::UiUpdate::Health(ui::HealthCue::Problem {
            key: "disk".into(),
            part: ui::Part::Archive,
            label: "Archive database is damaged".into(),
            action: "Click to show it in Finder".into(),
            control: ui::HealthControl::ShowArchive,
        }));
    } else if watchdog::RecoveryLog::load(data).is_ok_and(|log| log.allows(watchdog::unix_now())) {
        u.send(ui::UiUpdate::Health(ui::HealthCue::Restarting { key: "heimdall".into(), part: ui::Part::Heimdall, label: "Restarting Heimdall…".into() }));
        if watchdog::relaunch_now(data, "the live loop ended on an error", true, tools).is_ok() { return; }
        u.send(ui::UiUpdate::Health(ui::HealthCue::Problem {
            key: "heimdall".into(), part: ui::Part::Heimdall, label: "Heimdall stopped".into(),
            action: "Click to restart it".into(), control: ui::HealthControl::Relaunch,
        }));
    } else {
        u.send(ui::UiUpdate::Health(ui::HealthCue::Problem {
            key: "heimdall".into(),
            part: ui::Part::Heimdall,
            label: "Heimdall stopped".into(),
            action: "Click to restart it".into(),
            control: ui::HealthControl::Relaunch,
        }));
    }
}
/// The live app's callback: the red pill's restart and the Quit watchdog, then `ui_action_callback`.
fn live_action_callback(
    tools: Arc<bridge::Tools>,
    tx: crossbeam_channel::Sender<ui::UiAction>,
    u: ui::Ui,
    beats: Arc<watchdog::Beats>,
    data: PathBuf,
) -> impl Fn(ui::UiAction) {
    let recovery_tools = tools.clone();
    let inner = ui_action_callback(tools, tx, u);
    move |action| {
        // The user clicked "restart" on the red pill: relaunch whatever the automatic bound says, through a normal
        // Quit (the archive and deliveries drain), which the watchdog bounds even if the live loop is stuck.
        let action = match action {
            ui::UiAction::Health { control: ui::HealthControl::Relaunch, .. } => {
                if let Err(e) = watchdog::start_relaunch(&data, "User clicked restart", false, &recovery_tools)
                {
                    eprintln!("Relaunch helper failed: {e:#}");
                    return;
                }
                ui::UiAction::Quit
            }
            // Shown here, not by the live loop: the damaged-archive pill also appears after the loop has ended.
            ui::UiAction::Health { control: ui::HealthControl::ShowArchive, .. } => {
                if let Err(e) = std::process::Command::new("/usr/bin/open").arg(data.join("archive")).spawn() {
                    eprintln!("Show archive failed: {e:#}");
                }
                return;
            }
            other => other,
        };
        if matches!(action, ui::UiAction::Quit) {
            beats.quitting();
            // A compaction step in flight holds the archive locks every teardown write waits for: it stops now.
            heimdall_archive::compact::preempt_all();
        }
        inner(action)
    }
}
const AGENT_OFF: &str = "Agent is off";

/// A window or island action the app could not take, in the user's words (the error goes to the log).
fn refusal(e: &anyhow::Error) -> &'static str {
    if e.downcast_ref::<agent::delivery::DeliveryBlocked>().is_some() { AGENT_OFF } else { "The agent isn't running" }
}

/// Called synchronously by the native menu selectors, before settings are queued.
fn ui_action_callback(
    tools: Arc<bridge::Tools>,
    tx: crossbeam_channel::Sender<ui::UiAction>,
    u: ui::Ui,
) -> impl Fn(ui::UiAction) {
    move |action| {
        // Intent is synchronous; applying this or any older settings action
        // later cannot change it. Every On has a newer latch generation.
        use agent::delivery::StopReason;
        match action {
            // Friends' cards answer here, not on the agent's path (crates/app/src/friends.rs).
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id, answers, .. }) if friends::owns(&request_id) => {
                // A friends card has one question: its answer is the label the user picked.
                let choice = answers.first().and_then(|a| a.labels.first()).cloned().unwrap_or_default();
                friends::answer(&request_id, &choice);
                return;
            },
            ui::UiAction::InviteFriend => {
                friends::ask_name();
                return;
            },
            ui::UiAction::InviteFriendNamed { name } => {
                friends::invite(&name);
                return;
            },
            ui::UiAction::RevokeFriend { id } => {
                friends::revoke(&id);
                return;
            },
            ui::UiAction::FriendReply { card_id, text } => {
                friends::reply(&card_id, &text);
                return;
            },
            ui::UiAction::AgentWindow(action) => {
                if let Err(e)=tools.window_action(action){
                    eprintln!("Agent window action refused: {e:#}");
                    bridge::window_problem(u,refusal(&e).into());
                }
                return;
            },
            ui::UiAction::AgentNotice(action) => {
                let connect = matches!(action, ui::agent_notice::AgentNoticeAction::ConnectT3 | ui::agent_notice::AgentNoticeAction::ConnectT3Notice { .. });
                let ids = match &action {
                    ui::agent_notice::AgentNoticeAction::Answer { request_id, .. } => vec![request_id.clone()],
                    ui::agent_notice::AgentNoticeAction::ConnectT3Notice { id } => vec![id.clone()],
                    _ => tools.mcp_actions.workers.lock().unwrap().as_ref().map(|w|bridge::connection_notice_ids(&w.client)).unwrap_or_else(||vec!["t3-connect-expiry:unconnected".into(),"t3-connect-401:unconnected".into()]),
                };
                if let Err(e)=tools.notice_action(action){
                    // The island shows a calm line the card understands; the error itself goes to the log.
                    eprintln!("Agent notice action refused: {e:#}");
                    use ui::agent_notice::NoticeOutcome as O;
                    let outcome = match (connect, refusal(&e)) {
                        (true, AGENT_OFF) => O::T3NeedsAgent,
                        (true, _) => O::T3ConnectionFailed,
                        (false, _) => O::QuestionUnavailable,
                    };
                    bridge::notice_outcomes(u, ids, outcome);
                }
                return;
            },
            ui::UiAction::FastRead(result) => {
                fast_read::finish(tools.clone(), result);
                return;
            },
            ui::UiAction::UpdateRow => {
                updates::row_clicked();
                return;
            },
            ui::UiAction::InstallUpdate => {
                updates::install_clicked();
                return;
            },
            ui::UiAction::SetDryRun(off) => { tools.menu_intent(StopReason::Agent, !off); },
            ui::UiAction::SetListening(on) => { tools.menu_intent(StopReason::Heimdall, on); },
            // Setup's Allow All turns Heimdall on at the click, before its permission dialogs: an Off clicked
            // while they are up is newer. Their completion (RequestSystemAudio) carries no intent.
            ui::UiAction::SetupAllowed => { tools.menu_intent(StopReason::Heimdall, true); },
            ui::UiAction::Quit => { tools.agent_enabled.intent(StopReason::Quit, false); },
            _ => {},
        }
        let quit = matches!(action, ui::UiAction::Quit);
        if tx.send(action).is_err() && quit {
            u.send(ui::UiUpdate::Terminate);
        }
    }
}
#[cfg(test)]
#[path = "main_tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "main_tests/permission_transition_tests.rs"]
mod permission_transition_tests;

#[cfg(test)]
#[path = "main_tests/immediate_off_tests.rs"]
mod immediate_off_tests;

#[cfg(test)]
#[path = "main_tests/stop_interleaving_tests.rs"]
mod stop_interleaving_tests;

#[cfg(test)]
#[path = "main_tests/settings_intent_tests.rs"]
mod settings_intent_tests;

#[cfg(test)]
#[path = "main_tests/agent_ui_tests.rs"]
mod agent_ui_tests;

#[cfg(test)]
#[path = "main_tests/quit_capture_tests.rs"]
mod quit_capture_tests;

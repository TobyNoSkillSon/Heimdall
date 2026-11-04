//! `heimdall-ui-demo`: the UI with fake data, looks only. Nothing is captured, nothing asks for a permission.
//!
//! Without a flag (or with `--help`) it prints what it can do and exits: nothing ever opens on screen unless asked.
//! `heimdall-ui-demo --live` runs it live on screen: the menu-bar item, the transcript filling in, one new-speaker
//! prompt (the user's screen; never run it from an agent).
//! `heimdall-ui-demo --render <dir>` writes PNGs of the menu and every window (ordered in at alpha 0), then quits.
//! `heimdall-ui-demo --checks <dir>` only runs the in-process menu checks (exit 1 on a failure).
//! `heimdall-ui-demo --island <dir>` checks the island offscreen: stills of every card's states, its windows, clicks
//! through its real event path, its cost at rest (exit 1 on a failure).
//! `heimdall-ui-demo --motion <dir>` plays every island card at 120 Hz offscreen and judges every frame (symmetry,
//! clipping, smoothness, rest), with the main thread's cost per frame, to `<dir>/anims/judge.json` (exit 1 on a
//! finding).
//! `heimdall-ui-demo --sheet <dir> (--seed N | --grid a,b,phase | --card name)` renders a contact sheet of a simulated
//! island sequence's key frames (for a failing random or grid sequence).
//! `heimdall-ui-demo --drawing <dir>` checks drawing mode and pointing in process (window, events, Send by a tap,
//! a Send end to end with a stand-in capture, the light and marks on the island's clock) and writes `draw-*.png`
//! (exit 1 on a failure).
//! `heimdall-ui-demo --agent <dir>` draws the Agent window in every run state and the island's questions offscreen,
//! and drives their controls in-process (exit 1 on a failure).
//! `heimdall-ui-demo --gallery <dir> [--only name,name] [--fps 120]` renders every island card
//! (`crates/ui/src/island/object/gallery.rs`) natively, frame by frame, into `<dir>/anims/` for
//! `<dir>/gallery.html`, judging every frame (exit 1 on a failure). `lab/animation-gallery/README.md` is the guide.
//! `heimdall-ui-demo --offscreen <dir> [--icns <file>]` draws the menu-bar helmet, the app icon (and builds its
//! `.icns`), the tile row with Listening on and off and the Recording submenu, offscreen; exit 1 on a failure.
//!
//! Every flag but `--live` keeps everything off screen.

use std::cell::RefCell;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use ui::*;

fn words(text: &str) -> Segment {
    Segment::Words(text.to_string())
}
fn note(text: &str) -> Segment {
    Segment::Note(text.to_string())
}
fn line(id: u64, source: &str, speaker: &str, segments: Vec<Segment>, is_final: bool) -> TranscriptLine {
    TranscriptLine {
        id,
        label: Some(Label { source: source.into(), speaker: speaker.into() }),
        segments,
        is_final,
    }
}

fn sources() -> Vec<Source> {
    let source = |id: &str, kind, name: &str, enabled| Source { id: id.into(), kind, name: name.into(), enabled };
    vec![
        source("mic", SourceKind::Mic, "MacBook Pro Microphone", true),
        source("discord", SourceKind::App, "Discord", true),
    ]
}

fn speakers() -> Vec<KnownSpeaker> {
    let known = |id: &str, name: &str, heard_in: &str, clips, last: &str| KnownSpeaker {
        id: id.into(),
        name: name.into(),
        heard_in: heard_in.into(),
        clips,
        last_heard: last.into(),
        ignored: false,
        listen: Listen::Off,
    };
    vec![
        KnownSpeaker { listen: Listen::Always, ..known("owner", "User", "Mic", 212, "today 14:03") },
        KnownSpeaker { listen: Listen::On, ..known("v2", "Vim", "Discord", 14, "today 14:02") },
        known("v4", "Dave", "Discord", 9, "today 13:40"),
        known("v3", "Erin", "Discord", 6, "yesterday 21:40"),
        KnownSpeaker { ignored: true, listen: Listen::None, ..known("ignored-1-1", "\u{201c}Subscribe for more\u{201d}", "Discord", 1, "21 Oct") },
    ]
}

fn transcript() -> Vec<TranscriptLine> {
    vec![
        line(1, "Discord", "Dave", vec![words("Okay, the build is green on my side. Did you push the audio fix?")], true),
        line(
            2,
            "Mic",
            "User",
            vec![
                words("Not yet. Look at this"),
                note("screenshot 14:02:11 attached"),
                words("the meter jumps every time Discord ducks the music."),
            ],
            true,
        ),
        line(
            3,
            "Chrome",
            "video",
            vec![words("…and that is why the sample-rate conversion has to happen before the ring buffer, not after it.")],
            true,
        ),
        line(4, "Discord", "Erin", vec![words("Is that the Rust one or the old Swift helper?")], true),
        line(5, "Mic", "User", vec![words("Dobra, zobacz to"), note("screenshot 14:02:40 attached"), note("sent to Pi")], true),
        TranscriptLine { id: 6, label: None, segments: vec![note("Pi woken")], is_final: true },
        // Speech started but nothing was recognised: draws nothing.
        line(8, "MacBook Pro Microphone", "User", vec![words("")], false),
        TranscriptLine { id: 9, label: None, segments: vec![note("AirPods Pro disconnected · 14:03")], is_final: true },
    ]
}

fn permissions() -> Vec<(Permission, PermissionState)> {
    vec![
        (Permission::Microphone, PermissionState::Granted),
        (Permission::SystemAudio, PermissionState::Unconfirmed),
        (Permission::ScreenRecording, PermissionState::Denied),
        (Permission::Accessibility, PermissionState::NotDetermined),
    ]
}

/// The floating name entry (`UiUpdate::VoiceNameEntry`).
fn name_entry() -> UiUpdate {
    UiUpdate::VoiceNameEntry { code: "Speaker Fri 23 Oct 21h #3".into(), quote: "Can someone share the link to the branch again?".into() }
}

const HELP: &str = "heimdall-ui-demo: Heimdall's UI with fake data, offscreen unless --live.
  --island <dir>        the island: stills of every state, windows, clicks, cost at rest
  --island-loops <dir>  the thinking dots and the spinner: stills through their cycle, commits a second while they loop
  --motion <dir>        every island card at 120 Hz, judged; frame costs (anims/judge.json)
  --gallery <dir> [--only a,b] [--fps n]   render the animation gallery into <dir>/anims
  --sheet <dir> (--seed N | --grid a,b,phase | --card name)   key frames of a simulated sequence
  --drawing <dir>       drawing mode and pointing, in process
  --drop <dir>          the drop target at the notch, in process, offscreen
  --drop-live [secs]    the real drop target over the notch for real drags (default 60 s); drops printed only
  --drawing-over-menu <other|own> [secs]   live: drawing mode over an open menu (default 3 s); each change printed
  --streams <dir>       Show Islands on Streams: the island windows' sharing, and a stand-in captured by screencapture
  --submenus-left <dir> LIVE: the real menu on screen ~1 s per scenario, submenus opened left (no events)
  --menu-sections-live <dir>  LIVE: the whole menu a few seconds, sections opened and folded by code, each change on real frames
  --menu-live <dir>     LIVE: the real menu on screen a few seconds: a row's expansion frame by frame, in-menu clicks keep it open
  --agent <dir>         the Agent window and the island's questions, in process
  --chrome <dir>        the menu's chrome and transcript naming, in process
  --menu-rows <dir>     the menu's rows under synthetic pointer events, and models added while it is open
  --claude-relay <dir>  Background Tasks paused in emergency mode and Agent ▸ Model ▸ Claude Code subscription, with checks
  --model-choice <dir>  Agent ▸ Model: click then Confirm, the relay's T3 models, Add Provider ▸ Local Splash Engine, and the click audit (only deliberate exits close a menu)
  --engine-models <dir> Engine ▸ the quick-pick and Add Model: every Splash model and precision, download, trash
  --engine-section <dir> Engine section in every state, the Engine tile, This Mac in Agent ▸ Model, folding sections
  --menu-climb <dir>      The whole menu in every state the UX climb judges (lab/climb/menu-ux)
  --decider-section <dir> Decider section: precision pick and Confirm, swapping, failed, missing folder; Setup's Decider step
  --relay-account <dir> Agent ▸ Model ▸ Claude Code subscription's accounts: Automatic and each T3 Claude instance, click then Confirm
  --menu-bench          times the menu's sync with 30 apps, before and after its cache
  --composer <dir>      the island chat's composer and ⌃⌘H: one placeholder, typing, focus, renders
  --pulldown-drop <dir> a drop on the open pull-down: attached while typing, sent with Return, renders
  --bug-notes <dir>     the Bug Notes window's cards in every state, offscreen, and a click on an evidence row
  --friends <dir> <qr.png>  the Friends submenu and the invite panel (with the QR's PNG), offscreen
  --render <dir>        PNGs of the menu and every window (alpha 0), then quit
  --checks <dir>        the in-process menu checks
  --offscreen <dir> [--icns <file>]   the helmet, app icon and tile row
  --speaking <dir>      the Speaking section's voice rows and submenus, offscreen
  --personalise <dir>   Personalise: the menu, its submenu and the menu-bar mark for Heimdall and Ivy, offscreen, checked
  --forms <dir>         the connect panels (key, sign-in, OpenAI-compatible, own engine), offscreen
  --background-picker <dir>  Background Tasks ▸ Model: providers open in place to models and levels, T3 instances
  --wizard <dir>        Setup: every step and state (permissions, home, providers, T3 Code), offscreen
  --memory <dir>        Memory ▸ and the menu with its Memory row, offscreen
  --live                run the live demo on screen (User's screen: never from an agent)";

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let mut render_dir = None;
    let mut render_checks_only = false;
    let mut live_demo = false;
    if args.peek().is_none() {
        println!("{HELP}");
        return;
    }
    while let Some(arg) = args.next() {
        let dir = |args: &mut std::iter::Peekable<std::iter::Skip<std::env::Args>>, flag: &str| -> PathBuf {
            args.next().map(PathBuf::from).unwrap_or_else(|| {
                eprintln!("{flag} needs a directory\n\n{HELP}");
                std::process::exit(2)
            })
        };
        let finish = |failures: Vec<String>| -> ! {
            for f in &failures {
                eprintln!("FAILED: {f}");
            }
            std::process::exit(if failures.is_empty() { 0 } else { 1 })
        };
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return;
            }
            "--live" => live_demo = true,
            "--bench-chat" | "--bench-layout" => {
                let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(400);
                print!("{}", ui::bench_chat(n, arg == "--bench-layout"));
                std::process::exit(0);
            }
            "--sheet" => {
                let d = dir(&mut args, "--sheet");
                let what = args.next().unwrap_or_default();
                let arg = args.next().unwrap_or_default();
                match ui::render_sheet(&d, &what, &arg) {
                    Ok(path) => {
                        println!("{}", path.display());
                        std::process::exit(0);
                    }
                    Err(e) => finish(vec![e]),
                }
            }
            "--island" => finish(ui::render_island(&dir(&mut args, "--island"))),
            "--relay-activity" => finish(ui::render_relay_activity(&dir(&mut args, "--relay-activity"))),
            "--recovery-countdown" => finish(ui::render_recovery_countdown(&dir(&mut args, "--recovery-countdown"))),
            "--catching-up" => finish(ui::render_catching_up(&dir(&mut args, "--catching-up"))),
            "--island-loops" => finish(ui::render_island_loops(&dir(&mut args, "--island-loops"))),
            "--drawing" => finish(ui::render_drawing(&dir(&mut args, "--drawing"))),
            "--drop" => finish(ui::render_drop(&dir(&mut args, "--drop"))),
            "--drop-live" => {
                let secs = match args.next() {
                    None => 60.0,
                    Some(s) => s.parse::<f64>().ok().filter(|s| *s > 0.0).unwrap_or_else(|| {
                        eprintln!("--drop-live: seconds, not {s:?}\n\n{HELP}");
                        std::process::exit(2);
                    }),
                };
                ui::drop_live(secs)
            }
            "--drawing-over-menu" => {
                let own = match args.next().as_deref() {
                    Some("own") => true,
                    Some("other") => false,
                    other => {
                        eprintln!("--drawing-over-menu: other or own, not {other:?}\n\n{HELP}");
                        std::process::exit(2);
                    }
                };
                let secs = args.next().and_then(|s| s.parse::<f64>().ok()).filter(|s| *s > 0.0).unwrap_or(3.0);
                ui::drawing_over_menu(own, secs)
            }
            "--streams" => finish(ui::render_streams(&dir(&mut args, "--streams"))),
            "--menu-live" => finish(ui::render_menu_live(&dir(&mut args, "--menu-live"))),
            "--menu-sections-live" => finish(ui::render_menu_sections_live(&dir(&mut args, "--menu-sections-live"))),
            "--submenus-left" => finish(ui::render_submenus_left(&dir(&mut args, "--submenus-left"))),
            "--agent" => finish(ui::render_agent(&dir(&mut args, "--agent"))),
            "--chrome" => finish(ui::render_chrome(&dir(&mut args, "--chrome"))),
            "--menu-rows" => finish(ui::render_menu_rows(&dir(&mut args, "--menu-rows"))),
            "--claude-relay" => finish(ui::render_claude_relay(&dir(&mut args, "--claude-relay"))),
            "--model-choice" => finish(ui::render_model_choice(&dir(&mut args, "--model-choice"))),
            "--background-picker" => finish(ui::render_background_picker(&dir(&mut args, "--background-picker"))),
            "--relay-account" => finish(ui::render_relay_account(&dir(&mut args, "--relay-account"))),
            "--engine-models" => finish(ui::render_engine_models(&dir(&mut args, "--engine-models"))),
            "--engine-section" => finish(ui::render_engine_section(&dir(&mut args, "--engine-section"))),
            "--decider-section" => finish(ui::render_decider_section(&dir(&mut args, "--decider-section"))),
            "--menu-climb" => finish(ui::render_menu_climb(&dir(&mut args, "--menu-climb"))),
            "--menu-bench" => finish(ui::bench_menu_sync()),
            "--composer" => finish(ui::render_composer(&dir(&mut args, "--composer"))),
            "--pulldown-drop" => finish(ui::render_pulldown_drop(&dir(&mut args, "--pulldown-drop"))),
            "--bug-notes" => finish(ui::render_bug_notes(&dir(&mut args, "--bug-notes"))),
            "--friends" => {
                let out = dir(&mut args, "--friends");
                let qr = dir(&mut args, "--friends");
                finish(ui::render_friends(&out, &qr))
            }
            "--motion" => {
                let (report, findings) = ui::render_motion(&dir(&mut args, "--motion"));
                print!("{report}");
                std::process::exit(if findings == 0 { 0 } else { 1 });
            }
            "--gallery" => {
                let d = dir(&mut args, "--gallery");
                let (mut only, mut fps) = (vec![], 120.0);
                while let Some(flag) = args.next() {
                    match flag.as_str() {
                        "--only" => only = args.next().unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                        "--fps" => fps = args.next().and_then(|s| s.parse().ok()).unwrap_or(120.0),
                        other => {
                            eprintln!("--gallery: unknown flag {other}\n\n{HELP}");
                            std::process::exit(2);
                        }
                    }
                }
                finish(ui::render_gallery(&d, &only, fps));
            }
            "--forms" => finish(ui::render_forms(&dir(&mut args, "--forms"))),
            "--wizard" => finish(ui::render_wizard(&dir(&mut args, "--wizard"))),
            "--personalise" => finish(ui::render_personalise(&dir(&mut args, "--personalise"))),
            "--memory" => finish(ui::render_memory(&dir(&mut args, "--memory"))),
            "--speaking" => {
                let d = dir(&mut args, "--speaking");
                finish(ui::render_speaking(&d));
            }
            "--offscreen" => {
                let d = dir(&mut args, "--offscreen");
                let icns = (args.peek().map(String::as_str) == Some("--icns")).then(|| {
                    args.next();
                    dir(&mut args, "--icns")
                });
                finish(ui::render_offscreen(&d, icns.as_deref()));
            }
            "--render" | "--checks" => {
                render_checks_only = arg == "--checks";
                render_dir = Some(dir(&mut args, "--render"));
            }
            other => {
                eprintln!("unknown flag {other}\n\n{HELP}");
                std::process::exit(2);
            }
        }
    }
    if render_dir.is_none() && !live_demo {
        println!("{HELP}");
        return;
    }
    let render = render_dir.is_some();
    let ui = Ui::handle();

    ui.send(UiUpdate::Listen(ListenState::Listening));
    ui.send(UiUpdate::Sources(sources()));
    ui.send(UiUpdate::AppAudio(demo_app_audio()));
    ui.send(UiUpdate::Microphones(Microphones {
        devices: vec![
            Microphone { id: "BuiltInMicrophoneDevice".into(), name: "MacBook Pro Microphone".into(), connected: true, built_in: true },
            Microphone { id: "AC-3E-B1:input".into(), name: "AirPods Pro".into(), connected: true, built_in: false },
        ],
        primary: None,
        in_use: Some("BuiltInMicrophoneDevice".into()),
        extras: vec![],
    }));
    ui.send(UiUpdate::Agent(AgentState {
        harness: Harness::Pi,
        session: SessionState::Running { since: "09:14".into() },
    }));
    ui.send(UiUpdate::RuntimeSettings(RuntimeSettings { dry_run: false, ..Default::default() }));
    ui.send(UiUpdate::AgentSession(AgentSession {
        harness: Harness::Pi,
        model: Some("claude-opus-5-5".into()),
        session_id: Some("1b6e0c52-7d1a-4f0e-9a3c-5e8f2d4b7a91".into()),
        agent_on: true,
        can_open: true,
    }));
    ui.send(UiUpdate::AgentWindow(AgentWindowUpdate::State(ui::agent_view::demo_state(AgentRunState::Ready))));
    ui.send(UiUpdate::AgentWindow(AgentWindowUpdate::History(ui::agent_view::demo_live_entries())));
    ui.send(UiUpdate::Speakers(speakers()));
    ui.send(UiUpdate::Permissions(permissions()));
    ui.send(UiUpdate::Settings(Settings { speak_replies: true, launch_at_login: true, ..Default::default() }));
    ui.send(engine(EngineState::Ready));
    ui.send(UiUpdate::ApiProviders(ui::demo_api_providers(ui::ProviderAuth::Missing, false)));
    for l in transcript() {
        ui.send(UiUpdate::Transcript(l));
    }
    if render_checks_only {
        // The menu checks only: no popup may appear while a menu tracks.
    } else if render {
        // The name entry opens after launch.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(600));
            ui.send(name_entry());
        });
        // The partial line arrives word by word from another thread, as the recogniser sends it.
        thread::spawn(move || {
            let partial = "Rust. I can hear the click again when";
            let all: Vec<&str> = partial.split(' ').collect();
            for n in 1..=all.len() {
                ui.send(UiUpdate::Transcript(line(7, "Discord", "Dave", vec![words(&all[..n].join(" "))], false)));
                thread::sleep(Duration::from_millis(20));
            }
        });
    } else {
        ui.send(UiUpdate::ShowTranscript);
        thread::spawn(move || live(ui));
    }

    let speakers_now = RefCell::new(speakers());
    let runtime = RefCell::new(RuntimeSettings { dry_run: false, ..Default::default() });
    let options = Options { fake_permissions: true, render_dir, render_checks_only, system_profile: None, lab: false, friends: false };
    run(options, move |action| {
        eprintln!("action: {action:?}");
        match action {
            UiAction::SetListening(on) => {
                ui.send(UiUpdate::Listen(if on { ListenState::Listening } else { ListenState::Paused }))
            }
            UiAction::ListenToSpeaker { id, on } => {
                let mut list = speakers_now.borrow_mut();
                if let Some(k) = list.iter_mut().find(|k| k.id == id) {
                    k.listen = if on { Listen::On } else { Listen::Off };
                }
            }
            UiAction::NameVoice { name, .. } => {
                let mut list = speakers_now.borrow_mut();
                let id = format!("v{}", list.len() + 1);
                list.push(KnownSpeaker {
                    id,
                    name,
                    heard_in: "Discord".into(),
                    clips: 1,
                    last_heard: "just now".into(),
                    ignored: false,
                    listen: Listen::Off,
                });
                ui.send(UiUpdate::Speakers(list.clone()));
            }
            UiAction::SetDryRun(dry_run) => {
                let mut r = runtime.borrow_mut();
                r.dry_run = dry_run;
                ui.send(UiUpdate::RuntimeSettings(r.clone()));
            }
            UiAction::SetSeeing(seeing) => {
                let mut r = runtime.borrow_mut();
                r.seeing = seeing;
                ui.send(UiUpdate::RuntimeSettings(r.clone()));
            }
            UiAction::SetScreenRecording(screen) => {
                let mut r = runtime.borrow_mut();
                r.screen = screen;
                ui.send(UiUpdate::RuntimeSettings(r.clone()));
            }
            // The engine answers at once: Starting, then Ready a moment later (the real one loads for a while).
            UiAction::SetEngine(on) => {
                ui.send(engine(if on { EngineState::Starting } else { EngineState::Off }));
                if on {
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(300));
                        ui.send(engine(EngineState::Ready));
                    });
                }
            }
            // Looks-only: any pasted key connects; "sk-bad…" is rejected, as a key without Go would be.
            // Looks-only sign-in: a code at once, approved a few seconds later.
            UiAction::StartProviderLogin { provider } => {
                ui.send(UiUpdate::ProviderLogin { provider: provider.clone(), code: "DLXZ-HLMT".into(), url: "https://opencode.ai/console/device?user_code=DLXZ-HLMT".into() });
                thread::spawn(move || {
                    thread::sleep(Duration::from_secs(4));
                    ui.send(UiUpdate::ApiProviders(ui::demo_api_providers(ui::ProviderAuth::Connected, false)));
                    ui.send(UiUpdate::ProviderConnected { provider, result: Ok(()) });
                });
            }
            UiAction::ConnectProviderKey { provider, key } => {
                let result = if key.starts_with("sk-bad") { Err("OpenCode Go: access rejected / no subscription".into()) } else { Ok(()) };
                if result.is_ok() { ui.send(UiUpdate::ApiProviders(ui::demo_api_providers(ui::ProviderAuth::Connected, false))); }
                ui.send(UiUpdate::ProviderConnected { provider, result });
            }
            UiAction::DisconnectProvider { .. } => ui.send(UiUpdate::ApiProviders(ui::demo_api_providers(ui::ProviderAuth::Missing, false))),
            UiAction::Quit => ui.send(UiUpdate::Terminate),
            _ => {}
        }
    });
}

/// The one engine model in `state`; Ready carries the figures Splash reports.
fn engine(state: EngineState) -> UiUpdate {
    let ready = state == EngineState::Ready;
    UiUpdate::Engine {
        state,
        model_id: "qwen3.8-27b".into(),
        model_label: "Qwen3.8 27B".into(),
        models: vec![EngineModel {
            id: "qwen3.8-27b".into(),
            label: "Qwen3.8 27B".into(),
            quant: "UD-Q4_K_M".into(),
            source: "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M".into(),
            ..Default::default()
        }],
        port: 8100,
        context_limit: ready.then_some(262_144),
        cache_used_bytes: ready.then_some(44_237_195_264),
        cache_limit_bytes: ready.then_some(100 << 30),
        memory_limit_bytes: Some(48 << 30),
    }
}

/// Fake speech: a partial line grows word by word, turns final, the next one starts.
fn live(ui: Ui) {
    let script: [(&str, &str, &str); 4] = [
        ("Discord", "Dave", "Rust. I can hear the click again when the stream restarts, every single time."),
        ("Mic", "User", "Okay, take a screenshot and ask Heimdall why the ring buffer underruns."),
        ("Chrome", "video", "So the first thing to check is whether your callback ever allocates."),
        ("Discord", "Erin", "I think it is the resampler, it buffers a whole block."),
    ];
    thread::sleep(Duration::from_secs(4));
    ui.send(name_entry());
    let mut id = 7;
    let mut turn = 0usize;
    loop {
        let (source, speaker, text) = script[turn % script.len()];
        let all: Vec<&str> = text.split(' ').collect();
        for n in 1..=all.len() {
            let mut segments = vec![words(&all[..n].join(" "))];
            if n == all.len() && speaker == "User" {
                segments.push(note("screenshot attached"));
            }
            ui.send(UiUpdate::Transcript(line(id, source, speaker, segments, n == all.len())));
            thread::sleep(Duration::from_millis(220));
        }
        id += 1;
        turn += 1;
        thread::sleep(Duration::from_millis(1600));
    }
}

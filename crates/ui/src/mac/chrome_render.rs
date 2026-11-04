//! [chrome] Night 1022, stage 3: the menu's chrome rendered and checked offscreen (`heimdall-ui-demo --chrome <dir>`;
//! nothing reaches the screen or the menu bar). The whole menu from the real `menu::build` in each state the chrome
//! changes (agent on, local and API; agent off; a setting waiting for a retry; setup needed), its rows checked per
//! state; Support the Developer at rest and highlighted; the transcript with nameable "Speaker N" labels, their hit
//! test through the text view, and the naming sheet; the Speakers window's listen switches.

use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSColor, NSTextInputClient as _};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRange};

use super::menu::{self, StatusClick, DEBUG_AGENT_TITLE, SUPPORT_TITLE, SUPPORT_URL};
use super::offscreen::{capture, capture_rounded, engine_states, stacked, MENU_WIDTH};
use super::prominent::ProminentRow;
use super::transcript::{SpeakerSpan, TranscriptWindow};
use crate::model::*;

type Setup = Box<dyn Fn(&mut super::State)>;

/// What a state's menu must show: the Agent caption (`None` = hidden) and the status line with its click.
struct Expect {
    caption: Option<&'static str>,
    status: Option<(&'static str, StatusClick)>,
}

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("the chrome renders run on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        eprintln!("check {} {name}{}", if ok { "ok" } else { "FAILED" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok { failures.push(format!("{name}: {detail}")); }
    };

    let granted = |s: &mut super::State| {
        s.apply(UiUpdate::Permissions(Permission::ALL.iter().map(|p| (*p, PermissionState::Granted)).collect()));
    };
    let opus = move |s: &mut super::State| {
        granted(s);
        let mut api = demo_anthropic_providers(ProviderAuth::Connected);
        if let Some(m) = api.models.iter_mut().find(|m| m.provider == "anthropic" && m.id == "claude-opus-5-5") {
            if let Some(d) = m.detail.as_mut() { d.thinking = "high".into(); }
        }
        api.selected = Some(("anthropic".into(), "claude-opus-5-5".into()));
        s.api = api;
    };
    let pending = |s: &mut super::State| {
        s.apply(UiUpdate::RuntimeSettings(RuntimeSettings { dry_run: false, pending_reload: vec!["Screen Recording".into()], ..Default::default() }));
    };
    let engine = engine_states().into_iter().find(|(n, _)| *n == "ready").unwrap().1.title();
    let local: &'static str = Box::leak(format!("{engine} · Low · Local Splash Engine").into_boxed_str());
    let retry = "Screen Recording didn\u{2019}t apply — Retry";
    let states: Vec<(&str, bool, Setup, Expect)> = vec![
        ("menu-agent-on-local", true, Box::new(granted), Expect { caption: Some(local), status: None }),
        ("menu-agent-on-anthropic", true, Box::new(opus), Expect { caption: Some("Claude Opus 5.5 · High · Anthropic API"), status: None }),
        ("menu-agent-on-anthropic-light", false, Box::new(opus), Expect { caption: Some("Claude Opus 5.5 · High · Anthropic API"), status: None }),
        ("menu-agent-off", true, Box::new(move |s| {
            granted(s);
            s.apply(UiUpdate::RuntimeSettings(RuntimeSettings { dry_run: true, ..Default::default() }));
        }), Expect { caption: None, status: None }),
        ("menu-reload-pending", true, Box::new(move |s| { granted(s); pending(s); }), Expect { caption: Some(local), status: Some((retry, StatusClick::Retry)) }),
        ("menu-reload-pending-paused", true, Box::new(move |s| {
            granted(s);
            pending(s);
            s.apply(UiUpdate::Listen(ListenState::Paused));
        // Listening off leaves the agent running, so its caption stays.
        }), Expect { caption: Some(local), status: Some((retry, StatusClick::Retry)) }),
        // Setup outranks a waiting retry, on the line and on its click.
        ("menu-setup-needed", true, Box::new(move |s| {
            pending(s);
            s.apply(UiUpdate::Listen(ListenState::NeedsSetup));
        }), Expect { caption: Some(local), status: Some(("Permissions needed — Set Up…", StatusClick::Setup)) }),
    ];
    for (name, dark, setup, expect) in states {
        let state = demo_state(mtm, setup);
        menu_checks(name, &state, &expect, &mut check);
        let path = dir.join(format!("{name}.png"));
        let container = stacked(&state.menu, MENU_WIDTH, (5.0, 0.5), dark, mtm);
        let ok = capture_rounded(&path, &container, dark);
        check(&format!("{name}: rendered {}", path.display()), ok, "capture failed".into());
    }

    // Support the Developer: resting (lighter row) and highlighted, the menu's last group at real width.
    for (name, lit) in [("menu-support-resting", false), ("menu-support-highlighted", true)] {
        let state = demo_state(mtm, Box::new(granted));
        let support = state.menu.itemArray().iter().find(|i| i.action() == Some(sel!(supportDeveloper:)));
        let row = support.as_ref().and_then(|i| i.view()).and_then(|v| v.downcast::<ProminentRow>().ok());
        let Some(row) = row else { check(name, false, "no ProminentRow".into()); continue };
        row.force_highlight(lit);
        let (fill, ink) = row.look();
        let fill = fill.colorUsingColorSpace(&objc2_app_kit::NSColorSpace::sRGBColorSpace());
        let accent = NSColor::controlAccentColor().colorUsingColorSpace(&objc2_app_kit::NSColorSpace::sRGBColorSpace());
        if lit {
            let same = matches!((&fill, &accent), (Some(f), Some(a)) if (f.redComponent() - a.redComponent()).abs() < 0.01 && (f.blueComponent() - a.blueComponent()).abs() < 0.01);
            check(&format!("{name}: accent highlight, white ink"), same && ink == NSColor::whiteColor(), format!("{fill:?}"));
        } else {
            let alpha = fill.as_ref().map_or(0.0, |f| f.alphaComponent());
            check(&format!("{name}: rests at {} label alpha", super::prominent::RESTING_ALPHA), (alpha - super::prominent::RESTING_ALPHA).abs() < 0.01, format!("alpha {alpha}"));
        }
        // The last group alone (separator, Launch at Login, Support, Quit), at the full menu's width.
        let tail = objc2_app_kit::NSMenu::new(mtm);
        let items = state.menu.itemArray();
        let from = items.iter().position(|i| i.action() == Some(sel!(showArchive:))).map_or(0, |p| p + 1);
        for item in items.iter().skip(from) {
            state.menu.removeItem(&item);
            tail.addItem(&item);
        }
        let path = dir.join(format!("{name}.png"));
        let ok = capture_rounded(&path, &stacked(&tail, MENU_WIDTH, (0.0, 5.0), true, mtm), true);
        check(&format!("{name}: rendered {}", path.display()), ok, "capture failed".into());
    }

    transcript_checks(dir, mtm, &mut check);
    speakers_checks(dir, mtm, &mut check);
    failures
}

/// Every listen switch in `view`, depth first (the rows' order).
fn switches(view: &objc2_app_kit::NSView) -> Vec<Retained<objc2_app_kit::NSButton>> {
    let mut out = vec![];
    for sub in view.subviews().iter() {
        match sub.clone().downcast::<objc2_app_kit::NSButton>() {
            Ok(b) if b.action() == Some(sel!(speakerListen:)) => out.push(b),
            _ => out.extend(switches(&sub)),
        }
    }
    out
}

/// The Speakers window with the listen switches: the user on and locked, a listened guest on, one off, an ignored voice
/// with none. Each live switch sends `speakerListen:` with its row; the window renders dark at its real size.
fn speakers_checks(dir: &Path, mtm: MainThreadMarker, check: &mut impl FnMut(&str, bool, String)) {
    let controller = super::Controller::new(mtm);
    let known = |id: &str, name: &str, heard_in: &str, clips, last: &str, listen| KnownSpeaker {
        id: id.into(), name: name.into(), heard_in: heard_in.into(), clips, last_heard: last.into(), ignored: false, listen,
    };
    let list = vec![
        known("User", "User", "Mic", 0, "", Listen::Always),
        known("Vim", "Vim", "Discord", 8, "", Listen::On),
        known("Dave", "Dave", "Discord", 6, "", Listen::Off),
        KnownSpeaker { ignored: true, ..known("ignored-1-1", "\u{201c}Subscribe for more\u{201d}", "Discord", 1, "21 Oct", Listen::None) },
    ];
    let mut w = super::speakers::SpeakersWindow::new(mtm);
    w.show(&list, &controller, mtm);
    let Some(content) = w.window.contentView() else { check("speakers: content view", false, String::new()); return };
    let found = switches(&content);
    let states: Vec<_> = found.iter().map(|s| (s.tag(), s.state() == objc2_app_kit::NSControlStateValueOn, s.isEnabled())).collect();
    check("speakers: a switch per named voice, none for the ignored one; User on and locked, Vim on, Dave off",
        states == [(0, true, false), (1, true, true), (2, false, true)], format!("{states:?}"));
    let wired = found.iter().all(|s| s.target().is_some());
    check("speakers: the switches send speakerListen: to the controller", wired, String::new());
    let tips: Vec<String> = found.iter().map(|s| s.toolTip().map(|t| t.to_string()).unwrap_or_default()).collect();
    check("speakers: the switch says how long it lasts", tips[1] == "Heimdall acts on Vim's requests until you turn this off" && tips[0] == "Heimdall always listens to you", format!("{tips:?}"));
    // The switches line up in one column, clear of the pencil.
    let xs: Vec<f64> = found.iter().map(|s| s.frame().origin.x + s.frame().size.width).collect();
    check("speakers: switches share one right edge", xs.windows(2).all(|p| (p[0] - p[1]).abs() < 0.5), format!("{xs:?}"));
    // A click flips it in place (a toggle button: off, on, off); the user's never moves.
    unsafe { found[2].performClick(None) };
    let flipped = found[2].state() == objc2_app_kit::NSControlStateValueOn;
    unsafe { found[2].performClick(None) };
    check("speakers: a click flips Dave's switch in place and back", flipped && found[2].state() == objc2_app_kit::NSControlStateValueOff, String::new());
    unsafe { found[0].performClick(None) };
    check("speakers: User's switch stays on", found[0].state() == objc2_app_kit::NSControlStateValueOn, String::new());
    for (name, dark) in [("speakers-listen", true), ("speakers-listen-light", false)] {
        let path = dir.join(format!("{name}.png"));
        let ok = capture(&path, &content, dark);
        check(&format!("speakers: rendered {}", path.display()), ok, "capture failed".into());
    }
}

/// The demo's menu state (as `offscreen::full_menu`: listening, the agent running since 09:14, the engine ready),
/// changed by `setup`, then built by the real `menu::build`.
fn demo_state(mtm: MainThreadMarker, setup: Setup) -> super::State {
    let controller = super::Controller::new(mtm);
    let menu = objc2_app_kit::NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let options = crate::Options { fake_permissions: true, ..Default::default() };
    let mut state = super::State::new(mtm, controller, options, Box::new(|_| {}), None, menu, None);
    state.apply(UiUpdate::Listen(ListenState::Listening));
    state.apply(UiUpdate::Sources(vec![Source { id: "mic".into(), kind: SourceKind::Mic, name: "MacBook Pro Microphone".into(), enabled: true }]));
    state.apply(UiUpdate::Agent(AgentState { harness: Harness::Pi, session: SessionState::Running { since: "09:14".into() } }));
    state.apply(UiUpdate::RuntimeSettings(RuntimeSettings { dry_run: false, ..Default::default() }));
    state.apply(UiUpdate::Settings(Settings { speak_replies: true, launch_at_login: true, ..Default::default() }));
    state.engine = engine_states().into_iter().find(|(n, _)| *n == "ready").unwrap().1;
    setup(&mut state);
    state.rebuild_menu();
    state
}

/// The chrome's rows in one state: Debug Agent (no Open Agent), no Reload, no Archive submenu, Open Archive Folder,
/// then Launch at Login, Support the Developer (the prominent row, its action and page) and Quit last; the caption
/// and the status line with what its click does.
fn menu_checks(name: &str, state: &super::State, expect: &Expect, check: &mut impl FnMut(&str, bool, String)) {
    let items = state.menu.itemArray();
    let titles: Vec<String> = items.iter().filter(|i| !i.isHidden() && !i.isSeparatorItem() && !i.isSectionHeader()).map(|i| i.title().to_string()).filter(|t| !t.is_empty()).collect();
    let has = |t: &str| titles.iter().any(|x| x == t);
    check(&format!("{name}: Debug Agent, no Open Agent"), has(DEBUG_AGENT_TITLE) && !has("Open Agent"), format!("{titles:?}"));
    check(&format!("{name}: no Reload row"), !titles.iter().any(|t| t.starts_with("Reload")) && !items.iter().any(|i| i.action() == Some(sel!(reload:))), format!("{titles:?}"));
    check(&format!("{name}: no Archive submenu"), !has("Archive") && !items.iter().any(|i| i.hasSubmenu() && i.title().to_string() == "Archive"), format!("{titles:?}"));
    // Shortcuts ▸ (24 Oct) closes the settings rows; Name & Icon ▸ lives in Personalisation (26 Oct 16:45).
    let tail = ["Open Archive Folder", super::menu::SHORTCUTS_TITLE, "Launch at Login", SUPPORT_TITLE, "Quit Heimdall"];
    check(&format!("{name}: ends Open Archive Folder · Shortcuts · Launch at Login · Support · Quit"), titles.ends_with(&tail.map(String::from)), format!("{titles:?}"));
    let support = items.iter().find(|i| i.title().to_string() == SUPPORT_TITLE);
    let prominent = support.as_ref().and_then(|i| i.view()).is_some_and(|v| v.downcast::<ProminentRow>().is_ok_and(|r| r.title() == SUPPORT_TITLE));
    check(&format!("{name}: Support is the prominent row, opening {SUPPORT_URL}"),
        prominent && support.as_ref().is_some_and(|i| i.action() == Some(sel!(supportDeveloper:)) && i.isEnabled()) && SUPPORT_URL == "https://tobynoskillson.com/#support",
        "missing or plain".into());
    // The caption.
    let caption = state.session_item.as_ref().map(|(item, row)| (!item.isHidden()).then(|| row.text()));
    check(&format!("{name}: caption {:?}", expect.caption), caption.clone().flatten().as_deref() == expect.caption, format!("{caption:?}"));
    // The status line and its click.
    let status = state.status_line.as_ref().and_then(|(item, row)| (!item.isHidden()).then(|| row.text()));
    let click = menu::status_click(state);
    let want = expect.status.map(|(t, c)| (t.to_string(), c));
    let got = status.clone().map(|t| (t, click));
    check(&format!("{name}: status {:?}", expect.status), got == want || (want.is_none() && status.is_none()), format!("{got:?}"));
}

fn tline(id: u64, source: &str, speaker: &str, words: &str) -> TranscriptLine {
    TranscriptLine { id, label: Some(Label { source: source.into(), speaker: speaker.into() }), segments: vec![Segment::Words(words.into())], is_final: true }
}

/// UTF-16 index of the `nth` occurrence of `needle` in `text`.
fn utf16_index(text: &str, needle: &str, nth: usize) -> Option<usize> {
    let byte = text.match_indices(needle).nth(nth)?.0;
    Some(text[..byte].encode_utf16().count())
}

fn transcript_checks(dir: &Path, mtm: MainThreadMarker, check: &mut impl FnMut(&str, bool, String)) {
    let target: Retained<AnyObject> = Retained::into_super(Retained::into_super(super::Controller::new(mtm)));
    let mut t = TranscriptWindow::new(mtm);
    t.upsert(&tline(1, "Mic", "User", "Can you all hear me?"));
    t.upsert(&tline(2, "Discord", "Dave", "Loud and clear."));
    t.upsert(&tline(3, "Discord", "Speaker 2", "Hi, I just joined from the other server."));
    // One utterance, three speakers' runs: one id.
    t.upsert_runs(4, &[tline(4, "Discord", "Speaker 3", "Welcome."), tline(4, "Discord", "Speaker ?", "Hey."), tline(4, "Discord", "Speaker 2", "Thanks.")]);
    let text = t.text();
    let at = |needle: &str, nth: usize| utf16_index(&text, needle, nth).map(|i| i + 3);
    let span = |line_id, speaker: &str, start: usize| SpeakerSpan { start, len: format!("[Discord · {speaker}]").encode_utf16().count(), line_id, source: "Discord".into(), speaker: speaker.into() };
    let speaker2 = t.speaker_at(at("[Discord · Speaker 2]", 0).unwrap());
    check("transcript: [Discord · Speaker 2] is nameable (line 3)", speaker2 == Some(span(3, "Speaker 2", 0)), format!("{speaker2:?}"));
    check("transcript: a named speaker is not", t.speaker_at(at("[Discord · Dave]", 0).unwrap()).is_none(), String::new());
    check("transcript: User on the mic is not", t.speaker_at(at("[Mic · User]", 0).unwrap()).is_none(), String::new());
    check("transcript: the words after the label are not", t.speaker_at(at("just joined", 0).unwrap()).is_none(), String::new());
    let s3 = t.speaker_at(at("[Discord · Speaker 3]", 0).unwrap());
    check("transcript: a run's Speaker 3 is nameable on its utterance (line 4)", s3.as_ref().is_some_and(|s| s.line_id == 4 && s.speaker == "Speaker 3" && s.start == 0), format!("{s3:?}"));
    check("transcript: Speaker ? is not", t.speaker_at(at("[Discord · Speaker ?]", 0).unwrap()).is_none(), String::new());
    let s2b = t.speaker_at(at("[Discord · Speaker 2]", 1).unwrap());
    check("transcript: the third run's Speaker 2 is nameable on line 4", s2b.as_ref().is_some_and(|s| s.line_id == 4 && s.speaker == "Speaker 2" && s.start > 0), format!("{s2b:?}"));

    // Through the text view's own hit test (what the double-click uses): the label's glyphs, by window point.
    let view = t.text_view();
    let hit_at = |index: usize, dx: f64| {
        let rect = unsafe { view.firstRectForCharacterRange_actualRange(NSRange::new(index, 1), std::ptr::null_mut()) };
        let screen = NSPoint::new(rect.origin.x + rect.size.width / 2.0 + dx, rect.origin.y + rect.size.height / 2.0);
        view.window().and_then(|w| view.speaker_at_window_point(w.convertPointFromScreen(screen)))
    };
    let hit = |index: usize| hit_at(index, 0.0);
    let by_point = hit(at("[Discord · Speaker 2]", 0).unwrap());
    check("transcript: the text view's hit test finds Speaker 2 by point", by_point.as_ref().is_some_and(|s| s.line_id == 3), format!("{by_point:?}"));
    check("transcript: and not Dave by point", hit(at("[Discord · Dave]", 0).unwrap()).is_none(), String::new());
    let past = at("Loud and clear.", 0).map(|i| hit_at(i, 480.0));
    check("transcript: a click past a line's end names nothing", past == Some(None), format!("{past:?}"));

    // The render: the window as it reads, then the naming sheet's alert.
    if let Some(content) = t.window.contentView() {
        let path = dir.join("transcript-speakers.png");
        let ok = capture(&path, &content, true);
        check(&format!("transcript: rendered {}", path.display()), ok, "capture failed".into());
    }
    let alert = t.begin_naming_offscreen(speaker2.clone().unwrap(), &target, mtm);
    let buttons: Vec<String> = alert.buttons().iter().map(|b| b.title().to_string()).collect();
    let actions: Vec<_> = alert.buttons().iter().map(|b| b.action()).collect();
    check("naming sheet: \"Who is Speaker 2?\" · Discord · Name, Cancel",
        alert.messageText().to_string() == "Who is Speaker 2?" && alert.informativeText().to_string() == "Discord" && buttons == ["Name", "Cancel"]
            && actions == [Some(sel!(transcriptNameConfirm:)), Some(sel!(transcriptNameCancel:))],
        format!("{buttons:?}"));
    if let Some(content) = alert.window().contentView() {
        t.set_naming_text("Ada");
        let path = dir.join("transcript-naming-sheet.png");
        let ok = capture(&path, &content, true);
        check(&format!("naming sheet: rendered {}", path.display()), ok, "capture failed".into());
    }
    t.set_naming_text("  Ada ");
    let named = t.finish_naming(true);
    check("naming sheet: Name sends the trimmed name for line 3's Speaker 2", named == Some((span(3, "Speaker 2", 0), "Ada".into())), format!("{named:?}"));
    t.begin_naming_offscreen(speaker2.clone().unwrap(), &target, mtm);
    t.set_naming_text("   ");
    check("naming sheet: an empty name sends nothing", t.finish_naming(true).is_none(), String::new());
    t.begin_naming_offscreen(speaker2.unwrap(), &target, mtm);
    t.set_naming_text("Ada");
    check("naming sheet: Cancel sends nothing", t.finish_naming(false).is_none() && t.naming_span().is_none(), String::new());
    // The app relabels the line: the name is no longer nameable, the run's own labels stay.
    t.upsert(&tline(3, "Discord", "Ada", "Hi, I just joined from the other server."));
    let text = t.text();
    let after = utf16_index(&text, "[Discord · Ada]", 0).map(|i| t.speaker_at(i + 3));
    check("transcript: once named the label is plain", after == Some(None), format!("{after:?}"));
    let still = utf16_index(&text, "[Discord · Speaker 3]", 0).and_then(|i| t.speaker_at(i + 3));
    check("transcript: the next utterance's labels moved with it", still.is_some_and(|s| s.line_id == 4), String::new());
    if let Some(content) = t.window.contentView() {
        let path = dir.join("transcript-speakers-after-naming.png");
        let ok = capture(&path, &content, true);
        check(&format!("transcript: rendered {}", path.display()), ok, "capture failed".into());
    }
}

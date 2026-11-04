//! Every Setup page and key state offscreen at real size (800 × 680 points, the screen's scale), dark, into `dir`, with
//! stable names (`01-welcome` … `11-customise-last`), plus the window at its minimum and widened. Each shot also checks
//! the controls' state and that the page is settled (no arrival animation half way). Returns what failed.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_foundation::{MainThreadMarker, NSSize};

use super::*;

/// Decider V5's three precisions as the app reports them (memory and disk measured), 4-bit chosen.
pub(crate) fn demo_decider() -> DeciderStatus {
    let o = |key: &str, label: &str, disk: u64, memory: u64| DeciderOption { key: key.into(), label: label.into(), disk_bytes: disk, memory_bytes: memory, present: true };
    DeciderStatus {
        version: "Decider V5".into(),
        options: vec![o("16bit", "16-bit", 3_854_334_606, 4_160_000_000), o("8bit", "8-bit", 2_100_954_734, 3_080_000_000), o("4bit", "4-bit", 1_367_819_482, 2_350_000_000)],
        chosen: "4bit".into(),
        running: Some("4bit".into()),
        fallback: None,
        reason: None,
        state: DeciderState::Ready,
    }
}

fn statuses(f: impl Fn(ProviderChoice) -> ProviderStatus) -> Vec<(ProviderChoice, ProviderStatus)> {
    ProviderChoice::ALL.iter().map(|c| (*c, f(*c))).collect()
}

pub(crate) fn render(dir: &std::path::Path, mtm: MainThreadMarker) -> Vec<String> {
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    // The public edition's first run: Heimdall, no user name yet.
    crate::identity::set(Identity { name: "Heimdall".into(), user_name: crate::identity::PUBLIC_USER.into(), icon: Icon::Helmet });
    let controller = super::super::Controller::new(mtm);
    let target: Retained<AnyObject> = Retained::into_super(Retained::into_super(controller.clone()));
    // Every step, the public edition's Coding Agents included.
    let fresh = || SetupWindow::build(&target, mtm, true);
    let shot = |w: &SetupWindow, name: &str, ok: bool, failures: &mut Vec<String>| {
        let path = dir.join(format!("{name}.png"));
        // The first frame is the settled one: opaque, in place.
        let page = w.current_view();
        let settled = (page.alphaValue() - 1.0).abs() < 1e-6 && page.frame().origin.x.abs() < 0.5;
        if !super::super::agent_render::capture_window(&w.window, &path) {
            failures.push(format!("{name}: not written"));
        }
        if !settled {
            failures.push(format!("{name}: the page is mid-arrival"));
        }
        if !ok {
            failures.push(format!("{name}: controls in the wrong state"));
        }
    };
    // A sample user, never this Mac's: renders are shared. `tilde` reads HOME, so it is the sample's while rendering.
    let real_home = std::env::var_os("HOME");
    let user_home = std::path::PathBuf::from("/Users/ada");
    std::env::set_var("HOME", &user_home);
    let fresh_permissions: Vec<_> = Permission::ALL.iter().map(|p| (*p, PermissionState::NotDetermined)).collect();
    let granted: Vec<_> = Permission::ALL.iter().map(|p| (*p, PermissionState::Granted)).collect();

    let mut w = fresh();
    w.show(&fresh_permissions);
    if w.allow_page.title.stringValue().to_string() != "Allow Heimdall" {
        failures.push("allow: the title is empty until the identity changes".into());
    }
    let size = w.window.frame().size;
    if (size.width - W).abs() > 0.5 || (size.height - H).abs() > 0.5 {
        failures.push(format!("window: {} × {}, not {W} × {H}", size.width, size.height));
    }
    let ok = w.current() == PageKind::Builtin(SetupStep::Welcome) && w.back.isHidden() && w.next.label() == "Set Up" && w.next.keyEquivalent().to_string() == "\r";
    shot(&w, "01-welcome", ok, &mut failures);

    let advanced = w.advance();
    let ok = advanced == Advance::Next(vec![]) && w.current() == PageKind::Builtin(SetupStep::Name) && !w.back.isHidden() && w.name.field.stringValue().to_string().is_empty();
    shot(&w, "02-name", ok, &mut failures);
    // A name that does not validate stays, with the reason; a good one goes with Continue.
    w.name.field.setStringValue(&ns("Ada!"));
    let stayed = w.advance() == Advance::Stay && !w.name.error.isHidden();
    shot(&w, "02-name-invalid", stayed, &mut failures);
    w.name.field.setStringValue(&ns("Ada"));
    let advanced = w.advance();
    let ok = advanced == Advance::Next(vec![UiAction::Identity(IdentityAction::SetUserName("Ada".into()))]) && w.current() == PageKind::Builtin(SetupStep::Providers);
    // The app's answer: the saved identity, now everywhere.
    let ada = Identity { user_name: "Ada".into(), ..crate::identity::current() };
    crate::identity::set(ada.clone());
    w.set_identity(&ada);
    let ok = ok && w.brain.band.isHidden();
    shot(&w, "03-brain-none", ok, &mut failures);
    w.set_providers(statuses(|c| ProviderStatus { ready: c == ProviderChoice::ChatGpt, ..Default::default() }));
    w.choose_provider(ProviderChoice::ChatGpt);
    let selected = |w: &SetupWindow| w.brain.tiles.iter().filter(|(_, t)| t.is_selected()).map(|(c, _)| *c).collect::<Vec<_>>();
    let ok = selected(&w) == [ProviderChoice::ChatGpt] && !w.brain.band.isHidden() && w.brain.rows[0].button.isHidden();
    shot(&w, "03-brain-codex", ok, &mut failures);
    w.choose_provider(ProviderChoice::OpenCodeGo);
    shot(&w, "03-brain-opencode-signin", !w.brain.rows[0].button.isHidden() && w.brain.rows[0].button.label() == "Sign In…", &mut failures);
    w.set_providers(statuses(|c| match c {
        ProviderChoice::ChatGpt => ProviderStatus { ready: true, ..Default::default() },
        ProviderChoice::SplashEngine => ProviderStatus { ready: true, model: Some("Qwen3.8 27B · 4-bit".into()), ..Default::default() },
        _ => ProviderStatus::default(),
    }));
    w.choose_provider(ProviderChoice::SplashEngine);
    let ok = selected(&w) == [ProviderChoice::SplashEngine] && !w.brain.rows[1].label.isHidden();
    shot(&w, "03-brain-local", ok, &mut failures);

    // Listening: 4-bit by default with its RAM and disk; a pick goes on Continue.
    let decider = demo_decider();
    w.set_decider(&decider);
    let advanced = w.advance();
    let checked = |w: &SetupWindow| decider_checked(&w.decider, w.decider_choice.as_deref());
    let ok = advanced == Advance::Next(vec![]) && w.current() == PageKind::Builtin(SetupStep::Decider) && checked(&w) == "4bit"
        && w.listen.seg.selectedSegment() == 0 && w.listen.disk.stringValue().to_string() == "1.4 GB" && w.listen.ram.stringValue().to_string() == "2.4 GB RAM";
    shot(&w, "04-listening", ok, &mut failures);
    let sixteen = w.decider_option_at(2);
    w.choose_decider(sixteen.unwrap_or(9));
    let ok = checked(&w) == "16bit" && w.listen.disk.stringValue().to_string() == "3.9 GB" && leaving_decider(&w.decider, w.decider_choice.as_deref()) == Some(UiAction::SetDeciderPrecision("16bit".into()));
    shot(&w, "04-listening-16bit", ok, &mut failures);
    let mut missing = decider.clone();
    missing.options[1].present = false;
    w.set_decider(&missing);
    let eight = w.decider_option_at(1).unwrap_or(9);
    w.choose_decider(eight);
    shot(&w, "04-listening-8bit-missing", checked(&w) == "16bit" && !w.listen.seg.isEnabledForSegment(1), &mut failures);
    let advanced = w.advance();
    if advanced != Advance::Next(vec![UiAction::SetDeciderPrecision("16bit".into())]) {
        failures.push(format!("listening: Continue sent {advanced:?}"));
    }

    // The voice: Phonon S and its presets; a click picks one.
    let voice = crate::speech_voice::demo_speech_voice(false, crate::speech_voice::LocalVoice::Phonon, false);
    w.set_voice(&voice);
    let ok = w.current() == PageKind::Builtin(SetupStep::Voice) && w.voice_page.tiles.len() == 25
        && w.voice_page.tiles.iter().filter(|(_, t)| t.is_selected()).count() == 1 && w.voice_action(0).is_none()
        && w.voice_action(1) == Some(crate::speech_voice::SpeechVoiceAction::SetPreset("Zoey".into()));
    shot(&w, "05-voice", ok, &mut failures);
    w.advance();

    // Allow: the four rows and Allow All (Return), then all allowed (Continue).
    let ok = w.current() == PageKind::Builtin(SetupStep::Permissions) && !w.allow.isHidden() && w.allow.keyEquivalent().to_string() == "\r" && w.next.keyEquivalent().to_string().is_empty()
        && w.allow_page.title.stringValue().to_string() == "Allow Heimdall";
    shot(&w, "06-allow-fresh", ok, &mut failures);
    let denied: Vec<_> = Permission::ALL.iter().map(|p| (*p, match p { Permission::Microphone => PermissionState::Granted, Permission::ScreenRecording => PermissionState::Denied, Permission::SystemAudio => PermissionState::Unconfirmed, _ => PermissionState::NotDetermined })).collect();
    w.show(&denied);
    let ok = !w.allow_page.rows[2].open.isHidden() && w.allow_page.rows[0].open.isHidden() && !w.allow_page.rows[0].check.isHidden();
    shot(&w, "06-allow-denied", ok, &mut failures);
    w.show(&granted);
    let ok = w.allow.isHidden() && w.next.keyEquivalent().to_string() == "\r" && w.allow_page.rows.iter().all(|r| r.state.stringValue().to_string() == "Allowed");
    shot(&w, "06-allow-granted", ok, &mut failures);

    let home = HomeState { home: user_home.clone(), data: user_home.join(".heimdall"), needs_choice: true, pending: None, error: None };
    w.set_home(&home);
    // Everything already granted, Allow All hidden: Continue sends what Allow All sends, once.
    let advanced = w.advance();
    if advanced != Advance::Next(vec![UiAction::SetupAllowed, UiAction::RequestSystemAudio]) {
        failures.push(format!("allow: Continue with all granted sent {advanced:?}"));
    }
    w.back();
    let again = w.advance();
    if again != Advance::Next(vec![]) {
        failures.push(format!("allow: a second Continue sent {again:?}"));
    }
    let ok = w.current() == PageKind::Builtin(SetupStep::Home) && w.home_page.pending.isHidden();
    shot(&w, "07-home", ok, &mut failures);
    let moving = HomeState {
        pending: Some(HomeMove { to_home: user_home.join("Documents"), to: user_home.join("Documents/.heimdall"), bytes: 2_431_000_000 }),
        ..home.clone()
    };
    w.set_home(&moving);
    let ok = !w.home_page.pending.isHidden() && w.home_page.pending_line.stringValue().to_string() == "Move 2.4 GB to ~/Documents/.heimdall";
    shot(&w, "07-home-move", ok, &mut failures);
    w.set_home(&HomeState { error: Some("~/Documents already has a .heimdall folder; move or remove it first.".into()), ..home.clone() });
    shot(&w, "07-home-error", !w.home_page.error.isHidden(), &mut failures);
    w.set_home(&home);
    let advanced = w.advance();
    if advanced != Advance::Next(vec![UiAction::KeepHome]) {
        failures.push(format!("home: Continue sent {advanced:?}"));
    }

    // The bundles on this Mac, for their real icons (read only).
    let beta = std::path::PathBuf::from("/Applications/T3 Code (Beta).app");
    let found = T3App { path: beta, name: "T3 Code (Beta)".into(), version: "0.1.3".into(), supports_v2: true };
    let t3_states: Vec<(&str, T3Setup, bool)> = vec![
        ("08-t3-missing", T3Setup::default(), true),
        ("08-t3-installing", T3Setup { install: T3Install::Working("Downloading T3 Code…".into()), ..Default::default() }, false),
        ("08-t3-failed", T3Setup { install: T3Install::Failed("The download does not match its checksum.".into()), ..Default::default() }, true),
        ("08-t3-too-old", T3Setup { found: Some(T3App { path: "/Applications/T3 Code (Alpha).app".into(), name: "T3 Code (Alpha)".into(), version: "0.0.45".into(), supports_v2: false }), ..Default::default() }, true),
        ("08-t3-found", T3Setup { found: Some(found.clone()), connect_allowed: false, install: T3Install::Idle }, true),
        ("08-t3-connected", T3Setup { found: Some(found.clone()), connect_allowed: true, install: T3Install::Idle }, false),
    ];
    for (name, state, button) in t3_states {
        w.set_t3(&state);
        let p = &w.t3_page;
        let shown = [&p.found, &p.install, &p.connect].iter().any(|r| !r.row.isHidden() && !r.button.isHidden());
        let ok = w.current() == PageKind::Builtin(SetupStep::T3) && shown == button && (p.connect.row.alphaValue() < 1.0) == !state.ready() && w.next.label() == "Continue";
        // Nothing animates at rest: a hidden row's spinner is stopped and hidden.
        let ok = ok && [&p.found, &p.install, &p.connect].iter().all(|r| !r.row.isHidden() || r.spinner.isHidden());
        shot(&w, name, ok, &mut failures);
    }
    w.advance();

    // Coding Agents (public edition): one row per harness found, the deny list, Set Up (Return) and Leave as Is.
    let deny: Vec<String> = ["No deletes outside your home folder", "No disk erasing", "No Keychain access", "No system settings"].map(String::from).to_vec();
    let row = |name: &str, ready: bool| HarnessRow { name: name.into(), ready, guarded: matches!(name, "Claude Code" | "T3 Code") };
    let rows = |ready: [bool; 5]| -> Vec<HarnessRow> { ["Claude Code", "T3 Code", "Codex", "OpenCode", "Grok"].iter().zip(ready).map(|(n, r)| row(n, r)).collect() };
    let fresh_mac = HarnessSetup { rows: rows([false; 5]), deny: deny.clone(), chosen: None };
    w.set_harnesses(&fresh_mac);
    let no_overlap = |w: &SetupWindow| {
        let (b, l, n) = (w.back.frame(), w.leave.frame(), w.next.frame());
        b.origin.x + b.size.width <= l.origin.x && l.origin.x + l.size.width <= n.origin.x
    };
    let ok = w.current() == PageKind::Builtin(SetupStep::Harnesses) && w.next.label() == "Set Up" && w.next.keyEquivalent().to_string() == "\r" && !w.leave.isHidden() && no_overlap(&w)
        && w.harness_page.rows.iter().all(|r| r.state.stringValue().to_string() == "Asks first") && w.harness_page.rows.iter().filter(|r| !r.shield.isHidden()).count() == 2 && !w.harness_page.deny_band.isHidden();
    shot(&w, "09-agents", ok, &mut failures);
    w.set_harnesses(&HarnessSetup { rows: rows([true; 5]), deny: deny.clone(), chosen: Some(true) });
    let ok = w.next.label() == "Continue" && w.leave.isHidden() && w.harness_page.rows.iter().all(|r| r.state.stringValue().to_string() == "Never asks");
    shot(&w, "09-agents-set", ok, &mut failures);
    w.set_harnesses(&HarnessSetup { chosen: Some(false), ..fresh_mac.clone() });
    let ok = w.leave.keyEquivalent().to_string() == "\r" && w.next.keyEquivalent().to_string().is_empty();
    if !ok {
        failures.push("agents: after Leave as Is, Return is not Leave as Is".into());
    }
    let left = w.leave_harnesses();
    if left != Advance::Next(vec![UiAction::SetHarnessAutonomy(false)]) || w.current() != PageKind::Builtin(SetupStep::Vella) {
        failures.push(format!("agents: Leave as Is sent {left:?}"));
    }

    let vella_app = VellaApp { path: "/Applications/Vella.app".into(), version: "2.1.2".into() };
    w.set_vella(&VellaSetup { found: None, brew: true, install: T3Install::Idle });
    // Add Vella chosen by default (Homebrew here); Not now clears it; only Continue with Add runs the install, and the
    // page waits for it: on when it is done, the reason shown when it fails.
    let missing = VellaSetup { found: None, brew: true, install: T3Install::Idle };
    w.set_vella(&missing);
    let ok = !w.vella_page.not_now.isHidden() && !w.vella_page.not_now.is_selected() && w.vella_page.add.is_selected()
        && w.vella_page.status.stringValue().to_string() == "32 MB" && w.vella_page.spinner.isHidden();
    shot(&w, "10-vella-missing", ok, &mut failures);
    w.set_vella(&VellaSetup { brew: false, ..missing.clone() });
    let ok = w.vella_page.not_now.is_selected() && !w.vella_page.add.is_selected() && w.vella_page.add.label() == "Get Vella…";
    shot(&w, "10-vella-no-homebrew", ok, &mut failures);
    w.set_vella(&missing);
    w.choose_vella(false);
    shot(&w, "10-vella-not-now", w.vella_page.not_now.is_selected() && !w.vella_page.add.is_selected(), &mut failures);
    if w.advance() != Advance::Next(vec![]) {
        failures.push("vella: Continue with Not now sent something".into());
    }
    w.back();
    w.choose_vella(true);
    let waited = w.advance();
    if waited != Advance::Wait(vec![UiAction::InstallVella]) || w.current() != PageKind::Builtin(SetupStep::Vella) {
        failures.push(format!("vella: Continue with Add sent {waited:?}"));
    }
    w.set_vella(&VellaSetup { install: T3Install::Working("Installing Vella with Homebrew…".into()), ..missing.clone() });
    let ok = w.current() == PageKind::Builtin(SetupStep::Vella) && !w.vella_page.spinner.isHidden() && !w.vella_page.progress.isHidden();
    shot(&w, "10-vella-installing", ok, &mut failures);
    w.set_vella(&VellaSetup { install: T3Install::Failed("Error: Cask 'vella' is unavailable".into()), ..missing.clone() });
    let ok = w.current() == PageKind::Builtin(SetupStep::Vella) && w.vella_page.spinner.isHidden() && !w.vella_page.progress.isHidden();
    shot(&w, "10-vella-failed", ok, &mut failures);
    if w.advance() != Advance::Next(vec![]) || w.current() != PageKind::Builtin(SetupStep::Customise) {
        failures.push("vella: Continue after a failure does not move on".into());
    }
    w.back();
    // A second try that finishes: the page moves on by itself.
    w.set_vella(&missing);
    w.advance();
    w.set_vella(&VellaSetup { install: T3Install::Working("Installing Vella with Homebrew…".into()), ..missing.clone() });
    w.set_vella(&VellaSetup { found: Some(vella_app.clone()), brew: true, install: T3Install::Idle });
    if w.current() != PageKind::Builtin(SetupStep::Customise) {
        failures.push("vella: a finished install does not move on".into());
    }
    w.back();
    let ok = w.vella_page.not_now.isHidden() && w.vella_page.add.is_selected() && w.vella_page.status.stringValue().to_string() == "Found Vella 2.1.2" && w.vella_page.spinner.isHidden();
    shot(&w, "10-vella-installed", ok, &mut failures);
    let advanced = w.advance();
    if advanced != Advance::Next(vec![]) {
        failures.push(format!("vella: Continue with Vella installed sent {advanced:?}"));
    }

    // Customise: the hero, the name, the marks, the wake words and the greeting; Start closes Setup.
    let ok = w.current() == PageKind::Builtin(SetupStep::Customise) && w.next.label() == "Start Heimdall" && w.custom.marks.len() == 12
        && w.custom.preview.greeting() == "Hi Ada. I’m Heimdall." && !w.custom.chips[2].isHidden() && w.custom.voice.stringValue().to_string() == "Edwin";
    shot(&w, "11-customise", ok, &mut failures);
    let moon = w.marks().iter().position(|i| *i == Icon::Symbol { name: "moon.stars".into() }).unwrap_or(0);
    let mark = w.mark_action(moon);
    let selene = Identity { icon: Icon::Symbol { name: "moon.stars".into() }, ..ada.clone() };
    crate::identity::set(selene.clone());
    w.set_identity(&selene);
    w.custom.field.setStringValue(&ns("Selene"));
    w.name_typed();
    if w.next.label() != "Start Selene" {
        failures.push(format!("customise: the button says {} while Selene is typed", w.next.label()));
    }
    let ok = mark == Some(UiAction::Identity(IdentityAction::SetIcon(Icon::Symbol { name: "moon.stars".into() })))
        && w.custom.preview.greeting() == "Hi Ada. I’m Selene." && w.custom.wake.stringValue().to_string() == "“Selene”";
    shot(&w, "11-customise-last", ok, &mut failures);
    let closed = w.advance();
    if closed != Advance::Close(vec![UiAction::Identity(IdentityAction::SetName("Selene".into()))]) {
        failures.push(format!("customise: Start sent {closed:?}"));
    }

    // Resized: the window at its minimum and widened; cards stay centred, the footer at the bottom right.
    let right_gap = |w: &SetupWindow, b: &Pad| w.footer.frame().size.width - (b.frame().origin.x + b.frame().size.width);
    for (name, step, size) in [("12-min-voice", SetupStep::Voice, NSSize::new(MIN.0, MIN.1)), ("12-wide-home", SetupStep::Home, NSSize::new(1100.0, 760.0))] {
        let mut r = fresh();
        r.show(&granted);
        r.set_identity(&ada);
        r.set_voice(&voice);
        r.set_home(&moving);
        r.go_to(step);
        let f = r.window.frame();
        r.window.setFrame_display(objc2_foundation::NSRect::new(f.origin, size), false);
        r.voice_page.lay();
        let footer = r.footer.frame();
        let ok = (right_gap(&r, &r.next) - FOOTER_PAD).abs() < 1.0 && (footer.origin.y + footer.size.height - r.window.frame().size.height).abs() < 1.0;
        shot(&r, name, ok, &mut failures);
    }
    crate::identity::set(Identity::default());
    match real_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }

    // The status menu reaches the home step later: one Home Folder… row.
    let state = super::super::offscreen::demo_menu(mtm, |_| {});
    let rows = state.menu.itemArray().iter().filter(|i| i.action() == Some(sel!(openHomeFolder:)) && i.title().to_string() == "Home Folder…").count();
    if rows != 1 {
        failures.push(format!("menu: {rows} Home Folder… rows"));
    }
    // The status menu per edition: only the public one has the updater's row, right above Support the Developer.
    let check = UpdateRow { title: "Check for Updates…".into(), enabled: true };
    for (name, row) in [("menu-personal", None), ("menu-public", Some(check))] {
        let public = row.is_some();
        let state = super::super::offscreen::demo_menu(mtm, move |s| s.update_row = row);
        let titles: Vec<String> = state.menu.itemArray().iter().map(|i| i.title().to_string()).collect();
        let at = titles.iter().position(|t| t == "Check for Updates…");
        let support = titles.iter().position(|t| t == super::super::menu::SUPPORT_TITLE);
        let ok = if public { at.is_some() && at.map(|a| a + 1) == support } else { at.is_none() };
        if !ok {
            failures.push(format!("{name}: the updater's row at {at:?}, Support at {support:?}"));
        }
        let path = dir.join(format!("{name}.png"));
        let container = super::super::offscreen::stacked(&state.menu, super::super::offscreen::MENU_WIDTH, (5.0, 0.5), true, mtm);
        if !super::super::offscreen::capture_rounded(&path, &container, true) {
            failures.push(format!("{name}: not written"));
        }
    }
    failures
}

//! The tool over a fake Accessibility tree (bug note 2026-10-22T19-16-01). No real app, pointer or permission.
use super::fake::{el, Fake};
use super::*;
use serde_json::json;

const ON: Gate = Gate { enabled: true, clicks: false, seeing: true };
const CLICKS: Gate = Gate { enabled: true, clicks: true, seeing: true };
const DISCORD: i32 = 501;

fn req(v: Value) -> Request {
    Request::from_args(&v).unwrap()
}
fn run(f: &Fake, gate: Gate, v: Value) -> Result<String> {
    Computer { backend: f, gate, owner_lines: &[] }.run(&req(v))
}
fn err(f: &Fake, gate: Gate, v: Value) -> String {
    run(f, gate, v).unwrap_err().to_string()
}

/// Discord's member list with "morgan" (a row whose name is its text), a message by morgan, and the message box.
/// A right-click on morgan's row opens a web menu with Profile, Mute (a checkbox item) and Deafen; picking Mute ticks
/// it and closes the menu, as Discord's own menu does.
fn discord() -> Fake {
    let mut f = Fake::new();
    let window = el(2, "AXWindow", "#general | Friends - Discord").at(0.0, 0.0, 1200.0, 800.0).with(vec![
        el(3, "AXGroup", "").described("Members").at(1000.0, 0.0, 200.0, 800.0).with(vec![
            el(4, "AXGroup", "").at(1000.0, 100.0, 200.0, 40.0).with(vec![el(5, "AXStaticText", "").value("morgan").at(1040.0, 110.0, 80.0, 20.0)]),
            el(6, "AXGroup", "").at(1000.0, 140.0, 200.0, 40.0).with(vec![el(7, "AXStaticText", "").value("anna").at(1040.0, 150.0, 80.0, 20.0)]),
        ]),
        el(8, "AXTextArea", "").described("Message #general").at(100.0, 740.0, 800.0, 40.0),
        el(9, "AXCheckBox", "Notifications").value("0").at(100.0, 10.0, 20.0, 20.0),
    ]);
    let root = el(1, "AXApplication", "Discord").with(vec![window]);
    f.app("Discord", "com.hnc.Discord", DISCORD, root)
        .app("Heimdall", "dev.heimdall.app", 77, el(100, "AXApplication", "Heimdall"))
        .app("System Settings", "com.apple.systempreferences", 78, el(200, "AXApplication", "System Settings"))
        .app("Terminal", "com.apple.Terminal", 79, el(300, "AXApplication", "Terminal"));
    f.give(4, &["AXPress", "AXShowMenu"]).give(6, &["AXPress", "AXShowMenu"]).give(8, &["AXShowMenu"]).give(9, &["AXPress"]);
    f.give(21, &["AXPress"]).give(22, &["AXPress"]).give(23, &["AXPress"]);
    f.on(4, "AXShowMenu", |root| {
        root.children[0].children.push(el(20, "AXMenu", "").at(1010.0, 120.0, 180.0, 90.0).with(vec![
            el(21, "AXMenuItem", "Profile").at(1010.0, 120.0, 180.0, 30.0),
            el(22, "AXMenuItem", "Mute").value("0").at(1010.0, 150.0, 180.0, 30.0),
            el(23, "AXMenuItem", "Deafen").value("0").at(1010.0, 180.0, 180.0, 30.0),
        ]));
    });
    f.on(22, "AXPress", |root| {
        // Discord records the mute and closes the menu.
        root.children[0].children.retain(|c| c.role != "AXMenu");
    });
    f.on(9, "AXPress", |root| {
        let b = &mut root.children[0].children[2];
        b.value = if b.value == "1" { "0".into() } else { "1".into() };
    });
    f
}

#[test]
fn mutes_a_discord_user_through_the_menu_and_reads_it_back() {
    // The 22 Oct request: right-click morgan, tick Mute.
    let f = discord();
    let opened = run(&f, ON, json!({"action":"menu","app":"Discord","title":"morgan"})).unwrap();
    assert_eq!(opened, "Opened the menu of group “morgan” in Discord: Profile, Mute, Deafen. Pick one with action pick and its title; it stays open until then.");
    let picked = run(&f, ON, json!({"action":"pick","app":"discord","title":"mute"})).unwrap();
    assert_eq!(picked, "Picked menu item “Mute” in Discord; the menu closed. It showed no tick before.");
    // Accessibility only: the row got AXShowMenu and the item AXPress; nothing clicked.
    let calls = f.calls();
    assert!(calls.contains(&"AXShowMenu 4".to_string()) && calls.contains(&"AXPress 22".to_string()), "{calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("click")), "{calls:?}");
}

#[test]
fn a_menu_that_stays_open_reads_the_tick_back() {
    let mut f = discord();
    f.on(22, "AXPress", |root| {
        if let Some(menu) = root.children[0].children.iter_mut().find(|c| c.role == "AXMenu") {
            menu.children[1].value = "1".into();
        }
    });
    run(&f, ON, json!({"action":"menu","app":"Discord","title":"morgan"})).unwrap();
    let picked = run(&f, ON, json!({"action":"pick","app":"Discord","title":"Mute"})).unwrap();
    assert_eq!(picked, "Picked menu item “Mute” in Discord; the menu is still open and it now reads ticked. It showed no tick before.");
}

#[test]
fn pick_without_an_open_menu_says_to_open_one() {
    let f = discord();
    let e = err(&f, ON, json!({"action":"pick","app":"Discord","title":"Mute"}));
    assert!(e.starts_with("NOT_FOUND:") && e.contains("no menu is open. Open one first with action menu"), "{e}");
}

#[test]
fn a_missing_element_names_the_closest_matches() {
    let f = discord();
    let e = err(&f, ON, json!({"action":"menu","app":"Discord","title":"morgn"}));
    assert!(e.starts_with("NOT_FOUND: no “morgn” in Discord's windows. Closest: text “morgan”"), "{e}");
    let e = err(&f, ON, json!({"action":"press","app":"Discord","title":"morgan","role":"button"}));
    assert!(e.contains("Closest: text “morgan”") && e.contains("(drop role to match any kind)"), "{e}");
}

#[test]
fn ambiguous_titles_list_the_candidates_and_index_picks_one() {
    let mut f = discord();
    // morgan also wrote a message.
    f.trees.borrow_mut().get_mut(&DISCORD).unwrap().children[0].children.push(
        el(30, "AXGroup", "").at(100.0, 300.0, 600.0, 40.0).with(vec![el(31, "AXStaticText", "").value("morgan").at(110.0, 300.0, 60.0, 20.0)]),
    );
    f.give(30, &["AXShowMenu"]);
    let e = err(&f, ON, json!({"action":"menu","app":"Discord","title":"morgan"}));
    assert!(e.starts_with("AMBIGUOUS: 2 elements match “morgan” in Discord; pass index: 1. group") && e.contains("2. group"), "{e}");
    let ok = run(&f, ON, json!({"action":"menu","app":"Discord","title":"morgan","index":2})).unwrap();
    assert!(f.calls().contains(&"AXShowMenu 30".to_string()), "{ok}");
    let e = err(&f, ON, json!({"action":"menu","app":"Discord","title":"morgan","index":3}));
    assert!(e.starts_with("INVALID_ARGUMENT: index: 3, but 2 match"), "{e}");
}

#[test]
fn a_row_and_its_own_text_are_one_match() {
    // morgan's row (pressable) and its text both match; the row wins and nothing is ambiguous.
    let f = discord();
    run(&f, ON, json!({"action":"press","app":"Discord","title":"morgan"})).unwrap();
    assert!(f.calls().contains(&"AXPress 4".to_string()), "{:?}", f.calls());
}

#[test]
fn press_reads_a_checkbox_back() {
    let f = discord();
    let out = run(&f, ON, json!({"action":"press","app":"Discord","title":"Notifications","role":"checkbox"})).unwrap();
    assert_eq!(out, "Pressed checkbox “Notifications” in Discord; it now reads on (was off).");
}

#[test]
fn off_by_default_and_without_accessibility_nothing_is_touched() {
    let f = discord();
    assert!(!gate::enabled() && !gate::clicks(), "both switches start off");
    let e = err(&f, Gate { enabled: false, clicks: true, seeing: true }, json!({"action":"find","app":"Discord"}));
    assert!(e.starts_with("COMPUTER_OFF:") && e.contains("Control Apps"), "{e}");
    let mut untrusted = discord();
    untrusted.trusted = false;
    let e = err(&untrusted, ON, json!({"action":"find","app":"Discord"}));
    assert!(e.starts_with("ACCESSIBILITY_MISSING:") && e.contains("Privacy & Security, Accessibility"), "{e}");
    assert!(f.calls().is_empty() && untrusted.calls().is_empty(), "no tree was read");
}

#[test]
fn heimdall_system_settings_and_terminals_are_refused() {
    let f = discord();
    for (app, why) in [("Heimdall", "Heimdall itself"), ("System Settings", "system settings"), ("Terminal", "a terminal")] {
        let e = err(&f, ON, json!({"action":"find","app":app}));
        assert!(e.starts_with(&format!("REFUSED: {app} is {why}")), "{e}");
    }
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    for (name, id) in [("FloatingIslands", "dev.heimdall.islands"), ("Keychain Access", "com.apple.keychainaccess"), ("1Password", "com.1password.1password"), ("iTerm2", "com.googlecode.iterm2"), ("Heimdall Alpha", "com.tobynoskillson.heimdall")] {
        assert!(refused_app(&AppInfo { name: name.into(), bundle_id: id.into(), pid: 9, active: false }).is_some(), "{name}");
    }
    assert!(refused_app(&AppInfo { name: "Discord".into(), bundle_id: "com.hnc.Discord".into(), pid: 9, active: false }).is_none());
}

#[test]
fn password_and_payment_windows_and_spending_buttons_are_refused() {
    let mut f = Fake::new();
    let login = el(2, "AXWindow", "Sign in").at(0.0, 0.0, 400.0, 300.0).with(vec![
        el(3, "AXTextField", "").described("Email").at(10.0, 10.0, 200.0, 20.0),
        el(4, "AXTextField", "").sub("AXSecureTextField").described("Password").at(10.0, 40.0, 200.0, 20.0),
        el(5, "AXButton", "Continue").at(10.0, 70.0, 80.0, 20.0),
    ]);
    let pay = el(6, "AXWindow", "Order").at(500.0, 0.0, 400.0, 300.0).with(vec![
        el(7, "AXTextField", "").described("Card number").at(510.0, 10.0, 200.0, 20.0),
        el(8, "AXButton", "Next").at(510.0, 70.0, 80.0, 20.0),
    ]);
    let shop = el(9, "AXWindow", "Store").at(0.0, 400.0, 400.0, 300.0).with(vec![el(10, "AXButton", "Buy now").at(10.0, 410.0, 80.0, 20.0), el(11, "AXButton", "Display").at(100.0, 410.0, 80.0, 20.0)]);
    let billing = el(12, "AXWindow", "Billing details").at(500.0, 400.0, 400.0, 300.0).with(vec![el(13, "AXButton", "Save").at(510.0, 410.0, 80.0, 20.0)]);
    f.app("Shop", "com.example.shop", 9, el(1, "AXApplication", "Shop").with(vec![login, pay, shop, billing]));
    for h in [3, 4, 5, 8, 10, 11, 13] {
        f.give(h, &["AXPress"]);
    }
    let e = err(&f, ON, json!({"action":"press","app":"Shop","title":"Continue"}));
    assert!(e.starts_with("REFUSED: button “Continue”: its window has a password field"), "{e}");
    let e = err(&f, ON, json!({"action":"type","app":"Shop","title":"Password","text":"hunter2"}));
    assert!(e.starts_with("REFUSED: password field “Password” is a password field"), "{e}");
    let e = err(&f, ON, json!({"action":"press","app":"Shop","title":"Next"}));
    assert!(e.contains("its window is a payment form"), "{e}");
    let e = err(&f, ON, json!({"action":"press","app":"Shop","title":"Buy now"}));
    assert!(e.starts_with("REFUSED: button “Buy now” would buy or pay"), "{e}");
    let e = err(&f, ON, json!({"action":"press","app":"Shop","title":"Save"}));
    assert!(e.contains("its window “Billing details” is about passwords or payment"), "{e}");
    // "Display" holds "pay" only inside a word: allowed.
    run(&f, ON, json!({"action":"press","app":"Shop","title":"Display"})).unwrap();
    assert!(!f.calls().iter().any(|c| c.starts_with("AXPress") && c != "AXPress 11"), "{:?}", f.calls());
    // find shows them, marked.
    let found = run(&f, ON, json!({"action":"find","app":"Shop","role":"button"})).unwrap();
    assert!(found.contains("button “Buy now” in “Store” at 50,420 (80×20) (buys or pays: refused)"), "{found}");
}

#[test]
fn type_inserts_at_the_caret_and_never_presses_return() {
    let f = discord();
    let out = run(&f, ON, json!({"action":"type","app":"Discord","title":"Message #general","text":"hello"})).unwrap();
    assert_eq!(out, "Typed 5 characters at the caret in text area “Message #general” in Discord; it now reads “hello”. Nothing was sent: no Return was pressed.");
    assert!(f.calls().contains(&"focus 8".to_string()));
    // Without a title: the focused field.
    let out = run(&f, ON, json!({"action":"type","app":"Discord","text":" there"})).unwrap();
    assert!(out.contains("it now reads “hello there”"), "{out}");
}

#[test]
fn type_falls_back_to_appending_when_the_caret_cannot_be_set() {
    let mut f = discord();
    f.no_selected_text = vec![8];
    let out = run(&f, ON, json!({"action":"type","app":"Discord","title":"Message #general","text":"hi"})).unwrap();
    assert!(out.starts_with("Typed 2 characters at the end"), "{out}");
}

#[test]
fn type_with_no_focus_or_into_a_button_names_the_fix() {
    let f = discord();
    let e = err(&f, ON, json!({"action":"type","app":"Discord","text":"x"}));
    assert!(e.starts_with("NO_FOCUS:"), "{e}");
    let e = err(&f, ON, json!({"action":"type","app":"Discord","title":"Notifications","text":"x"}));
    assert!(e.starts_with("NOT_A_FIELD: checkbox “Notifications” is not a text field"), "{e}");
}

#[test]
fn set_reads_the_value_back() {
    let f = discord();
    let out = run(&f, ON, json!({"action":"set","app":"Discord","title":"Message #general","text":"draft"})).unwrap();
    assert_eq!(out, "Set text area “Message #general” in Discord; it now reads “draft”.");
}

#[test]
fn pointer_clicks_are_off_unless_allowed_and_wait_for_owner_to_be_still() {
    let f = discord();
    let e = err(&f, ON, json!({"action":"click","app":"Discord","x":500,"y":300}));
    assert!(e.starts_with("CLICKS_OFF:"), "{e}");
    let mut busy = discord();
    busy.idle = 0.4;
    let e = err(&busy, CLICKS, json!({"action":"click","app":"Discord","x":500,"y":300}));
    assert!(e.starts_with("BUSY: {user} touched the mouse or keyboard 0.4 s ago"), "{e}");
    let e = err(&f, CLICKS, json!({"action":"click","app":"Discord","x":5000,"y":300}));
    assert!(e.starts_with("OUTSIDE:"), "{e}");
    let out = run(&f, CLICKS, json!({"action":"click","app":"Discord","x":1050,"y":115,"button":"right"})).unwrap();
    assert!(out.starts_with("Right-clicked text “morgan” in Discord at 1050,115, sent to the app only; the pointer did not move."), "{out}");
    assert!(f.calls().contains(&format!("click {DISCORD} 1050,115 right")));
    assert!(!busy.calls().iter().any(|c| c.starts_with("click")));
}

#[test]
fn an_element_without_the_action_falls_back_to_a_click_only_when_allowed() {
    let mut f = discord();
    f.actions.remove(&4);
    f.actions.remove(&6);
    let e = err(&f, ON, json!({"action":"menu","app":"Discord","title":"morgan"}));
    assert!(e.starts_with("NO_ACTION: text “morgan” cannot be opened through Accessibility (its actions: none), and pointer clicks are off"), "{e}");
    let out = run(&f, CLICKS, json!({"action":"menu","app":"Discord","title":"morgan"})).unwrap();
    assert!(out.starts_with("Right-clicked text “morgan”"), "{out}");
    assert!(f.calls().contains(&format!("click {DISCORD} 1080,120 right")), "{:?}", f.calls());
}

#[test]
fn find_lists_named_controls_and_matches() {
    let f = discord();
    let all = run(&f, ON, json!({"action":"find","app":"Discord"})).unwrap();
    assert!(all.starts_with("Discord: 3 named controls."), "{all}");
    assert!(all.contains("text area “Message #general”, text “”") && all.contains("checkbox “Notifications”, off"), "{all}");
    let one = run(&f, ON, json!({"action":"find","app":"Discord","title":"anna"})).unwrap();
    assert_eq!(one, "Discord: 1 match.\n1. text “anna” in “#general | Friends - Discord” at 1080,160 (80×20)");
}

#[test]
fn unknown_and_ambiguous_apps_list_what_is_running() {
    let f = discord();
    let e = err(&f, ON, json!({"action":"find","app":"Slack"}));
    assert!(e.starts_with("APP_NOT_FOUND: no running app named “Slack”. Running: ") && e.contains("Discord"), "{e}");
    assert!(!e.contains("Heimdall") && !e.contains("Terminal"), "refused apps are not offered: {e}");
    // A unique prefix is enough.
    run(&f, ON, json!({"action":"find","app":"disc"})).unwrap();
}

#[test]
fn arguments_are_checked() {
    for (args, want) in [
        (json!({"action":"press","app":"Discord"}), "INVALID_ARGUMENT: title"),
        (json!({"action":"set","app":"Discord","title":"x"}), "INVALID_ARGUMENT: text"),
        (json!({"action":"type","app":"Discord"}), "INVALID_ARGUMENT: text"),
        (json!({"action":"click","app":"Discord"}), "INVALID_ARGUMENT: click needs"),
        (json!({"action":"press","app":"Discord","title":"x","x":1,"y":2}), "INVALID_ARGUMENT: x and y are for click only"),
        (json!({"action":"click","app":"Discord","x":1}), "INVALID_ARGUMENT: x and y go together"),
        (json!({"action":"jump","app":"Discord"}), "INVALID_ARGUMENT: action"),
        (json!({"action":"find","app":" "}), "INVALID_ARGUMENT: app"),
    ] {
        let e = Request::from_args(&args).unwrap_err().to_string();
        assert!(e.starts_with(want), "{args}: {e}");
    }
    let f = discord();
    let e = err(&f, ON, json!({"action":"find","app":"Discord","role":"spaceship"}));
    assert!(e.starts_with("INVALID_ARGUMENT: role: “spaceship” is not one of button"), "{e}");
    run(&f, ON, json!({"action":"find","app":"Discord","role":"AXCheckBox"})).unwrap();
    run(&f, ON, json!({"action":"find","app":"Discord","role":"text_area"})).unwrap();
}

#[test]
fn disabled_elements_are_not_pressed() {
    let f = discord();
    f.trees.borrow_mut().get_mut(&DISCORD).unwrap().children[0].children[2].enabled = false;
    let e = err(&f, ON, json!({"action":"press","app":"Discord","title":"Notifications"}));
    assert!(e.starts_with("DISABLED:"), "{e}");
    assert!(!f.calls().iter().any(|c| c.starts_with("AXPress")));
}

#[test]
fn a_closed_native_menu_is_not_read_as_open() {
    // Found in the real test (10 Nov): a native NSMenu's items still read back after the menu closed, unticked, while
    // the app had ticked Mute. The menu's absence from the app's tree decides.
    let mut f = discord();
    f.on(22, "AXPress", |root| {
        if let Some(menu) = root.children[0].children.iter_mut().find(|c| c.role == "AXMenu") {
            menu.role = "AXGroup".into(); // closed: no longer a menu in the tree, its item still there
        }
    });
    run(&f, ON, json!({"action":"menu","app":"Discord","title":"morgan"})).unwrap();
    let picked = run(&f, ON, json!({"action":"pick","app":"Discord","title":"Mute"})).unwrap();
    assert_eq!(picked, "Picked menu item “Mute” in Discord; the menu closed. It showed no tick before.");
}

#[test]
fn a_field_is_named_by_its_label_not_its_text() {
    let f = discord();
    let out = run(&f, ON, json!({"action":"set","app":"Discord","title":"Message #general","text":"draft"})).unwrap();
    assert!(out.starts_with("Set text area “Message #general”"), "{out}");
    let again = run(&f, ON, json!({"action":"set","app":"Discord","title":"Message #general","text":"two"})).unwrap();
    assert!(again.starts_with("Set text area “Message #general”"), "named by its label once it holds text: {again}");
}

// ---- review 10 Nov ----

#[test]
fn line_breaks_are_never_typed_or_set() {
    for text in ["ls\n", "a\rb", "x\u{2028}"] {
        let e = Request::from_args(&json!({"action":"type","app":"Discord","text":text})).unwrap_err().to_string();
        assert!(e.starts_with("INVALID_ARGUMENT: text: one line only"), "{e}");
    }
}

#[test]
fn editors_agents_finder_and_activity_monitor_are_refused() {
    for (name, id) in [
        ("Visual Studio Code", "com.microsoft.VSCode"), ("Cursor", "com.todesktop.230313mzl4w4u92"), ("Zed", "dev.zed.Zed"),
        ("T3 Code", "com.t3tools.t3code"), ("Claude", "com.anthropic.claudefordesktop"), ("ChatGPT", "com.openai.chat"),
        ("Finder", "com.apple.finder"), ("Activity Monitor", "com.apple.ActivityMonitor"), ("Hyper", "co.zeit.hyper"),
    ] {
        assert!(refused_app(&AppInfo { name: name.into(), bundle_id: id.into(), pid: 9, active: false }).is_some(), "{name}");
    }
}

#[test]
fn order_and_price_buttons_spend() {
    for label in ["Place your order", "Complete order", "$4.99", "Get for 4,99 zł", "€10", "12 PLN", "Rent"] {
        assert!(spends(label), "{label}");
    }
    for label in ["Mute", "Display", "Ordering", "Version 4.2", "Payload"] {
        assert!(!spends(label), "{label}");
    }
    assert!(sensitive_title("Place Your Order - Amazon.com") && sensitive_title("Your Cart"));
}

#[test]
fn an_unconfirmed_press_is_reported_as_sent_not_failed() {
    let mut f = discord();
    f.unconfirmed = vec![9];
    let out = run(&f, ON, json!({"action":"press","app":"Discord","title":"Notifications"})).unwrap();
    assert!(out.starts_with("Sent to checkbox “Notifications” in Discord, but the app did not confirm in time") && out.contains("a second press could undo it"), "{out}");
}

#[test]
fn a_cut_tree_refuses_writing_and_clicking_but_not_reading() {
    let mut f = discord();
    f.truncated = true;
    let e = err(&f, ON, json!({"action":"type","app":"Discord","title":"Message #general","text":"hi"}));
    assert!(e.starts_with("TOO_LARGE:"), "{e}");
    let e = err(&f, ON, json!({"action":"set","app":"Discord","title":"Message #general","text":"hi"}));
    assert!(e.starts_with("TOO_LARGE:"), "{e}");
    assert!(run(&f, ON, json!({"action":"find","app":"Discord","title":"morgan"})).unwrap().contains("some elements may be missing"));
    run(&f, ON, json!({"action":"press","app":"Discord","title":"Notifications"})).unwrap();
}

#[test]
fn a_payment_fields_text_is_not_read_back() {
    let mut f = Fake::new();
    let pay = el(2, "AXWindow", "Order").at(0.0, 0.0, 400.0, 300.0).with(vec![el(3, "AXTextField", "").described("Card number").value("4111 1111 1111 1111").at(10.0, 10.0, 200.0, 20.0)]);
    f.app("Shop", "com.example.shop", 9, el(1, "AXApplication", "Shop").with(vec![pay]));
    let out = run(&f, ON, json!({"action":"find","app":"Shop","role":"field"})).unwrap();
    assert!(!out.contains("4111"), "{out}");
}


const BLIND: Gate = Gate { enabled: true, clicks: false, seeing: false };

fn said(f: &Fake, gate: Gate, owner: &[&str], v: Value) -> Result<String> {
    let owner: Vec<String> = owner.iter().map(|s| s.to_string()).collect();
    Computer { backend: f, gate, owner_lines: &owner }.run(&req(v))
}

/// A mail app's window: Send, Delete, Reply, Share and an Allow switch, a Send icon inside its button, and a harmless
/// Mute button.
fn mail() -> Fake {
    let mut f = Fake::new();
    let window = el(2, "AXWindow", "Draft to Anna").at(0.0, 0.0, 800.0, 600.0).with(vec![
        el(3, "AXButton", "Send").at(10.0, 10.0, 60.0, 30.0).with(vec![el(4, "AXImage", "").at(20.0, 15.0, 20.0, 20.0)]),
        el(5, "AXButton", "").described("Move to Trash").at(80.0, 10.0, 60.0, 30.0),
        el(6, "AXButton", "Reply All").at(150.0, 10.0, 60.0, 30.0),
        el(7, "AXButton", "Share").at(220.0, 10.0, 60.0, 30.0),
        el(8, "AXCheckBox", "Allow remote content").value("0").at(290.0, 10.0, 20.0, 20.0),
        el(9, "AXButton", "Mute thread").at(330.0, 10.0, 60.0, 30.0),
        el(10, "AXButton", "Install update").at(400.0, 10.0, 60.0, 30.0),
        el(11, "AXButton", "Post").at(470.0, 10.0, 60.0, 30.0),
        el(12, "AXButton", "Approve").at(540.0, 10.0, 60.0, 30.0),
        el(13, "AXButton", "Order").at(610.0, 10.0, 60.0, 30.0),
        el(14, "AXTextArea", "").described("Reply").at(10.0, 100.0, 700.0, 300.0),
    ]);
    f.app("Mail", "com.apple.mail", 600, el(1, "AXApplication", "Mail").with(vec![window]));
    for h in [3, 5, 6, 7, 8, 9, 10, 11, 12, 13] {
        f.give(h, &["AXPress"]);
    }
    f
}

#[test]
fn outward_buttons_are_refused_without_owners_words() {
    let f = mail();
    for (title, what) in [
        ("Send", "send"), ("Move to Trash", "delete"), ("Reply All", "send"), ("Share", "post"), ("Post", "post"),
        ("Allow remote content", "approve"), ("Install update", "install"), ("Approve", "approve"), ("Order", "buy"),
    ] {
        let e = err(&f, ON, json!({"action":"press","app":"Mail","title":title}));
        assert!(e.starts_with("REFUSED:") && e.contains(&format!("would {what} something")) && e.contains("owner_asked"), "{title}: {e}");
    }
    // A switch set to on approves as a press would.
    let e = err(&f, ON, json!({"action":"set","app":"Mail","title":"Allow remote content","text":"1"}));
    assert!(e.starts_with("REFUSED:"), "{e}");
    assert!(!f.calls().iter().any(|c| c.starts_with("AXPress") || c.starts_with("set")), "{:?}", f.calls());
    // A harmless button, and typing into the reply field (nothing is sent without Return), still work.
    run(&f, ON, json!({"action":"press","app":"Mail","title":"Mute thread"})).unwrap();
    run(&f, ON, json!({"action":"type","app":"Mail","title":"Reply","text":"See you then"})).unwrap();
}

#[test]
fn a_click_on_an_outward_buttons_icon_is_refused() {
    let f = mail();
    let e = err(&f, CLICKS, json!({"action":"click","app":"Mail","x":30,"y":25}));
    assert!(e.starts_with("REFUSED: button “Send” would send something"), "{e}");
    assert!(!f.calls().iter().any(|c| c.starts_with("click")), "{:?}", f.calls());
}

#[test]
fn an_outward_button_is_pressed_when_owners_own_words_name_it() {
    let f = mail();
    let owner = ["Yes, press Send on the draft to Anna"];
    let out = said(&f, ON, &owner, json!({"action":"press","app":"Mail","title":"Send","owner_asked":"press Send on the draft"})).unwrap();
    assert!(out.starts_with("Pressed button “Send” in Mail"), "{out}");
    // Not his words, words that do not name this label, a negated ask, or no quote at all: refused.
    for (owner, quote) in [
        (&["send the draft please"][..], Some("press Send")),
        (&["delete it"][..], Some("delete it")),
        (&["don't press Send yet"][..], Some("press Send yet")),
        (&["press Send"][..], None),
    ] {
        let mut args = json!({"action":"press","app":"Mail","title":"Send"});
        if let Some(q) = quote {
            args["owner_asked"] = q.into();
        }
        let e = said(&f, ON, owner, args).unwrap_err().to_string();
        assert!(e.starts_with("REFUSED:"), "{owner:?} {quote:?}: {e}");
    }
    // The whole label: "delete" does not name “Move to Trash”.
    let e = said(&f, ON, &["delete that email"], json!({"action":"press","app":"Mail","title":"Move to Trash","owner_asked":"delete that email"})).unwrap_err().to_string();
    assert!(e.starts_with("REFUSED:"), "{e}");
    said(&f, ON, &["ok, move to trash"], json!({"action":"press","app":"Mail","title":"Move to Trash","owner_asked":"move to trash"})).unwrap();
}

#[test]
fn outward_menu_items_need_owners_words_too() {
    let mut f = discord();
    f.on(4, "AXShowMenu", |root| {
        root.children[0].children.push(el(20, "AXMenu", "").at(1010.0, 120.0, 180.0, 90.0).with(vec![
            el(21, "AXMenuItem", "Mute").at(1010.0, 120.0, 180.0, 30.0),
            el(22, "AXMenuItem", "Delete Message").at(1010.0, 150.0, 180.0, 30.0),
            el(23, "AXMenuItem", "Kick morgan").at(1010.0, 180.0, 180.0, 30.0),
        ]));
    });
    run(&f, ON, json!({"action":"menu","app":"Discord","title":"morgan"})).unwrap();
    for item in ["Delete Message", "Kick morgan"] {
        let e = err(&f, ON, json!({"action":"pick","app":"Discord","title":item}));
        assert!(e.starts_with("REFUSED:"), "{item}: {e}");
    }
    run(&f, ON, json!({"action":"pick","app":"Discord","title":"Mute"})).unwrap();
}

#[test]
fn launchers_installers_disk_utility_arsenal_and_the_x_browser_are_refused() {
    for (name, id) in [
        ("Disk Utility", "com.apple.DiskUtility"), ("Installer", "com.apple.installer"), ("App Store", "com.apple.AppStore"),
        ("Raycast", "com.raycast.macos"), ("Alfred 5", "com.runningwithcrayons.Alfred"), ("Keyboard Maestro", "com.stairways.keyboardmaestro.editor"),
        ("Hammerspoon", "org.hammerspoon.Hammerspoon"), ("Windsurf", "com.exafunction.windsurf"), ("Arsenal", "dev.arsenal.app"),
        ("ego lite", "com.citrolabs.ego.lite"), ("Proton Pass", "me.proton.pass.electron"),
    ] {
        assert!(refused_app(&AppInfo { name: name.into(), bundle_id: id.into(), pid: 9, active: false }).is_some(), "{name}");
        // By name alone too (a lab build or an unknown id).
        assert!(refused_app(&AppInfo { name: name.into(), bundle_id: "x.unknown".into(), pid: 9, active: false }).is_some(), "{name} by name");
    }
    for (name, id) in [("Discord", "com.hnc.Discord"), ("Mail", "com.apple.mail"), ("Safari", "com.apple.Safari")] {
        assert!(refused_app(&AppInfo { name: name.into(), bundle_id: id.into(), pid: 9, active: false }).is_none(), "{name}");
    }
}

#[test]
fn seeing_off_refuses_find_and_reads_back_only_the_acted_element() {
    let f = discord();
    let e = err(&f, BLIND, json!({"action":"find","app":"Discord"}));
    assert!(e.starts_with("SEEING_OFF:"), "{e}");
    let e = err(&f, BLIND, json!({"action":"find","app":"Discord","title":"morgan"}));
    assert!(e.starts_with("SEEING_OFF:"), "{e}");
    assert!(f.calls().is_empty(), "no tree was read: {:?}", f.calls());
    // The mute still works, by the names the user gave; the menu's items are not read out.
    let opened = run(&f, BLIND, json!({"action":"menu","app":"Discord","title":"morgan"})).unwrap();
    assert_eq!(opened, "Opened the menu of group “morgan” in Discord (3 items; Seeing is off, so they are not read). Pick the one {user} named with action pick; it stays open until then.");
    let picked = run(&f, BLIND, json!({"action":"pick","app":"Discord","title":"Mute"})).unwrap();
    assert!(picked.starts_with("Picked menu item “Mute” in Discord"), "{picked}");
    // A miss names no other element; an ambiguous title lists kinds and places, no text.
    let e = err(&f, BLIND, json!({"action":"press","app":"Discord","title":"morgn"}));
    assert!(e.starts_with("NOT_FOUND:") && !e.contains("morgan") && e.contains("Seeing is off"), "{e}");
    let e = err(&f, BLIND, json!({"action":"menu","app":"Discord","role":"text"}));
    assert!(e.starts_with("AMBIGUOUS:") && !e.contains("morgan") && !e.contains("anna") && !e.contains("#general"), "{e}");
}

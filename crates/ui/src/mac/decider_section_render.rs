//! `heimdall-ui-demo --decider-section <dir>`: the Decider section, driven through the real State
//! and menu builder and rendered offscreen at 2× (nothing on screen), with the checks behind it:
//! - at rest the header names the precision that runs (grey), the version is the caption, the running row rests lit;
//! - a click on another precision marks it and shows Confirm, sending nothing; the current one, or the menu closing,
//!   cancels; Confirm sends one `SetDeciderPrecision` and the header spins on the target until the app says it answered;
//! - swapping takes no clicks; a failed swap shows its reason in red under the version (rolled back: what runs, grey;
//!   nothing runs: "Failed" in red); a missing folder greys its row and says which precision runs instead;
//! - the section folds like the others and a rebuilt menu keeps it folded;
//! - Setup's Decider step at real size: 4-bit checked, an 8-bit pick, a missing 16-bit.
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSMenu};
use objc2_foundation::MainThreadMarker;

use super::controls::StatusTone;
use super::engine_render::shoot_wide;
use super::offscreen::{demo_menu, full_menu};
use crate::model::{DeciderState, DeciderStatus, MenuSection, UiAction, UiUpdate};

type Sent = Rc<RefCell<Vec<UiAction>>>;

fn open_state(mtm: MainThreadMarker, d: DeciderStatus, setup: impl FnOnce(&mut super::State)) -> (super::State, Sent) {
    let mut state = demo_menu(mtm, |s| {
        let all = crate::Permission::ALL.iter().map(|p| (*p, crate::PermissionState::Granted)).collect();
        s.apply(UiUpdate::Permissions(all));
        s.decider = d;
        // The section under test open (one big section open at a time).
        s.settings_value.collapsed = MenuSection::ALL.iter().copied().filter(|x| *x != MenuSection::Decider).collect();
        setup(s);
    });
    let sent: Sent = Default::default();
    let log = sent.clone();
    state.on_action = Box::new(move |a| log.borrow_mut().push(a));
    state.menu_open = true;
    super::menu::sync_tracking(&mut state);
    (state, sent)
}

/// The Decider section's rows (header to Confirm) as a menu of their own for a render.
fn section_menu(state: &super::State, mtm: MainThreadMarker) -> objc2::rc::Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    let Some((header, items)) = state.sections.get(&MenuSection::Decider) else { return menu };
    let all: Vec<_> = header.enclosingMenuItem().into_iter().chain(items.iter().cloned()).collect();
    for item in all {
        if let Some(parent) = unsafe { item.menu() } { parent.removeItem(&item); }
        menu.addItem(&item);
    }
    menu
}

fn status(state: DeciderState, chosen: &str, running: Option<&str>) -> DeciderStatus {
    DeciderStatus { chosen: chosen.into(), running: running.map(Into::into), state, ..super::setup::demo_decider() }
}

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--decider-section runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let renders = dir.join("renders");
    std::fs::create_dir_all(&renders).ok();
    super::controls::STILL_SPINNERS.with(|c| c.set(true));
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok { failures.push(name.to_owned()) }
    };
    // Each shot lays the menu again first: a render takes the section's items out of the menu it was built in.
    let shoot = |name: &str, state: &mut super::State, check: &mut dyn FnMut(&str, bool, String)| {
        state.rebuild_menu();
        state.menu_open = true;
        super::menu::sync_tracking(state);
        if let Some((item, view)) = &state.decider_rows.confirm { if !item.isHidden() { view.set_still(0.8); } }
        let path = renders.join(format!("{name}.png"));
        check(&format!("render {}", path.display()), shoot_wide(&path, &section_menu(state, mtm), mtm), String::new());
    };
    // The precision the header names (climb 2: after the version, "V5 · 4-bit").
    // The precision and tone the header gives (climb 2: open, the header shows the version alone and the lit row the
    // precision; the look is what folding shows after "V5 · "). Failures and spinners show as they are.
    let header = |s: &super::State| s.decider_rows.header.as_ref().and_then(|h| h.status()).map(|(t, tone)| {
        if tone == StatusTone::Grey { (super::decider_section::look(&s.decider, s.decider_pending.as_deref()).header.0, tone) } else { (t, tone) }
    });
    let spins = |s: &super::State| s.decider_rows.header.as_ref().and_then(|h| h.status_spinner()).is_some();
    let lit = |s: &super::State| s.decider_rows.options.iter().filter(|(_, r, _)| r.is_resting()).map(|(_, _, k)| k.clone()).collect::<Vec<_>>();
    let confirm_shown = |s: &super::State| s.decider_rows.confirm.as_ref().is_some_and(|(i, _)| !i.isHidden());
    let line = |s: &super::State| s.decider_rows.line.as_ref().filter(|(i, _)| !i.isHidden()).map(|(_, r)| r.text());

    // 1. At rest on the 4-bit.
    let (mut state, sent) = open_state(mtm, status(DeciderState::Ready, "4bit", Some("4bit")), |_| {});
    check("rest: the header names the 4-bit, grey, still", header(&state) == Some(("4-bit".into(), StatusTone::Grey)) && !spins(&state), format!("{:?}", header(&state)));
    let version = state.decider_rows.version.as_ref().map(|(_, r)| r.text());
    let shown = state.decider_rows.header.as_ref().and_then(|h| h.status()).map(|(t, _)| t);
    check("rest, open: the header shows the version alone, the lit row the precision; no version line", version.as_deref() == Some("Decider V5") && shown.as_deref() == Some("V5")
        && state.decider_rows.version.as_ref().is_some_and(|(i, _)| i.isHidden()), format!("{version:?} {shown:?}"));
    let folded = {
        let (mut st, _) = open_state(mtm, status(DeciderState::Ready, "4bit", Some("4bit")), |s| s.settings_value.collapsed = MenuSection::ALL.to_vec());
        super::decider_section::sync(&st);
        let t = st.decider_rows.header.as_ref().and_then(|h| h.status()).map(|(t, _)| t);
        st.on_action = Box::new(|_| {});
        t
    };
    check("rest, folded: the header says version and precision", folded.as_deref() == Some("V5 \u{b7} 4-bit"), format!("{folded:?}"));
    check("rest: the 4-bit rests lit, alone; no reason line, no Confirm", lit(&state) == ["4bit"] && line(&state).is_none() && !confirm_shown(&state), format!("{:?}", lit(&state)));
    let details: Vec<String> = state.decider.options.iter().map(|o| super::setup::size_text(o.memory_bytes)).collect();
    check("rest: each precision shows one instance's memory", details == ["4.2 GB", "3.1 GB", "2.4 GB"], format!("{details:?}"));
    check("rest: nothing sent", sent.borrow().is_empty(), format!("{:?}", sent.borrow()));
    shoot("section-rest-4bit", &mut state, &mut check);

    // 2. A click on 8-bit: marked, Confirm shows, nothing sent. The current one cancels; so does closing the menu.
    state.pick_decider(1);
    check("8-bit clicked: marked, Confirm glows, the 4-bit still lit", state.decider_pending.as_deref() == Some("8bit") && confirm_shown(&state)
        && lit(&state) == ["8bit", "4bit"] && sent.borrow().is_empty(), format!("{:?} {:?}", state.decider_pending, lit(&state)));
    shoot("section-pending-8bit", &mut state, &mut check);
    state.pick_decider(2);
    check("the current one clicked: the mark and Confirm go", state.decider_pending.is_none() && !confirm_shown(&state), String::new());
    state.pick_decider(1);
    state.decider_pending = None; // menuDidClose: (its with_state body; Escape closes the menu too)
    super::menu::sync_tracking(&mut state);
    check("the menu closed: nothing pending, nothing sent", !confirm_shown(&state) && sent.borrow().is_empty(), String::new());

    // 3. Confirm: one action, then the swap spins on the target and takes no clicks until the app answers.
    state.pick_decider(1);
    state.confirm_decider();
    check("Confirm sends the 8-bit, once", sent.borrow().as_slice() == [UiAction::SetDeciderPrecision("8bit".into())], format!("{:?}", sent.borrow()));
    check("swapping: the header spins on 8-bit, the rows take no clicks", header(&state) == Some(("8-bit".into(), StatusTone::Grey)) && spins(&state)
        && state.decider_rows.options.iter().all(|(i, _, _)| !i.isEnabled()) && !confirm_shown(&state), format!("{:?}", header(&state)));
    shoot("section-swapping-8bit", &mut state, &mut check);
    state.pick_decider(0);
    check("swapping: a click marks nothing", state.decider_pending.is_none(), String::new());
    state.apply(UiUpdate::Decider(status(DeciderState::Ready, "8bit", Some("8bit"))));
    check("answered: still again, the 8-bit lit", header(&state) == Some(("8-bit".into(), StatusTone::Grey)) && !spins(&state) && lit(&state) == ["8bit"], format!("{:?}", lit(&state)));

    // 4. A failed swap rolled back: what runs in grey, the reason whole in red under the version.
    let mut back = status(DeciderState::Ready, "4bit", Some("4bit"));
    back.reason = Some("8-bit didn\u{2019}t load: the decider engine cannot read this checkpoint".into());
    let (mut state, _) = open_state(mtm, back, |_| {});
    check("rolled back: the 4-bit named in grey, the reason under the version", header(&state) == Some(("4-bit".into(), StatusTone::Grey))
        && line(&state).is_some_and(|l| l.starts_with("8-bit didn\u{2019}t load")) && lit(&state) == ["4bit"], format!("{:?}", line(&state)));
    shoot("section-failed-rolled-back", &mut state, &mut check);
    let mut failed = status(DeciderState::Failed("no Metal device".into()), "4bit", None);
    failed.reason = Some("8-bit didn\u{2019}t load: no Metal device; 4-bit didn\u{2019}t load either: no Metal device".into());
    let (mut state, _) = open_state(mtm, failed, |_| {});
    check("nothing runs: Failed in red, nothing lit", header(&state) == Some(("Failed".into(), StatusTone::Alert)) && lit(&state).is_empty() && line(&state).is_some(), format!("{:?}", header(&state)));
    shoot("section-failed", &mut state, &mut check);

    // 5. The 4-bit's folder missing: the 8-bit runs in its place, the row greyed, the line says so.
    let mut fb = status(DeciderState::Ready, "4bit", Some("8bit"));
    fb.fallback = Some("8bit".into());
    fb.options[2].present = false;
    let (mut state, sent) = open_state(mtm, fb, |_| {});
    check("missing 4-bit: the 8-bit named and lit, the line says why", header(&state) == Some(("8-bit".into(), StatusTone::Grey)) && lit(&state) == ["8bit"]
        && line(&state).as_deref() == Some("4-bit is not on this Mac \u{b7} 8-bit runs") && !state.decider_rows.options[2].0.isEnabled(), format!("{:?}", line(&state)));
    state.pick_decider(2);
    check("missing 4-bit: not a pick", state.decider_pending.is_none() && sent.borrow().is_empty(), String::new());
    shoot("section-missing-4bit", &mut state, &mut check);

    // 6. Loading (Listening just turned on): a spinner on what loads.
    let (mut state, _) = open_state(mtm, status(DeciderState::Loading, "4bit", None), |_| {});
    check("loading: the 4-bit with a spinner", header(&state) == Some(("4-bit".into(), StatusTone::Grey)) && spins(&state), String::new());
    shoot("section-loading", &mut state, &mut check);

    // 7. Folded, and kept folded by a rebuild (the next launch); the whole menu with it open.
    let (mut state, sent) = open_state(mtm, status(DeciderState::Ready, "4bit", Some("4bit")), |_| {});
    state.toggle_section(MenuSection::Decider);
    let folded = state.sections.get(&MenuSection::Decider).is_some_and(|(h, items)| !h.is_on() && items.iter().all(|i| i.isHidden()));
    check("Decider folds to its header in the open menu, and the fold is sent", folded && state.menu_open
        && *sent.borrow() == [UiAction::SetSectionsCollapsed(MenuSection::ALL.to_vec())], format!("{:?}", sent.borrow()));
    shoot("section-collapsed", &mut state, &mut check);
    let (state2, _) = open_state(mtm, status(DeciderState::Ready, "4bit", Some("4bit")), |s| s.settings_value.collapsed = vec![MenuSection::Decider]);
    let kept = state2.sections.get(&MenuSection::Decider).is_some_and(|(h, items)| !h.is_on() && items.iter().all(|i| i.isHidden()));
    check("a rebuilt menu keeps Decider folded", kept, String::new());
    check("Decider's symbol exists", super::util::symbol(MenuSection::Decider.icon()).is_some(), MenuSection::Decider.icon().into());
    let path = renders.join("menu-expanded.png");
    let ok = full_menu(&path, true, mtm, |s| { s.decider = super::setup::demo_decider(); });
    check(&format!("render {}", path.display()), ok, String::new());

    // 8. Setup's Decider step at real size.
    let controller = super::Controller::new(mtm);
    let target: objc2::rc::Retained<objc2::runtime::AnyObject> = objc2::rc::Retained::into_super(objc2::rc::Retained::into_super(controller.clone()));
    let mut w = super::setup::SetupWindow::new(&target, mtm);
    {
        use objc2_app_kit::{NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameDarkAqua};
        w.window.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref());
    }
    w.set_decider(&super::setup::demo_decider());
    w.go_to(crate::model::SetupStep::Decider);
    let setup_shot = |name: &str, w: &super::setup::SetupWindow, ok: bool, check: &mut dyn FnMut(&str, bool, String)| {
        let path = renders.join(format!("{name}.png"));
        check(&format!("render {}", path.display()), super::agent_render::capture_window(&w.window, &path) && ok, String::new());
    };
    setup_shot("setup-decider-4bit", &w, super::setup::decider_checked(&w.decider, None) == "4bit", &mut check);
    w.choose_decider(1);
    setup_shot("setup-decider-8bit", &w, w.decider_choice.as_deref() == Some("8bit"), &mut check);
    let mut missing = super::setup::demo_decider();
    missing.options[0].present = false;
    w.set_decider(&missing);
    w.choose_decider(0);
    setup_shot("setup-decider-16bit-missing", &w, w.decider_choice.as_deref() == Some("8bit"), &mut check);
    super::controls::STILL_SPINNERS.with(|c| c.set(false));
    failures
}

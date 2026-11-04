//! Heimdall's Show Islands on Streams ("hide all islands"): `streams.json` in the data
//! folder holds `{"shown": true}` while the islands appear in screen shares, streams and screenshots (Discord, OBS,
//! macOS capture). Missing or false, the default: every island window is unshared (`NSWindowSharingType::None`), out
//! of every capture. Read when the first window is made, then watched: a change applies to every island window at
//! once, no restart. Lab runs that capture the islands write `{"shown": true}` into their own folder.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2_app_kit::NSWindowSharingType;

use super::app::app;
use super::filewatch::FileWatch;
use crate::model::file::Streams;
use crate::model::Paths;

thread_local! {
    static SHOWN: Cell<Option<bool>> = const { Cell::new(None) };
    static WATCH: RefCell<Option<Rc<FileWatch>>> = const { RefCell::new(None) };
}

/// Whether the islands show in captures now (the file, read once, then kept by the watch).
pub fn shown() -> bool {
    SHOWN.with(|s| {
        s.get().unwrap_or_else(|| {
            let v = Streams::shown();
            s.set(Some(v));
            v
        })
    })
}

/// The sharing type an island window takes now; new windows read it too.
pub fn sharing() -> NSWindowSharingType {
    if shown() { NSWindowSharingType::ReadOnly } else { NSWindowSharingType::None }
}

/// Watches the file from now on (the app's start).
pub fn watch() {
    let w = FileWatch::new(&Paths::streams(), Box::new(|| set(Streams::shown())));
    WATCH.with(|x| *x.borrow_mut() = Some(w));
    // A write between the first read and the watch is not missed.
    set(Streams::shown());
}

/// A new value: every island window takes it at once; the same value again touches nothing.
pub fn set(v: bool) {
    if shown() == v {
        return;
    }
    SHOWN.with(|s| s.set(Some(v)));
    let sharing = sharing();
    for c in app().all() {
        c.panel.setSharingType(sharing);
    }
}

/// `FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --streams-check`: the switch over real island windows, which the
/// app never orders onto the screen (offscreen mode): out of captures with no `streams.json`, in them once Heimdall writes
/// `{"shown": true}` (live, through the watch, for windows that exist and one made after), out again with false. The
/// capture: one island window parked 24 pt square behind the notch at 3 % opacity for about half a second, captured by
/// macOS's `screencapture -l` (an image with the switch on, none with it off), then out again (without a notch, a
/// 3 % square at the top centre for under a second). Quits by itself; the windows go with the process whatever happens.
pub fn check() {
    use crate::jobj;
    use crate::model::file::Streams;
    use super::render::{clear_folder, put};
    use super::sys;

    clear_folder();
    let _ = std::fs::remove_file(Paths::streams());
    let lines: Rc<RefCell<Vec<(bool, String)>>> = Rc::default();
    let check = {
        let lines = lines.clone();
        move |ok: bool, what: &str| {
            println!("{} {what}", if ok { "check ok  " } else { "check FAIL" });
            lines.borrow_mut().push((ok, what.to_string()));
        }
    };
    let all_are = |want: NSWindowSharingType| {
        let all = app().all();
        !all.is_empty() && all.iter().all(|c| c.panel.sharingType() == want)
    };
    put("streams-a", jobj! {"title": "Streams", "state": "done", "value": "OK"});
    sys::after(0.6, move || {
        check(app().all().len() == 1 && all_are(NSWindowSharingType::None), "no streams.json: the island window is out of captures");
        let _ = Streams::set_shown(true);
        sys::after(0.6, move || {
            check(all_are(NSWindowSharingType::ReadOnly), "shown true: the island window is shared, live");
            put("streams-b", jobj! {"title": "Second", "state": "running"});
            sys::after(0.6, move || {
                check(app().all().len() == 2 && all_are(NSWindowSharingType::ReadOnly), "an island made after the switch is shared too");
                let panel = app().ctl("streams-a").map(|c| c.panel.clone());
                let number = panel.as_ref().map(|p| p.windowNumber()).unwrap_or(0);
                let park = move |on: bool| {
                    let Some(p) = &panel else { return };
                    if on {
                        let s = objc2_app_kit::NSScreen::screens(objc2_foundation::MainThreadMarker::new().unwrap()).firstObject();
                        let f = s.map(|s| s.frame()).unwrap_or_default();
                        let r = objc2_foundation::NSRect::new(
                            objc2_foundation::NSPoint::new(f.origin.x + f.size.width / 2.0 - 12.0, f.origin.y + f.size.height - 24.0),
                            objc2_foundation::NSSize::new(24.0, 24.0),
                        );
                        p.setAlphaValue(0.03);
                        p.setFrame_display(r, true);
                        p.orderFrontRegardless();
                    } else {
                        p.orderOut(None);
                    }
                };
                let shot = move || {
                    let out = Paths::root().join("streams-check.png");
                    let _ = std::fs::remove_file(&out);
                    let ok = std::process::Command::new("/usr/sbin/screencapture")
                        .args(["-x", "-o", &format!("-l{number}")])
                        .arg(&out)
                        .stderr(std::process::Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success());
                    let _ = std::fs::remove_file(&out);
                    ok
                };
                park(true);
                sys::after(0.2, move || {
                    let on = shot();
                    let _ = Streams::set_shown(false);
                    sys::after(0.6, move || {
                        let off = shot();
                        park(false);
                        check(on, "shown true: macOS captures the island window");
                        check(!off && all_are(NSWindowSharingType::None), "shown false: every island window is out of captures again, and macOS cannot capture it");
                        clear_folder();
                        let _ = std::fs::remove_file(Paths::streams());
                        let failed = lines.borrow().iter().filter(|(ok, _)| !ok).count();
                        println!("streams-check: {failed} failed");
                        std::process::exit(if failed == 0 { 0 } else { 1 });
                    });
                });
            });
        });
    });
}

//! Setup: one window, one step at a time with Back and Continue, drawn as the approved prototype
//! (`Projects/Heimdall/lab/setup-wizard`): Welcome, the user's name, a subscription or a local model, listening (Parakeet
//! and the decider's precision), the voice (Phonon S's presets), the four permissions with Allow All, the home folder,
//! T3 Code, the coding agents set to work without asking (public edition: Set Up or Leave as Is), Vella, any steps
//! other parts of Heimdall add (`extra_steps`), and Customise me, whose Start closes Setup (the first-run greeting).
//!
//! `kit` draws the pieces (tiles, cards, buttons, dots, the app tile); `pages` lays each step; `render` writes every
//! page and state offscreen at real size.

mod kit;
mod pages;
mod render;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{sel, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameDarkAqua, NSAutoresizingMaskOptions as Mask,
    NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSSize};

use super::util::*;
use crate::identity::{Icon, Identity, IdentityAction};
use crate::model::*;
use crate::speech_voice::SpeechVoiceState;
use kit::{Dots, Pad};
use pages::*;

pub(crate) use render::{demo_decider, render};

/// `#win`: 800 × 680, resizable down to 640 × 520.
pub(crate) const W: f64 = 800.0;
pub(crate) const H: f64 = 680.0;
pub(crate) const MIN: (f64, f64) = (640.0, 520.0);
/// `#titlebar` and `#footer`.
pub(crate) const TITLEBAR: f64 = 52.0;
pub(crate) const FOOTER: f64 = 68.0;
/// What a page has between them.
pub(crate) const PAGE_H: f64 = H - TITLEBAR - FOOTER;
/// `.page`'s side padding, and the cards' `max-width`.
pub(crate) const SIDE: f64 = 48.0;
pub(crate) const CARD: f64 = 760.0;

/// A step another part of Heimdall adds after the built-in ones: its page (`PAGE_H` tall, the window's width; resized
/// with the window).
pub struct ExtraStep {
    pub view: Retained<NSView>,
}

/// The place to append steps after Vella (the relay's fallback offer goes here). Their controls target `target`.
pub(crate) fn extra_steps(_target: &AnyObject, _mtm: MainThreadMarker) -> Vec<ExtraStep> {
    vec![]
}

/// "2.4 GB", "18 MB", "512 KB": a move's size.
pub(crate) fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    if value < 10.0 { format!("{value:.1} {}", UNITS[unit]) } else { format!("{value:.0} {}", UNITS[unit]) }
}

/// A path with the user's home folder as `~`.
pub(crate) fn tilde(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// What a page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageKind {
    Builtin(SetupStep),
    Extra(usize),
}

/// What Continue does on the current page.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Advance {
    /// To the next page, after telling the app these (the home folder kept, a precision, the user's name…).
    Next(Vec<UiAction>),
    /// The last page: these, then Setup closes (and the app's first-run greeting follows `SetupDone`).
    Close(Vec<UiAction>),
    /// Nothing moves: the page shows why (a name that does not validate).
    Stay,
    /// These go to the app and the page stays until they finish (Vella's install).
    Wait(Vec<UiAction>),
}

/// The Name and Customise pages' Continue: the typed name sent when it is new and valid, nothing when it is empty or
/// unchanged, the reason when it would not validate (`personalise::commit`, the menu's own check).
pub(crate) fn leaving_name(identity: &Identity, field: super::personalise::Field, typed: &str) -> Result<Option<UiAction>, String> {
    if typed.trim().is_empty() {
        return Ok(None);
    }
    super::personalise::commit(identity, field, typed).map(|a| a.map(UiAction::Identity))
}

/// Continue on the home page with nothing chosen yet and no move waiting: the folder shown is the choice.
pub(crate) fn leaving_home(home: &HomeState) -> Option<UiAction> {
    (home.needs_choice && home.pending.is_none()).then_some(UiAction::KeepHome)
}

/// Continue on the listening page: the precision picked there, when it is not the one in settings.
pub(crate) fn leaving_decider(decider: &DeciderStatus, choice: Option<&str>) -> Option<UiAction> {
    choice.filter(|c| *c != decider.chosen && decider.option(c).is_some_and(|o| o.present)).map(|c| UiAction::SetDeciderPrecision(c.to_string()))
}

/// The precision the listening page shows chosen: the one picked there, else the one in settings, else 4-bit.
pub(crate) fn decider_checked(decider: &DeciderStatus, choice: Option<&str>) -> String {
    choice.map(str::to_string).or_else(|| (!decider.chosen.is_empty()).then(|| decider.chosen.clone())).unwrap_or_else(|| "4bit".into())
}

/// Continue on Allow with every permission already satisfied (Allow All hidden, so it can't be clicked) on a Mac where
/// setup was never requested: what Allow All sends, once, so the app records it and starts listening. With the grants
/// present no prompt appears. Nothing while one is missing (Allow All is the button), once Allow All was clicked, or
/// once setup was requested (Setup reopened later must not turn Heimdall back on).
pub(crate) fn leaving_permissions(all_granted: bool, allow_sent: bool, setup_requested: bool) -> Vec<UiAction> {
    if all_granted && !allow_sent && !setup_requested { vec![UiAction::SetupAllowed, UiAction::RequestSystemAudio] } else { vec![] }
}

/// What Continue on Vella does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VellaLeave {
    /// Move on: Not now, Vella already here, an install running, or one that failed (its reason stays on the page).
    Next,
    /// Install with Homebrew and stay, the spinner on, until it finishes (then on) or fails (shown, then Continue).
    Install,
    /// No Homebrew: Vella's page opens and Setup moves on.
    OpenPage,
}

pub(crate) fn leaving_vella(vella: &VellaSetup, add: bool) -> VellaLeave {
    if !add || vella.found.is_some() || !matches!(vella.install, T3Install::Idle) {
        VellaLeave::Next
    } else if vella.brew {
        VellaLeave::Install
    } else {
        VellaLeave::OpenPage
    }
}

/// While Continue waits on Vella's install: `None` keeps waiting; `Some(true)` it finished, move on; `Some(false)` it
/// failed, stay with the reason.
pub(crate) fn vella_wait(vella: &VellaSetup) -> Option<bool> {
    match vella.install {
        T3Install::Working(_) => None,
        T3Install::Idle => Some(true),
        T3Install::Failed(_) => Some(false),
    }
}

/// Add Vella's default: chosen when Homebrew can install it, else Not now (Add would only open a web page).
pub(crate) fn vella_add_default(vella: &VellaSetup) -> bool {
    vella.brew
}

/// Continue (titled Set Up) on Coding Agents while a harness still asks: set them all.
pub(crate) fn leaving_harnesses(harnesses: &HarnessSetup) -> Option<UiAction> {
    harnesses.pending().then_some(UiAction::SetHarnessAutonomy(true))
}

/// Whether Setup has the Coding Agents step: the public edition asks; the personal edition sets the harnesses itself
/// (docs/harness-autonomy.md).
pub(crate) const HARNESS_STEP: bool = cfg!(feature = "public");

/// A provider tile's state, from what the menu knows (`provider_status`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProviderStatus {
    /// Signed in, added, or (this Mac) in use.
    pub ready: bool,
    pub checking: bool,
    /// The provider's problem now, in its words.
    pub problem: Option<String>,
    /// This Mac: the engine's model, once it has one.
    pub model: Option<String>,
}

/// Each tile's state: the subscriptions from their sign-in, the Claude relay once added, this Mac while the engine is
/// in use. `api` is the menu's drawn copy (`model_choice::drawn`), with the relay and engine flags filled in.
pub(crate) fn provider_status(choice: ProviderChoice, api: &ApiProviders, engine: &EngineStatus) -> ProviderStatus {
    match choice.menu_action() {
        ProviderAction::Connect(id) => match api.provider(id).map(|p| &p.auth) {
            Some(ProviderAuth::Connected) => ProviderStatus { ready: true, ..Default::default() },
            Some(ProviderAuth::Problem(p)) => ProviderStatus { problem: Some(p.clone()), ..Default::default() },
            Some(ProviderAuth::Checking) => ProviderStatus { checking: true, ..Default::default() },
            _ => ProviderStatus::default(),
        },
        ProviderAction::ClaudeViaT3 => ProviderStatus { ready: api.relay_added || api.relay_on, ..Default::default() },
        ProviderAction::Engine => ProviderStatus {
            ready: super::provider_rows::engine_in_use(api, engine),
            model: (!engine.model_label.is_empty()).then(|| engine.model_label.clone()),
            ..Default::default()
        },
        ProviderAction::CustomKinds => ProviderStatus::default(),
    }
}

/// The Return key's button: Allow All while permissions are missing, Leave as Is when that was the saved choice, else
/// Continue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReturnKey {
    Allow,
    Leave,
    Next,
}

pub struct SetupWindow {
    pub window: Retained<NSWindow>,
    pages: Vec<(PageKind, Retained<NSView>)>,
    current: usize,
    welcome: WelcomePage,
    name: NamePage,
    brain: BrainPage,
    listen: ListenPage,
    voice_page: VoicePage,
    allow_page: AllowPage,
    home_page: HomePage,
    t3_page: T3Page,
    harness_page: HarnessPage,
    vella_page: VellaPage,
    custom: CustomPage,
    pub(crate) allow: Retained<Pad>,
    all_granted: bool,
    permissions: Vec<(Permission, PermissionState)>,
    pub(crate) home: HomeState,
    pub(crate) chosen: Option<ProviderChoice>,
    providers: Vec<(ProviderChoice, ProviderStatus)>,
    pub(crate) decider: DeciderStatus,
    pub(crate) decider_choice: Option<String>,
    pub(crate) voice: SpeechVoiceState,
    pub(crate) identity: Identity,
    pub(crate) harnesses: HarnessSetup,
    pub(crate) vella: VellaSetup,
    /// Add Vella or Not now as clicked; `None`: the default (`vella_add_default`). Continue acts on it.
    vella_choice: Option<bool>,
    /// Continue started Vella's install: the page waits for it (`vella_wait`).
    vella_waiting: bool,
    /// The app recorded that setup was requested (`RuntimeSettings::setup_requested`).
    setup_requested: bool,
    /// Allow All was clicked, or Continue already sent what it sends (`leaving_permissions`).
    allow_sent: bool,
    pub(crate) t3: T3Setup,
    back: Retained<Pad>,
    /// Coding Agents' Leave as Is, left of Set Up (Continue retitled) while a harness still asks.
    leave: Retained<Pad>,
    pub(crate) next: Retained<Pad>,
    dots: Retained<Dots>,
    footer: Retained<FlippedView>,
    mtm: MainThreadMarker,
}

/// The footer's buttons: 22 pt from the right edge, 10 pt apart.
const FOOTER_PAD: f64 = 22.0;
const BUTTON_GAP: f64 = 10.0;
const PRIMARY_MIN: f64 = 110.0;

impl SetupWindow {
    pub fn new(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        Self::build(target, mtm, HARNESS_STEP)
    }

    pub(crate) fn build(target: &AnyObject, mtm: MainThreadMarker, harness_step: bool) -> Self {
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, W, H),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable | NSWindowStyleMask::FullSizeContentView,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        // `#titlebar`: the traffic lights on the window's own colour, no title drawn (VoiceOver still reads it).
        window.setTitle(&ns(&format!("Set Up {}", crate::identity::app_title())));
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        window.setTitlebarAppearsTransparent(true);
        window.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref());
        window.setBackgroundColor(Some(&kit::hex(kit::BACKGROUND, 1.0)));
        window.setContentMinSize(NSSize::new(MIN.0, MIN.1));
        // An empty unified toolbar gives the 52-pt titlebar, the lights centred in it as the prototype draws them.
        let toolbar = objc2_app_kit::NSToolbar::initWithIdentifier(objc2_app_kit::NSToolbar::alloc(mtm), &ns("HeimdallSetup"));
        window.setToolbar(Some(&toolbar));
        window.setToolbarStyle(objc2_app_kit::NSWindowToolbarStyle::Unified);
        let root = FlippedView::new(rect(0.0, 0.0, W, H), mtm);
        window.setContentView(Some(&root));

        let identity = crate::identity::current();
        let welcome = WelcomePage::build(&identity, mtm);
        let name = NamePage::build(&identity, mtm);
        let brain = BrainPage::build(target, mtm);
        let listen = ListenPage::build(target, mtm);
        let voice_page = VoicePage::build(target, mtm);
        let allow_page = AllowPage::build(target, mtm);
        let home_page = HomePage::build(target, mtm);
        let t3_page = T3Page::build(target, mtm);
        let harness_page = HarnessPage::build(target, mtm);
        let vella_page = VellaPage::build(target, mtm);
        let custom = CustomPage::build(target, &identity, mtm);
        // The page keeps the delegate (it is weak).
        unsafe { name.field.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*custom.watch))) };
        let mut pages: Vec<(PageKind, Retained<NSView>)> = vec![
            (PageKind::Builtin(SetupStep::Welcome), welcome.frame.view.clone()),
            (PageKind::Builtin(SetupStep::Name), name.frame.view.clone()),
            (PageKind::Builtin(SetupStep::Providers), brain.frame.view.clone()),
            (PageKind::Builtin(SetupStep::Decider), listen.frame.view.clone()),
            (PageKind::Builtin(SetupStep::Voice), voice_page.frame.view.clone()),
            (PageKind::Builtin(SetupStep::Permissions), allow_page.frame.view.clone()),
            (PageKind::Builtin(SetupStep::Home), home_page.frame.view.clone()),
            (PageKind::Builtin(SetupStep::T3), t3_page.frame.view.clone()),
        ];
        if harness_step {
            pages.push((PageKind::Builtin(SetupStep::Harnesses), harness_page.frame.view.clone()));
        }
        pages.push((PageKind::Builtin(SetupStep::Vella), vella_page.frame.view.clone()));
        for (i, extra) in extra_steps(target, mtm).into_iter().enumerate() {
            pages.push((PageKind::Extra(i), extra.view));
        }
        pages.push((PageKind::Builtin(SetupStep::Customise), custom.frame.view.clone()));
        for (_, view) in &pages {
            view.setFrame(rect(0.0, TITLEBAR, W, PAGE_H));
            view.setAutoresizingMask(Mask::ViewWidthSizable | Mask::ViewHeightSizable);
            view.setHidden(true);
            root.addSubview(view);
        }

        // `#footer`: the dots centred, Back and the primary button at the right.
        let footer = FlippedView::new(rect(0.0, H - FOOTER, W, FOOTER), mtm);
        // Flipped root: the flexible margin is the top one (its minimum y), so the footer keeps to the bottom.
        footer.setAutoresizingMask(Mask::ViewWidthSizable | Mask::ViewMinYMargin);
        root.addSubview(&footer);
        let dots = Dots::new(rect(0.0, (FOOTER - 10.0) / 2.0, W, 10.0), pages.len(), mtm);
        dots.setAutoresizingMask(Mask::ViewWidthSizable);
        footer.addSubview(&dots);
        let next = Pad::button("Continue", true, false, PRIMARY_MIN, target, sel!(setupNext:), mtm);
        let back = Pad::button("Back", false, false, 0.0, target, sel!(setupBack:), mtm);
        let leave = Pad::button("Leave as Is", false, false, 0.0, target, sel!(setupLeaveHarnesses:), mtm);
        for b in [&next, &back, &leave] {
            b.setAutoresizingMask(Mask::ViewMinXMargin);
            footer.addSubview(b);
        }
        leave.setHidden(true);

        let allow = allow_page.allow.clone();
        let mut setup = SetupWindow {
            window,
            pages,
            current: 0,
            welcome,
            name,
            brain,
            listen,
            voice_page,
            allow_page,
            home_page,
            t3_page,
            harness_page,
            vella_page,
            custom,
            allow,
            all_granted: false,
            permissions: vec![],
            home: HomeState::default(),
            chosen: None,
            providers: vec![],
            decider: DeciderStatus::default(),
            decider_choice: None,
            voice: SpeechVoiceState::default(),
            identity,
            harnesses: HarnessSetup::default(),
            vella: VellaSetup::default(),
            vella_choice: None,
            vella_waiting: false,
            setup_requested: false,
            allow_sent: false,
            t3: T3Setup::default(),
            back,
            leave,
            next,
            dots,
            footer,
            mtm,
        };
        setup.sync_decider();
        setup.identity_views();
        setup.brain.sync(None, &[]);
        setup.go(0);
        setup.window.center();
        setup
    }

    pub(crate) fn current(&self) -> PageKind {
        self.pages[self.current].0
    }

    pub(crate) fn current_view(&self) -> &NSView {
        &self.pages[self.current].1
    }

    /// Opens at a built-in step.
    pub fn go_to(&mut self, step: SetupStep) {
        if let Some(i) = self.pages.iter().position(|(k, _)| *k == PageKind::Builtin(step)) {
            self.go(i);
        }
    }

    fn go(&mut self, index: usize) {
        let from = self.current;
        self.current = index.min(self.pages.len() - 1);
        for (i, (_, view)) in self.pages.iter().enumerate() {
            view.setHidden(i != self.current);
            view.setAlphaValue(1.0);
            let f = view.frame();
            view.setFrameOrigin(NSPoint::new(0.0, f.origin.y));
        }
        // Motion only on arrival, and only on screen: the page slides 24 pt in from the side it comes from and fades
        // in (`.page` → `.page.on`); offscreen renders get the settled frame.
        if self.window.isVisible() && from != self.current {
            arrive(&self.pages[self.current].1, self.current > from);
        }
        self.dots.set_current(self.current);
        self.back.setHidden(self.current == 0);
        if self.current() == PageKind::Builtin(SetupStep::Name) {
            self.window.makeFirstResponder(Some(&self.name.field));
        }
        self.sync_focus();
        self.sync_buttons();
    }

    /// Back: the previous page.
    pub fn back(&mut self) {
        if self.current > 0 {
            self.go(self.current - 1);
        }
    }

    /// Continue: what leaving this page tells the app, and whether Setup moves on, closes, or stays to show why not.
    pub fn advance(&mut self) -> Advance {
        let tell: Vec<UiAction> = match self.current() {
            PageKind::Builtin(SetupStep::Name) => {
                let typed = self.name.field.stringValue().to_string();
                match leaving_name(&self.identity, super::personalise::Field::UserName, &typed) {
                    Ok(action) => {
                        self.name.show_error(None);
                        action.into_iter().collect()
                    }
                    Err(reason) => {
                        self.name.show_error(Some(&reason));
                        return Advance::Stay;
                    }
                }
            }
            PageKind::Builtin(SetupStep::Customise) => {
                let typed = self.custom.field.stringValue().to_string();
                match leaving_name(&self.identity, super::personalise::Field::Name, &typed) {
                    Ok(action) => {
                        self.custom.show_error(None);
                        action.into_iter().collect()
                    }
                    Err(reason) => {
                        self.custom.show_error(Some(&reason));
                        return Advance::Stay;
                    }
                }
            }
            PageKind::Builtin(SetupStep::Permissions) => {
                let tell = leaving_permissions(self.all_granted, self.allow_sent, self.setup_requested);
                self.allow_sent |= !tell.is_empty();
                tell
            }
            PageKind::Builtin(SetupStep::Vella) => match leaving_vella(&self.vella, self.vella_add()) {
                VellaLeave::Next => vec![],
                VellaLeave::OpenPage => vec![UiAction::InstallVella],
                VellaLeave::Install => {
                    self.vella_waiting = true;
                    return Advance::Wait(vec![UiAction::InstallVella]);
                }
            },
            PageKind::Builtin(SetupStep::Home) => leaving_home(&self.home).into_iter().collect(),
            PageKind::Builtin(SetupStep::Decider) => leaving_decider(&self.decider, self.decider_choice.as_deref()).into_iter().collect(),
            PageKind::Builtin(SetupStep::Harnesses) => leaving_harnesses(&self.harnesses).into_iter().collect(),
            _ => vec![],
        };
        if self.current + 1 >= self.pages.len() {
            return Advance::Close(tell);
        }
        self.go(self.current + 1);
        Advance::Next(tell)
    }

    /// Coding Agents ▸ Leave as Is: nothing is touched, and the choice is saved.
    pub fn leave_harnesses(&mut self) -> Advance {
        if self.current() != PageKind::Builtin(SetupStep::Harnesses) {
            return Advance::Stay;
        }
        let tell = vec![UiAction::SetHarnessAutonomy(false)];
        if self.current + 1 >= self.pages.len() {
            return Advance::Close(tell);
        }
        self.go(self.current + 1);
        Advance::Next(tell)
    }

    /// The primary button's title on this page.
    pub(crate) fn primary_title(&self) -> String {
        let last = self.current + 1 >= self.pages.len();
        match self.current() {
            PageKind::Builtin(SetupStep::Welcome) => "Set Up".into(),
            PageKind::Builtin(SetupStep::Harnesses) if self.harnesses.pending() => "Set Up".into(),
            _ if last => {
                let typed = self.custom.field.stringValue().to_string();
                format!("Start {}", if typed.trim().is_empty() { self.identity.name.as_str() } else { typed.trim() })
            }
            _ => "Continue".into(),
        }
    }

    pub(crate) fn return_key(&self) -> ReturnKey {
        let choosing = self.current() == PageKind::Builtin(SetupStep::Harnesses) && self.harnesses.pending();
        if self.current() == PageKind::Builtin(SetupStep::Permissions) && !self.all_granted {
            ReturnKey::Allow
        } else if choosing && self.harnesses.chosen == Some(false) {
            // After Leave as Is (Setup reopened later), Return keeps that choice; Set Up needs its click.
            ReturnKey::Leave
        } else {
            ReturnKey::Next
        }
    }

    fn sync_buttons(&self) {
        let choosing = self.current() == PageKind::Builtin(SetupStep::Harnesses) && self.harnesses.pending();
        let width = self.footer.frame().size.width;
        self.next.retitle(&self.primary_title(), PRIMARY_MIN);
        let key = self.return_key();
        self.allow.setKeyEquivalent(&ns(if key == ReturnKey::Allow { "\r" } else { "" }));
        self.leave.setKeyEquivalent(&ns(if key == ReturnKey::Leave { "\r" } else { "" }));
        self.next.setKeyEquivalent(&ns(if key == ReturnKey::Next { "\r" } else { "" }));
        self.leave.setHidden(!choosing);
        // Right to left from the primary button: Leave as Is when shown, then Back.
        let y = ((FOOTER - 32.0) / 2.0).round();
        let mut x = width - FOOTER_PAD;
        for b in [&self.next, &self.leave, &self.back] {
            if b.isHidden() {
                continue;
            }
            let w = b.frame().size.width;
            b.setFrameOrigin(NSPoint::new(x - w, y));
            x -= w + BUTTON_GAP;
        }
        self.dots.set_reserve(width - x - BUTTON_GAP);
    }

    pub fn show(&mut self, permissions: &[(Permission, PermissionState)]) {
        self.permissions = permissions.to_vec();
        self.all_granted = permissions.iter().all(|(_, s)| s.satisfied());
        self.allow_page.sync(permissions, self.all_granted, super::permissions::request_in_flight());
        self.sync_buttons();
    }

    /// The assistant's name and mark, and the user's name: every page that shows them.
    pub fn set_identity(&mut self, identity: &Identity) {
        let user_changed = identity.user_name != self.identity.user_name;
        let name_changed = identity.name != self.identity.name;
        self.identity = identity.clone();
        self.identity_views();
        // A field the user is not typing in follows the saved value.
        if user_changed && !self.is_editing(&self.name.field) {
            self.name.field.setStringValue(&ns(&user_name_shown(identity)));
        }
        if name_changed && !self.is_editing(&self.custom.field) {
            self.custom.field.setStringValue(&ns(&identity.name));
        }
        self.sync_buttons();
    }

    /// Every view that shows the identity: the window's title, Welcome, Allow, Customise, Vella's diagram.
    fn identity_views(&mut self) {
        let identity = self.identity.clone();
        self.window.setTitle(&ns(&format!("Set Up {}", crate::identity::title_for(&identity.name))));
        self.welcome.sync(&identity);
        self.allow_page.title.setStringValue(&ns(&SetupStep::Permissions.title(&identity.name)));
        self.custom.sync(&identity, self.mtm);
        self.vella_page.diagram_heimdall.set_icon(&identity.icon);
    }

    fn is_editing(&self, field: &objc2_app_kit::NSTextField) -> bool {
        field.currentEditor().is_some()
    }

    /// Customise ▸ the name being typed: the wake word and the greeting follow it.
    pub(crate) fn name_typed(&mut self) {
        let typed = self.custom.field.stringValue().to_string();
        self.custom.preview_name(&self.identity, &typed);
        self.custom.show_error(None);
        self.sync_buttons();
    }

    /// Each name field's ring follows whether it is being edited (first responder).
    pub(crate) fn sync_focus(&mut self) {
        kit::focus_well(&self.name.well, self.is_editing(&self.name.field));
        kit::focus_well(&self.custom.well, self.is_editing(&self.custom.field));
    }

    /// Customise ▸ a mark: what the menu's mark click sends.
    pub(crate) fn mark_action(&self, index: usize) -> Option<UiAction> {
        let (icon, _) = self.custom.marks.get(index)?;
        (*icon != self.identity.icon).then(|| UiAction::Identity(IdentityAction::SetIcon(icon.clone())))
    }

    pub(crate) fn marks(&self) -> Vec<Icon> {
        self.custom.marks.iter().map(|(i, _)| i.clone()).collect()
    }

    pub fn set_home(&mut self, home: &HomeState) {
        self.home = home.clone();
        self.home_page.sync(home);
    }

    /// A provider tile clicked: it shows as chosen (nothing is chosen until then), its card under the tiles.
    pub fn choose_provider(&mut self, choice: ProviderChoice) {
        self.chosen = Some(choice);
        self.brain.sync(self.chosen, &self.providers);
    }

    /// What each provider tile shows: signed in, added, in use.
    pub fn set_providers(&mut self, statuses: Vec<(ProviderChoice, ProviderStatus)>) {
        if statuses == self.providers {
            return;
        }
        self.providers = statuses;
        self.brain.sync(self.chosen, &self.providers);
    }

    /// The decider's precisions from the app: each one's figures, the missing ones greyed, the chosen one.
    pub fn set_decider(&mut self, decider: &DeciderStatus) {
        // The page's pick is applied (now the setting) or was rolled back (a reason): the app's word holds from here.
        if self.decider_choice.as_deref() == Some(decider.chosen.as_str()) || (decider.reason.is_some() && decider.reason != self.decider.reason) {
            self.decider_choice = None;
        }
        self.decider = decider.clone();
        self.sync_decider();
    }

    /// A precision picked (`index` in `DeciderStatus::options`): it shows as the choice; Continue applies it.
    pub fn choose_decider(&mut self, index: usize) {
        let Some(o) = self.decider.options.get(index).filter(|o| o.present) else {
            self.sync_decider();
            return;
        };
        self.decider_choice = Some(o.key.clone());
        self.sync_decider();
    }

    /// The segmented control's segment `segment` as an index in `DeciderStatus::options`.
    pub(crate) fn decider_option_at(&self, segment: isize) -> Option<usize> {
        let segments = decider_segments(&self.decider);
        let key = &segments.get(usize::try_from(segment).ok()?)?.key;
        self.decider.options.iter().position(|o| &o.key == key)
    }

    fn sync_decider(&self) {
        self.listen.sync(&self.decider, &decider_checked(&self.decider, self.decider_choice.as_deref()));
    }

    /// Speaking's state from the app: the presets in the grid, the chosen one.
    pub fn set_voice(&mut self, voice: &SpeechVoiceState) {
        self.voice = voice.clone();
        self.voice_page.sync(voice, self.mtm);
        self.custom.set_voice(voice_name(voice).as_deref());
    }

    /// The voice tile at `index`: what choosing it sends (Phonon S with that preset), unless it is already the voice.
    pub(crate) fn voice_action(&self, index: usize) -> Option<crate::speech_voice::SpeechVoiceAction> {
        let preset = voice_list(&self.voice).get(index)?;
        (voice_chosen(&self.voice) != Some(preset.name.as_str())).then(|| crate::speech_voice::SpeechVoiceAction::SetPreset(preset.name.clone()))
    }

    pub fn set_t3(&mut self, t3: &T3Setup) {
        self.t3 = t3.clone();
        self.t3_page.sync(t3);
    }

    pub fn set_harnesses(&mut self, harnesses: &HarnessSetup) {
        self.harnesses = harnesses.clone();
        self.harness_page.sync(harnesses);
        self.sync_buttons();
    }

    pub fn set_vella(&mut self, vella: &VellaSetup) {
        self.vella = vella.clone();
        self.vella_page.sync(vella, self.vella_add(), &self.identity);
        if !self.vella_waiting {
            return;
        }
        match vella_wait(vella) {
            None => {}
            Some(done) => {
                self.vella_waiting = false;
                if done && self.current() == PageKind::Builtin(SetupStep::Vella) && self.current + 1 < self.pages.len() {
                    self.go(self.current + 1);
                }
            }
        }
    }

    fn vella_add(&self) -> bool {
        self.vella_choice.unwrap_or_else(|| vella_add_default(&self.vella))
    }

    /// Vella's tiles: Add (true) or Not now. Only the choice; Continue acts on it (`leaving_vella`).
    pub(crate) fn choose_vella(&mut self, add: bool) {
        self.vella_choice = Some(add);
        self.vella_page.sync(&self.vella, add, &self.identity);
    }

    /// The app's word on whether setup was requested (`RuntimeSettings`).
    pub(crate) fn set_setup_requested(&mut self, requested: bool) {
        self.setup_requested = requested;
    }

    /// Allow All was clicked: Continue sends nothing more.
    pub(crate) fn allow_clicked(&mut self) {
        self.allow_sent = true;
    }

    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    pub fn close(&self) {
        self.window.orderOut(None);
    }
}

/// The arrival: from 24 pt to the side and transparent, to in place and opaque, on the prototype's curve.
fn arrive(view: &NSView, forward: bool) {
    let f = view.frame();
    let dx = if forward { 24.0 } else { -24.0 };
    view.setAlphaValue(0.0);
    view.setFrameOrigin(NSPoint::new(f.origin.x + dx, f.origin.y));
    let target = unsafe { Retained::retain(view as *const NSView as *mut NSView) };
    let Some(target) = target else { return };
    let block = block2::RcBlock::new(move |context: std::ptr::NonNull<objc2_app_kit::NSAnimationContext>| {
        let context = unsafe { context.as_ref() };
        context.setDuration(0.34);
        let curve = objc2_quartz_core::CAMediaTimingFunction::functionWithControlPoints(0.3, 1.0, 0.4, 1.0);
        let _: () = unsafe { objc2::msg_send![context, setTimingFunction: &*curve] };
        use objc2_app_kit::NSAnimatablePropertyContainer as _;
        let animator = target.animator();
        animator.setAlphaValue(1.0);
        animator.setFrameOrigin(f.origin);
    });
    objc2_app_kit::NSAnimationContext::runAnimationGroup(&block);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wizard_decider_defaults_to_4_bit_and_continue_sends_only_a_change() {
        let d = render::demo_decider();
        assert_eq!(decider_checked(&d, None), "4bit");
        assert_eq!(decider_checked(&DeciderStatus::default(), None), "4bit", "before the app says: 4-bit");
        assert_eq!(leaving_decider(&d, None), None);
        assert_eq!(leaving_decider(&d, Some("4bit")), None, "the one in settings: nothing to send");
        assert_eq!(leaving_decider(&d, Some("16bit")), Some(UiAction::SetDeciderPrecision("16bit".into())));
        let mut missing = d.clone();
        missing.options[0].present = false;
        assert_eq!(leaving_decider(&missing, Some("16bit")), None);
        // The segmented control runs smallest first, whatever order the app lists them in.
        let keys: Vec<&str> = decider_segments(&d).iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["4bit", "8bit", "16bit"]);
    }

    #[test]
    fn wizard_coding_agents_set_up_only_while_one_still_asks() {
        let row = |name: &str, ready| HarnessRow { name: name.into(), ready, guarded: false };
        let asks = HarnessSetup { rows: vec![row("Claude Code", true), row("Codex", false)], ..Default::default() };
        assert_eq!(leaving_harnesses(&asks), Some(UiAction::SetHarnessAutonomy(true)));
        let set = HarnessSetup { rows: vec![row("Claude Code", true)], ..Default::default() };
        assert_eq!(leaving_harnesses(&set), None, "nothing to set: Continue sends nothing");
        assert_eq!(leaving_harnesses(&HarnessSetup::default()), None);
        assert_eq!(HARNESS_STEP, cfg!(feature = "public"), "the personal edition sets them without a step");
        assert_eq!(DENY_LINES, 4);
    }

    #[test]
    fn wizard_move_sizes_read_in_the_users_units() {
        assert_eq!(size_text(512), "512 bytes");
        assert_eq!(size_text(18_400_000), "18 MB");
        assert_eq!(size_text(2_431_000_000), "2.4 GB");
        assert_eq!(size_text(1_500_000_000_000), "1.5 TB");
    }

    #[test]
    fn wizard_continue_from_home_keeps_the_folder_only_when_nothing_is_chosen() {
        let fresh = HomeState { home: "/Users/a".into(), data: "/Users/a/.heimdall".into(), needs_choice: true, pending: None, error: None };
        assert_eq!(leaving_home(&fresh), Some(UiAction::KeepHome));
        // A move waiting for its confirm is not confirmed by Continue, and nothing else is recorded either.
        let moving = HomeState { pending: Some(HomeMove { to_home: "/Users/a/Documents".into(), to: "/Users/a/Documents/.heimdall".into(), bytes: 1 }), ..fresh.clone() };
        assert_eq!(leaving_home(&moving), None);
        // Already chosen: nothing to record.
        assert_eq!(leaving_home(&HomeState { needs_choice: false, ..fresh }), None);
    }

    #[test]
    fn wizard_providers_are_the_local_engine_and_subscriptions_with_nothing_chosen() {
        let titles: Vec<_> = ProviderChoice::ALL.iter().map(|c| c.title()).collect();
        assert_eq!(titles, ["Local Splash Engine", "OpenCode Go subscription", "Claude Code subscription", "Codex subscription"]);
        // No tile asks for an API key (readiness audit, 3 Nov).
        assert!(titles.iter().all(|t| !t.contains("API")));
        assert!(ProviderChoice::ALL.iter().all(|c| !matches!(c.menu_action(), ProviderAction::CustomKinds | ProviderAction::Connect("anthropic"))));
        // The tiles: every choice once, the subscriptions first, then this Mac; and no row of a card is an API key.
        assert_eq!(BRAIN_ORDER.map(tile_title), ["Codex", "Claude Code", "OpenCode Go", "This Mac"]);
        for c in ProviderChoice::ALL {
            assert!(BRAIN_ORDER.contains(&c));
            assert!(brain_rows(c, &ProviderStatus::default()).iter().all(|(label, _, _)| !label.contains("key") && !label.contains("Key")));
        }
    }

    #[test]
    fn wizard_provider_cards_say_what_is_done_and_offer_only_what_is_not() {
        let missing = brain_rows(ProviderChoice::ChatGpt, &ProviderStatus::default());
        assert_eq!(missing, [("Account", "Not signed in".to_string(), Some("Sign In…"))]);
        let signed = brain_rows(ProviderChoice::OpenCodeGo, &ProviderStatus { ready: true, ..Default::default() });
        assert_eq!(signed, [("Account", "Signed in".to_string(), None)]);
        let problem = brain_rows(ProviderChoice::ChatGpt, &ProviderStatus { problem: Some("Usage limit reached".into()), ..Default::default() });
        assert_eq!(problem[0].2, Some("Sign In Again…"));
        let local = brain_rows(ProviderChoice::SplashEngine, &ProviderStatus { ready: true, model: Some("Qwen3.8 27B".into()), ..Default::default() });
        assert_eq!(local.len(), 2);
        assert_eq!(local[0].2, None);
        assert_eq!(brain_rows(ProviderChoice::ClaudeViaT3, &ProviderStatus::default())[0].2, Some("Add"));
    }

    #[test]
    fn wizard_provider_status_reads_sign_in_relay_and_engine() {
        let row = |id: &str, auth: ProviderAuth| ApiProviderRow {
            id: id.into(), name: id.into(), icon: String::new(), console_url: String::new(), auth, notice: None, sign_in: true, credit: None, spend: None,
            custom_url: false, key_required: false, endpoint: None, group: ProviderGroup::default(), key_fallback: false,
        };
        let api = ApiProviders { providers: vec![row("openai", ProviderAuth::Connected), row("opencode-go", ProviderAuth::Missing)], unchosen: true, relay_added: true, ..Default::default() };
        let engine = EngineStatus::default();
        assert!(provider_status(ProviderChoice::ChatGpt, &api, &engine).ready);
        assert!(!provider_status(ProviderChoice::OpenCodeGo, &api, &engine).ready);
        assert!(provider_status(ProviderChoice::ClaudeViaT3, &api, &engine).ready);
        assert!(!provider_status(ProviderChoice::SplashEngine, &api, &engine).ready, "the engine off and not chosen");
        let running = EngineStatus { state: EngineState::Ready, model_label: "Qwen3.8 27B".into(), ..Default::default() };
        let s = provider_status(ProviderChoice::SplashEngine, &api, &running);
        assert!(s.ready && s.model.as_deref() == Some("Qwen3.8 27B"));
    }

    #[test]
    fn wizard_names_are_checked_as_the_menu_checks_them() {
        use super::super::personalise::Field;
        let identity = Identity { name: "Heimdall".into(), user_name: crate::identity::PUBLIC_USER.into(), icon: Icon::Helmet };
        // Empty: nothing sent, the page moves on.
        assert_eq!(leaving_name(&identity, Field::UserName, "  "), Ok(None));
        assert_eq!(leaving_name(&identity, Field::UserName, "Ada"), Ok(Some(UiAction::Identity(IdentityAction::SetUserName("Ada".into())))));
        // Unchanged: nothing sent.
        assert_eq!(leaving_name(&identity, Field::Name, "Heimdall"), Ok(None));
        assert!(leaving_name(&identity, Field::Name, "Hey").is_err(), "a greeting can't be the name");
        // The public placeholder user is never shown as if the user had typed it.
        assert_eq!(user_name_shown(&identity), "");
        assert_eq!(user_name_shown(&Identity { user_name: "Ada".into(), ..identity.clone() }), "Ada");
        assert_eq!(greeting("User", "Ivy"), "Hi. I’m Ivy.");
        assert_eq!(greeting("Ada", "Heimdall"), "Hi Ada. I’m Heimdall.");
    }

    #[test]
    fn wizard_steps_run_in_the_prototypes_order() {
        assert_eq!(SetupStep::ALL.first(), Some(&SetupStep::Welcome));
        assert_eq!(SetupStep::ALL.last(), Some(&SetupStep::Customise));
        let at = |s: SetupStep| SetupStep::ALL.iter().position(|x| *x == s).unwrap();
        assert!(at(SetupStep::Name) < at(SetupStep::Providers) && at(SetupStep::Providers) < at(SetupStep::Decider));
        assert!(at(SetupStep::Decider) < at(SetupStep::Voice) && at(SetupStep::Voice) < at(SetupStep::Permissions));
        assert!(at(SetupStep::Permissions) < at(SetupStep::T3) && at(SetupStep::T3) < at(SetupStep::Vella));
        assert_eq!(SetupStep::Welcome.title("Ivy"), "Welcome to Ivy");
        assert_eq!(SetupStep::Permissions.title("Ivy"), "Allow Ivy");
    }

    #[test]
    fn wizard_t3_rows_follow_what_was_found() {
        let found = |v2: bool| T3App { path: "/Applications/T3 Code.app".into(), name: "T3 Code".into(), version: "0.1.2".into(), supports_v2: v2 };
        let missing = t3_rows(&T3Setup::default());
        assert!(missing.install.as_ref().is_some_and(|(_, _, b)| *b == Some("Install")) && missing.connect == ConnectRow::Pending);
        let old = t3_rows(&T3Setup { found: Some(found(false)), ..Default::default() });
        assert_eq!(old.found_warning, Some("Needs 0.1.0 or later"));
        assert!(old.install.is_some(), "too old: install the new one");
        let ready = t3_rows(&T3Setup { found: Some(found(true)), ..Default::default() });
        assert!(ready.install.is_none() && ready.connect == ConnectRow::Ask);
        let connected = t3_rows(&T3Setup { found: Some(found(true)), connect_allowed: true, ..Default::default() });
        assert_eq!(connected.connect, ConnectRow::Done);
        let failed = t3_rows(&T3Setup { install: T3Install::Failed("checksum".into()), ..Default::default() });
        assert_eq!(failed.install.unwrap().2, Some("Download Page"));
    }

    #[test]
    fn wizard_vella_offers_not_now_only_while_it_is_missing() {
        assert_eq!(vella_tiles(&VellaSetup { brew: true, ..Default::default() }), ("Add Vella", true));
        assert_eq!(vella_tiles(&VellaSetup::default()), ("Get Vella…", true), "no Homebrew: Vella's page");
        let found = VellaSetup { found: Some(VellaApp { path: "/Applications/Vella.app".into(), version: "2.1.2".into() }), ..Default::default() };
        assert_eq!(vella_tiles(&found), ("Use it", false));
    }

    #[test]
    fn wizard_voice_grid_marks_the_preset_only_while_phonon_speaks() {
        use crate::speech_voice::{demo_speech_voice, LocalVoice};
        let phonon = demo_speech_voice(false, LocalVoice::Phonon, false);
        assert_eq!(voice_chosen(&phonon), Some("Edwin"));
        assert_eq!(voice_chosen(&demo_speech_voice(false, LocalVoice::Drama3, false)), None, "Drama 3 speaks: no preset is the voice");
        assert_eq!(voice_chosen(&demo_speech_voice(true, LocalVoice::Phonon, true)), None, "ElevenLabs speaks");
    }

    #[test]
    fn wizard_continue_on_an_all_granted_allow_page_sends_allow_all_once() {
        let allow = vec![UiAction::SetupAllowed, UiAction::RequestSystemAudio];
        assert_eq!(leaving_permissions(true, false, false), allow, "Allow All is hidden: Continue records setup");
        assert_eq!(leaving_permissions(true, true, false), vec![], "once: Allow All clicked, or Continue already sent it");
        assert_eq!(leaving_permissions(false, false, false), vec![], "one missing: Allow All is the button");
        assert_eq!(leaving_permissions(true, false, true), vec![], "setup was requested: reopened Setup never turns Heimdall on");
    }

    #[test]
    fn wizard_vella_installs_only_on_continue_with_add_chosen() {
        let missing = VellaSetup { found: None, brew: true, install: T3Install::Idle };
        assert_eq!(leaving_vella(&missing, true), VellaLeave::Install);
        assert_eq!(leaving_vella(&missing, false), VellaLeave::Next, "Not now does nothing");
        let working = VellaSetup { install: T3Install::Working("Installing Vella with Homebrew…".into()), ..missing.clone() };
        assert_eq!(leaving_vella(&working, true), VellaLeave::Next, "one install at a time");
        let failed = VellaSetup { install: T3Install::Failed("No cask".into()), ..missing.clone() };
        assert_eq!(leaving_vella(&failed, true), VellaLeave::Next, "after a failure, Continue moves on");
        let found = VellaSetup { found: Some(VellaApp { path: "/Applications/Vella.app".into(), version: "2.1.2".into() }), ..missing.clone() };
        assert_eq!(leaving_vella(&found, true), VellaLeave::Next, "already here");
        // Waiting on the install: on while it works, move on when done, stay with the reason when it fails.
        assert_eq!(vella_wait(&working), None);
        assert_eq!(vella_wait(&found), Some(true));
        assert_eq!(vella_wait(&failed), Some(false));
        // Without Homebrew Add would only open a web page: Not now by default, and Get Vella… opens it.
        let no_brew = VellaSetup { brew: false, ..missing.clone() };
        assert!(vella_add_default(&missing) && !vella_add_default(&no_brew));
        assert_eq!(leaving_vella(&no_brew, true), VellaLeave::OpenPage);
        assert_eq!(vella_tiles(&no_brew).0, "Get Vella…");
    }

    #[test]
    fn wizard_voice_grid_offers_the_everyday_voices_and_customise_names_the_voice() {
        use crate::speech_voice::{demo_speech_voice, LocalVoice};
        let v = demo_speech_voice(false, LocalVoice::Phonon, false);
        assert_eq!(voice_list(&v).len(), 25);
        assert_eq!(voice_list(&v)[0].name, "Edwin");
        assert!(voice_list(&v).iter().all(|p| p.group != "Characters"));
        let old = SpeechVoiceState { everyday: vec![], ..v.clone() };
        assert_eq!(voice_list(&old).len(), 4, "an app without the list: the menu's presets");
        assert_eq!(voice_name(&v).as_deref(), Some("Edwin"));
        assert_eq!(voice_name(&demo_speech_voice(true, LocalVoice::Phonon, true)).as_deref(), Some("George"));
        assert_eq!(voice_name(&demo_speech_voice(false, LocalVoice::Drama3, false)).as_deref(), Some("Drama 3"));
    }

    #[test]
    fn wizard_permission_rows_name_every_state() {
        assert_eq!(permission_text(PermissionState::Granted), ("Allowed", true));
        assert_eq!(permission_text(PermissionState::Denied).0, "Denied");
        assert_eq!(permission_text(PermissionState::NotDetermined).0, "Not asked");
        assert_eq!(permission_text(PermissionState::Unconfirmed).0, "Ready");
    }
}

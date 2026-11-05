//! Setup: one window, one step at a time with Back and Continue, drawn as the approved prototype
//! (`Projects/Heimdall/lab/setup-wizard`): Welcome, the user's name, a subscription or a local model, listening (Parakeet
//! and the decider's precision), the voice (Phonon S's presets), the four permissions with Allow All, the home folder,
//! T3 Code, the coding agents set to work without asking (public edition: Set Up or Leave as Is), Vella, any steps
//! other parts of Heimdall add (`extra_steps`), and Customise me, whose Start closes Setup (the first-run greeting).
//!
//! `kit` draws the pieces (tiles, cards, buttons, dots, the app tile); `pages` lays each step; `render` writes every
//! page and state offscreen at real size.

mod arsenal;
mod downloads;
mod microphone;
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

/// Start on Getting ready: every model downloaded and checked, and every readiness row green. Nothing reported yet
/// (the app has not sent its rows) is not ready.
pub(crate) fn start_allowed(downloads: &ModelDownloads, readiness: &SetupReadiness) -> bool {
    downloads.finished() && !readiness.rows.is_empty() && readiness.ready()
}

/// Getting ready's row `index`, as the list draws it (`downloads::lines`): what its button does.
pub(crate) fn fix_at(downloads: &ModelDownloads, readiness: &SetupReadiness, index: usize) -> Option<ReadyFix> {
    downloads::lines(downloads, readiness).into_iter().nth(index).and_then(|(_, fix)| fix)
}

pub(crate) use crate::identity::{persona, PERSONAS};

/// What picking a mark on Customise sends, through the menu's own actions: the mark; its name, unless the name field
/// holds one the user typed (anything but empty or another mark's name is kept); its voice, unless the user picked a
/// voice on the Voice step (`voice_picked`), or Phonon S is not the voice, or its voice is not a mark's (chosen in the
/// menu: kept).
pub(crate) fn mark_choice(identity: &Identity, typed: &str, icon: &Icon, voice: &SpeechVoiceState, voice_picked: bool) -> Vec<UiAction> {
    let mut actions = vec![];
    if *icon != identity.icon {
        actions.push(UiAction::Identity(IdentityAction::SetIcon(icon.clone())));
    }
    let Some((name, preset)) = persona(icon) else { return actions };
    let typed = typed.trim();
    let own_name = !typed.is_empty() && !PERSONAS.iter().any(|(_, n, _)| *n == typed);
    if !own_name && identity.name != name {
        actions.push(UiAction::Identity(IdentityAction::SetName(name.into())));
    }
    let current = voice_chosen(voice);
    let follows = !voice_picked && current.is_some_and(|c| c.is_empty() || PERSONAS.iter().any(|(_, _, v)| *v == c));
    if follows && current != Some(preset) && voice_list(voice).iter().any(|p| p.name == preset) {
        actions.push(UiAction::SpeechVoice(crate::speech_voice::SpeechVoiceAction::SetPreset(preset.into())));
    }
    actions
}

/// Continue on the home page with nothing chosen yet and no move waiting: the folder shown is the choice.
pub(crate) fn leaving_home(home: &HomeState) -> Option<UiAction> {
    (home.needs_choice && home.pending.is_none()).then_some(UiAction::KeepHome)
}

/// Continue on the listening page: the precision picked there, when it is not the one in settings.
pub(crate) fn leaving_decider(decider: &DeciderStatus, choice: Option<&str>) -> Option<UiAction> {
    choice.filter(|c| *c != decider.chosen && decider.option(c).is_some_and(|o| o.present || decider.downloadable)).map(|c| UiAction::SetDeciderPrecision(c.to_string()))
}

/// Continue on the listening page: the precision (when changed), then the downloads it needs (public edition; the app
/// ignores it when every model is here). The order matters: the downloads read the precision in settings.
pub(crate) fn leaving_listening(decider: &DeciderStatus, choice: Option<&str>) -> Vec<UiAction> {
    leaving_decider(decider, choice).into_iter().chain([UiAction::StartModelDownloads]).collect()
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

/// What Continue on Arsenal does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArsenalLeave {
    /// Move on after telling the app these (Use it, or Not now while it was on).
    Next(Vec<UiAction>),
    /// Install with Homebrew and stay, the spinner on, until it is here (then Heimdall uses it) or fails.
    Install,
    /// No Homebrew: Arsenal's page opens and Setup moves on.
    OpenPage,
}

/// Continue on Arsenal with `add` (Add Arsenal or Use it) or Not now. Nothing installs any other way.
pub(crate) fn leaving_arsenal(a: &ArsenalSetup, add: bool) -> ArsenalLeave {
    match (add, &a.found, &a.install) {
        (false, _, _) => ArsenalLeave::Next(if a.on { vec![UiAction::SetArsenal(false)] } else { vec![] }),
        (true, Some(_), _) => ArsenalLeave::Next(if a.on { vec![] } else { vec![UiAction::SetArsenal(true)] }),
        (true, None, T3Install::Working(_) | T3Install::Failed(_)) => ArsenalLeave::Next(vec![]),
        (true, None, T3Install::Idle) if a.brew => ArsenalLeave::Install,
        (true, None, T3Install::Idle) => ArsenalLeave::OpenPage,
    }
}

/// Arsenal's default: chosen when it is here or Homebrew can install it; Not now when Add would only open a page.
pub(crate) fn arsenal_add_default(a: &ArsenalSetup) -> bool {
    a.on || a.found.is_some() || a.brew
}

/// Continue (titled Set Up) on Coding Agents while a harness still asks: set them all.
pub(crate) fn leaving_harnesses(harnesses: &HarnessSetup) -> Option<UiAction> {
    harnesses.pending().then_some(UiAction::SetHarnessAutonomy(true))
}

/// Whether Setup has the Coding Agents step: the public edition asks; the personal edition sets the harnesses itself
/// (decided 2 Nov; docs/harness-autonomy.md).
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
        ProviderAction::CustomKinds => {
            let auth = super::provider_rows::CUSTOM_KINDS.iter().filter_map(|(id, _)| api.provider(id).map(|p| &p.auth)).collect::<Vec<_>>();
            ProviderStatus {
                ready: auth.iter().any(|a| **a == ProviderAuth::Connected),
                problem: auth.iter().find_map(|a| match a { ProviderAuth::Problem(p) => Some(p.clone()), _ => None }),
                ..Default::default()
            }
        }
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
    mic_page: microphone::MicPage,
    home_page: HomePage,
    t3_page: T3Page,
    harness_page: HarnessPage,
    vella_page: VellaPage,
    arsenal_page: arsenal::ArsenalPage,
    downloads_page: downloads::DownloadsPage,
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
    pub(crate) downloads: ModelDownloads,
    pub(crate) readiness: SetupReadiness,
    pub(crate) microphones: Microphones,
    /// The Microphone step's level meter, while that step is on screen and the microphone is allowed.
    meter: Option<microphone::Meter>,
    mic_allowed: bool,
    pub(crate) arsenal: ArsenalSetup,
    /// Add Arsenal (Use it) or Not now as clicked; `None`: the default (`arsenal_add_default`).
    arsenal_choice: Option<bool>,
    /// Continue started Arsenal's install: the page waits for it.
    arsenal_waiting: bool,
    /// Add Vella or Not now as clicked; `None`: the default (`vella_add_default`). Continue acts on it.
    vella_choice: Option<bool>,
    /// Continue started Vella's install: the page waits for it (`vella_wait`).
    vella_waiting: bool,
    /// The app recorded that setup was requested (`RuntimeSettings::setup_requested`).
    setup_requested: bool,
    /// Allow All was clicked, or Continue already sent what it sends (`leaving_permissions`).
    allow_sent: bool,
    /// When Allow All was clicked (`refocus_after`).
    allowed_at: Option<std::time::Instant>,
    /// The app active before the latest activation (`app_activated`).
    last_front: Option<String>,
    /// The watch for macOS's prompts while Setup shows (`prompt_tick`), and whether one was on screen at its last tick.
    prompt_watch: Option<Retained<objc2_foundation::NSTimer>>,
    prompt_up: bool,
    /// A voice was clicked on the Voice step: a mark's voice no longer replaces it (`mark_choice`).
    voice_picked: bool,
    /// Welcome offers Let my agent set it up (the public edition's Setup; its command is the public app's).
    agent_offered: bool,
    pub(crate) t3: T3Setup,
    back: Retained<Pad>,
    /// Coding Agents' Leave as Is, left of Set Up (Continue retitled) while a harness still asks.
    leave: Retained<Pad>,
    /// Welcome's Let my agent set it up, left of Set Up: the one line to paste into a coding agent (docs/setup.md).
    pub(crate) agent: Retained<Pad>,
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

    /// `harness_step`: the public edition's own steps (Coding Agents, Getting ready).
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
        // Closing before Setup is finished asks to quit (the Controller's `windowShouldClose:`).
        if let Some(delegate) = target.downcast_ref::<super::Controller>() {
            window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(delegate)));
        }

        let identity = crate::identity::current();
        let welcome = WelcomePage::build(&identity, mtm);
        let name = NamePage::build(&identity, mtm);
        let brain = BrainPage::build(target, mtm);
        let listen = ListenPage::build(target, mtm);
        let voice_page = VoicePage::build(target, mtm);
        let allow_page = AllowPage::build(target, mtm);
        let mic_page = microphone::MicPage::build(target, mtm);
        let home_page = HomePage::build(target, mtm);
        let t3_page = T3Page::build(target, mtm);
        let harness_page = HarnessPage::build(target, mtm);
        let vella_page = VellaPage::build(target, mtm);
        let downloads_page = downloads::DownloadsPage::build(target, mtm);
        let arsenal_page = arsenal::ArsenalPage::build(target, mtm);
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
            (PageKind::Builtin(SetupStep::Microphone), mic_page.frame.view.clone()),
            (PageKind::Builtin(SetupStep::Home), home_page.frame.view.clone()),
            (PageKind::Builtin(SetupStep::T3), t3_page.frame.view.clone()),
        ];
        if harness_step {
            pages.push((PageKind::Builtin(SetupStep::Harnesses), harness_page.frame.view.clone()));
        }
        pages.push((PageKind::Builtin(SetupStep::Vella), vella_page.frame.view.clone()));
        if harness_step {
            pages.push((PageKind::Builtin(SetupStep::Arsenal), arsenal_page.frame.view.clone()));
        }
        for (i, extra) in extra_steps(target, mtm).into_iter().enumerate() {
            pages.push((PageKind::Extra(i), extra.view));
        }
        pages.push((PageKind::Builtin(SetupStep::Customise), custom.frame.view.clone()));
        // Getting ready is the last step: Start waits there until every row is green.
        if harness_step {
            pages.push((PageKind::Builtin(SetupStep::Models), downloads_page.frame.view.clone()));
        }
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
        let agent = Pad::button(AGENT_BUTTON, false, false, 0.0, target, sel!(setupLetAgent:), mtm);
        for b in [&next, &back, &leave, &agent] {
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
            mic_page,
            home_page,
            t3_page,
            harness_page,
            vella_page,
            arsenal_page,
            downloads_page,
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
            downloads: ModelDownloads::default(),
            readiness: SetupReadiness::default(),
            microphones: Microphones::default(),
            meter: None,
            mic_allowed: false,
            arsenal: ArsenalSetup::default(),
            arsenal_choice: None,
            arsenal_waiting: false,
            vella_choice: None,
            vella_waiting: false,
            setup_requested: false,
            allow_sent: false,
            allowed_at: None,
            last_front: None,
            prompt_watch: None,
            prompt_up: false,
            voice_picked: false,
            agent_offered: harness_step,
            t3: T3Setup::default(),
            back,
            leave,
            agent,
            next,
            dots,
            footer,
            mtm,
        };
        setup.sync_decider();
        setup.identity_views();
        setup.brain.sync(None, &[]);
        setup.go(0);
        setup.place(true);
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
        self.sync_meter();
    }

    /// The Microphone step's inputs from the app (the menu's list): the card, the pop-up, the meter's input.
    pub fn set_microphones(&mut self, microphones: &Microphones) {
        self.microphones = microphones.clone();
        self.mic_page.sync(microphones);
        self.sync_meter();
    }

    /// The meter runs only on screen, on the Microphone step, with the microphone allowed, on the input shown.
    fn sync_meter(&mut self) {
        let wanted = meter_wanted(self.window.isVisible(), self.current() == PageKind::Builtin(SetupStep::Microphone), self.mic_allowed, &self.microphones);
        if self.meter.as_ref().map(|m| &m.uid) == wanted.as_ref() { return; }
        self.meter = None;
        self.mic_page.set_level(0.0);
        if let Some(uid) = wanted { self.meter = microphone::Meter::start(&uid); }
    }

    /// The meter's poll: the level moves while sound comes in; nothing changes (or draws) otherwise.
    pub(crate) fn mic_tick(&mut self) {
        let Some(db) = self.meter.as_ref().and_then(|m| m.power()) else { return };
        self.mic_page.set_level(microphone::level_share(db));
    }

    /// The Microphone step's pop-up: the input picked there, when it is not the one in use.
    pub(crate) fn picked_microphone(&self) -> Option<String> {
        self.mic_page.picked().filter(|id| self.microphones.in_use.as_ref() != Some(id))
    }

    /// The offscreen renders' level (the meter needs a real input and the screen).
    pub(crate) fn show_level(&mut self, share: f64) {
        self.mic_page.set_level(share);
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
            PageKind::Builtin(SetupStep::Arsenal) => match leaving_arsenal(&self.arsenal, self.arsenal_add()) {
                ArsenalLeave::Next(tell) => {
                    // Moving on (Not now while an install runs, too): a landing install no longer turns it on.
                    self.arsenal_waiting = false;
                    tell
                }
                ArsenalLeave::OpenPage => vec![UiAction::InstallArsenal],
                ArsenalLeave::Install => {
                    self.arsenal_waiting = true;
                    return Advance::Wait(vec![UiAction::InstallArsenal]);
                }
            },
            // Start waits for every row (its button is off until then; Return does nothing either).
            PageKind::Builtin(SetupStep::Models) if !self.start_ready() => return Advance::Stay,
            PageKind::Builtin(SetupStep::Home) => leaving_home(&self.home).into_iter().collect(),
            PageKind::Builtin(SetupStep::Decider) => leaving_listening(&self.decider, self.decider_choice.as_deref()),
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
        self.next.setEnabled(self.current() != PageKind::Builtin(SetupStep::Models) || self.start_ready());
        let key = self.return_key();
        self.allow.setKeyEquivalent(&ns(if key == ReturnKey::Allow { "\r" } else { "" }));
        self.leave.setKeyEquivalent(&ns(if key == ReturnKey::Leave { "\r" } else { "" }));
        self.next.setKeyEquivalent(&ns(if key == ReturnKey::Next { "\r" } else { "" }));
        self.leave.setHidden(!choosing);
        self.agent.setHidden(self.current() != PageKind::Builtin(SetupStep::Welcome) || !self.agent_offered);
        // Right to left from the primary button: Leave as Is when shown, then Back.
        let y = ((FOOTER - 32.0) / 2.0).round();
        let mut x = width - FOOTER_PAD;
        for b in [&self.next, &self.leave, &self.agent, &self.back] {
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
        self.mic_allowed = permissions.iter().any(|(p, s)| *p == Permission::Microphone && *s == PermissionState::Granted);
        self.sync_meter();
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
        // A name still being typed keeps the title and Welcome when the mark changes.
        self.typed_views();
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

    /// Customise ▸ the name being typed: the wake word, the greeting, Start, the window's title and Welcome follow it
    /// before it is committed.
    pub(crate) fn name_typed(&mut self) {
        let typed = self.custom.field.stringValue().to_string();
        self.custom.preview_name(&self.identity, &typed);
        self.custom.show_error(None);
        self.typed_views();
        self.sync_buttons();
    }

    /// The window's title, Welcome and Allow's title with the name as typed on Customise (the saved one when empty).
    fn typed_views(&mut self) {
        let typed = self.custom.field.stringValue().to_string();
        let shown = Identity { name: if typed.trim().is_empty() { self.identity.name.clone() } else { typed.trim().to_string() }, ..self.identity.clone() };
        self.window.setTitle(&ns(&format!("Set Up {}", crate::identity::title_for(&shown.name))));
        self.welcome.sync(&shown);
        self.allow_page.title.setStringValue(&ns(&SetupStep::Permissions.title(&shown.name)));
    }

    /// Customise ▸ a mark (`index` in the strip): what it sends (`mark_choice`). A name that follows the mark goes into
    /// the field at once; the rest shows when the app's identity and voice come back.
    pub(crate) fn choose_mark(&mut self, index: usize) -> Vec<UiAction> {
        let Some((icon, _)) = self.custom.marks.get(index).cloned() else { return vec![] };
        let typed = self.custom.field.stringValue().to_string();
        let actions = mark_choice(&self.identity, &typed, &icon, &self.voice, self.voice_picked);
        for a in &actions {
            if let UiAction::Identity(IdentityAction::SetName(name)) = a {
                // A field being edited keeps its editor's text over a new value, so the chip and the greeting read
                // the old name one click behind (VM run 5 Nov): the editing ends first.
                if self.is_editing(&self.custom.field) {
                    self.window.makeFirstResponder(None);
                }
                self.custom.field.setStringValue(&ns(name));
                self.name_typed();
            }
        }
        actions
    }

    /// The Voice step's tile was clicked: from now on the voice is the user's.
    pub(crate) fn voice_clicked(&mut self) {
        self.voice_picked = true;
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
        let Some(o) = self.decider.options.get(index).filter(|o| o.present || self.decider.downloadable) else {
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

    /// Arsenal's state from the app. Returns true when an install Continue waited for has landed: Heimdall uses it
    /// now (the caller sends `SetArsenal(true)`), and the page moves on.
    pub fn set_arsenal(&mut self, a: &ArsenalSetup) -> bool {
        self.arsenal = a.clone();
        self.arsenal_page.sync(a, self.arsenal_add());
        if !self.arsenal_waiting {
            return false;
        }
        match (&a.found, &a.install) {
            (None, T3Install::Working(_)) => false,
            (found, _) => {
                self.arsenal_waiting = false;
                let landed = found.is_some();
                if landed && self.current() == PageKind::Builtin(SetupStep::Arsenal) && self.current + 1 < self.pages.len() {
                    self.go(self.current + 1);
                }
                landed && !a.on && self.arsenal_add()
            }
        }
    }

    fn arsenal_add(&self) -> bool {
        self.arsenal_choice.unwrap_or_else(|| arsenal_add_default(&self.arsenal))
    }

    /// Arsenal's tiles: Add Arsenal / Use it (true) or Not now. Only the choice; Continue acts on it.
    pub(crate) fn choose_arsenal(&mut self, add: bool) {
        self.arsenal_choice = Some(add);
        self.arsenal_page.sync(&self.arsenal, add);
    }

    /// Getting ready's rows from the app.
    pub fn set_downloads(&mut self, downloads: &ModelDownloads) {
        self.downloads = downloads.clone();
        self.downloads_page.sync(downloads, &self.readiness);
        self.sync_buttons();
    }

    /// Getting ready's other rows (the voice, listening, the decider, This Mac's model, the agent).
    pub fn set_readiness(&mut self, readiness: &SetupReadiness) {
        self.readiness = readiness.clone();
        self.downloads_page.sync(&self.downloads, readiness);
        self.sync_buttons();
    }

    /// Start may close Setup: every model in place and every readiness row green (Getting ready is the last step).
    pub(crate) fn start_ready(&self) -> bool {
        start_allowed(&self.downloads, &self.readiness)
    }

    /// The fix of Getting ready's row at `index` (a model's Retry, or the readiness row's own).
    pub(crate) fn fix_at(&self, index: usize) -> Option<ReadyFix> {
        fix_at(&self.downloads, &self.readiness, index)
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
        self.allowed_at = Some(std::time::Instant::now());
    }

    /// Another app became active (`bundle`; `own`: Heimdall itself): Setup comes back in front, key, when a permission
    /// prompt hands activation on (`refocus_after`).
    pub(crate) fn app_activated(&mut self, bundle: Option<String>, own: bool) {
        let previous = std::mem::replace(&mut self.last_front, if own { None } else { bundle.clone() });
        if own || !self.window.isVisible() { return; }
        let on_permission_steps = matches!(self.current(), PageKind::Builtin(SetupStep::Permissions | SetupStep::Microphone));
        if refocus_after(previous.as_deref(), bundle.as_deref(), self.allowed_at.map(|t| t.elapsed()), on_permission_steps) {
            self.refocus();
        }
    }

    /// Heimdall active and Setup key, so its menu bar shows and Return reaches it.
    pub(crate) fn refocus(&self) {
        if !self.window.isVisible() { return; }
        let app = objc2_app_kit::NSApplication::sharedApplication(self.mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        self.window.makeKeyAndOrderFront(None);
    }

    /// On the primary screen's visible frame (VM run 5 Nov: it once opened at y = 1068 on a 1080-high display, 12 pt
    /// showing behind the Dock): centred when `centre`, and always inside it, shrunk to fit if the screen is smaller.
    pub fn place(&self, centre: bool) {
        let screen = objc2_app_kit::NSScreen::screens(self.mtm).firstObject().or_else(|| objc2_app_kit::NSScreen::mainScreen(self.mtm));
        let Some(screen) = screen else { return };
        let frame = placed(self.window.frame(), screen.visibleFrame(), centre);
        self.window.setFrame_display(frame, false);
    }

    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    pub fn close(&mut self) {
        self.window.orderOut(None);
        self.closed();
        leave_app_mode(self.mtm);
    }

    /// Every way the window goes (Start, its close button, Quit): the meter's input and the prompt watch stop.
    pub(crate) fn closed(&mut self) {
        self.stop_meter();
        if let Some(t) = self.prompt_watch.take() { t.invalidate(); }
    }

    /// While Setup shows, a once-a-second look for macOS's prompts (VM run 5 Nov 08:18: a System Audio prompt that came
    /// late, mid-wizard, left the keyboard elsewhere when it closed, and Return did nothing). Nothing draws.
    pub(crate) fn watch_prompts(&mut self) {
        if self.prompt_watch.is_some() { return; }
        let block = block2::RcBlock::new(|_: std::ptr::NonNull<objc2_foundation::NSTimer>| {
            super::with_state(|s| if let Some(w) = s.setup.as_mut() { w.prompt_tick() });
        });
        self.prompt_watch = Some(unsafe { objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &block) });
    }

    /// One look: a prompt that was on screen and is gone hands focus back to Setup (`refocus_after_prompt`).
    pub(crate) fn prompt_tick(&mut self) {
        if !self.window.isVisible() { self.prompt_up = false; return; }
        let up = prompt_on_screen();
        let front = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication();
        let own = front.as_ref().is_some_and(|a| a.processIdentifier() == std::process::id() as i32);
        let bundle = front.and_then(|a| a.bundleIdentifier()).map(|b| b.to_string());
        if refocus_after_prompt(self.prompt_up, up, bundle.as_deref(), own, self.window.isKeyWindow()) { self.refocus(); }
        self.prompt_up = up;
    }

    /// The level meter's input stream closed: on every way the window goes (Start, its close button, Quit; review 5
    /// Nov: only Start closed it).
    pub(crate) fn stop_meter(&mut self) {
        self.meter = None;
        self.mic_page.set_level(0.0);
    }
}

/// `frame` inside `visible` (AppKit's bottom-left coordinates): centred when `centre`, else moved only as far as it
/// takes to be wholly on screen; never larger than the screen.
pub(crate) fn placed(frame: objc2_foundation::NSRect, visible: objc2_foundation::NSRect, centre: bool) -> objc2_foundation::NSRect {
    let w = frame.size.width.min(visible.size.width);
    let h = frame.size.height.min(visible.size.height);
    let (vx, vy) = (visible.origin.x, visible.origin.y);
    let (x, y) = if centre {
        (vx + ((visible.size.width - w) / 2.0).round(), vy + ((visible.size.height - h) / 2.0).round())
    } else {
        (frame.origin.x.clamp(vx, vx + visible.size.width - w), frame.origin.y.clamp(vy, vy + visible.size.height - h))
    };
    objc2_foundation::NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// The input the level meter listens to: only while the window shows, on the Microphone step, with the microphone
/// allowed, and an input to listen to (`None` stops it).
pub(crate) fn meter_wanted(visible: bool, on_step: bool, allowed: bool, microphones: &Microphones) -> Option<String> {
    if !(visible && on_step && allowed) { return None; }
    let (_, any, inputs, at) = microphone::mic_card(microphones);
    any.then(|| at.map(|i| inputs[i].0.clone())).flatten()
}

/// The system processes that show macOS's permission prompts (Accessibility's, TCC's); each may take activation
/// while its prompt is up.
const PROMPT_AGENTS: [&str; 4] = ["com.apple.accessibility.universalAccessAuthWarn", "com.apple.UserNotificationCenter", "com.apple.coreservices.uiagent", "com.apple.tccd"];

/// The processes that own macOS's permission prompts' windows, as the window list names them.
const PROMPT_OWNERS: [&str; 4] = ["UserNotificationCenter", "universalAccessAuthWarn", "CoreServicesUIAgent", "tccd"];

/// Whether one of macOS's prompts has a window on screen (the window list's owner names; no permission needed).
fn prompt_on_screen() -> bool {
    use objc2_foundation::{NSArray, NSDictionary, NSString};
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *mut AnyObject;
    }
    // kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements.
    let raw = unsafe { CGWindowListCopyWindowInfo(1 | 16, 0) };
    let Some(list) = (unsafe { Retained::<NSArray<NSDictionary<NSString, AnyObject>>>::from_raw(raw.cast()) }) else { return false };
    let key = NSString::from_str("kCGWindowOwnerName");
    list.iter().any(|d| d.objectForKey(&key).and_then(|o| o.downcast_ref::<NSString>().map(|s| PROMPT_OWNERS.contains(&s.to_string().as_str()))).unwrap_or(false))
}

/// Whether a closed prompt hands focus back to Setup: a prompt was on screen at the last look and is gone, and what is
/// in front is what a prompt leaves (Finder, a prompt's own process, nothing) or Heimdall without Setup key. Any
/// prompt, any step, any time while Setup shows (VM run 5 Nov 08:18); never over an app the user went to.
pub(crate) fn refocus_after_prompt(was_up: bool, up: bool, front: Option<&str>, own_front: bool, setup_key: bool) -> bool {
    if !was_up || up { return false; }
    if own_front { return !setup_key; }
    front.is_none_or(|b| b == "com.apple.finder" || PROMPT_AGENTS.contains(&b))
}

/// Whether Setup takes activation back when `activated` becomes the active app (VM run 5 Nov 05:07: after the prompts
/// closed, macOS made Finder active, its menu bar showed and Return went to Finder on "Can you hear me?"). Back when a
/// prompt hands activation on, and when Finder comes up on Allow or Microphone within two minutes of Allow All; never
/// over a prompt itself or System Settings (the user's Open Settings).
pub(crate) fn refocus_after(previous: Option<&str>, activated: Option<&str>, since_allow: Option<std::time::Duration>, on_permission_steps: bool) -> bool {
    let prompt = |b: Option<&str>| b.is_some_and(|b| PROMPT_AGENTS.contains(&b));
    if prompt(activated) || matches!(activated, Some("com.apple.systempreferences" | "com.apple.Settings")) { return false; }
    if prompt(previous) { return true; }
    on_permission_steps && activated == Some("com.apple.finder") && since_allow.is_some_and(|d| d < std::time::Duration::from_secs(120))
}

/// While Setup shows, Heimdall is a regular app: its own menu bar (the app menu with Quit, and Edit for the name
/// fields) instead of the previous app's, which an accessory app leaves in place (VM run 5 Nov: Finder's menus while
/// Setup had focus). Back to a menu-bar app when Setup closes.
pub(crate) fn enter_app_mode(mtm: MainThreadMarker, target: &AnyObject) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem};
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let item = |title: &str, action: Option<objc2::runtime::Sel>, key: &str| unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &ns(title), action, &ns(key))
    };
    let bar = NSMenu::new(mtm);
    let app_item = item("", None, "");
    let app_menu = NSMenu::new(mtm);
    // Quit as the status menu's Quit does (the app drains and answers with Terminate).
    let quit = item(&format!("Quit {}", crate::identity::app_title()), Some(sel!(quit:)), "q");
    unsafe { quit.setTarget(Some(target)) };
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    bar.addItem(&app_item);
    let edit_item = item("Edit", None, "");
    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Edit"));
    for (title, action, key) in [("Undo", sel!(undo:), "z"), ("Cut", sel!(cut:), "x"), ("Copy", sel!(copy:), "c"), ("Paste", sel!(paste:), "v"), ("Select All", sel!(selectAll:), "a")] {
        edit.addItem(&item(title, Some(action), key));
    }
    edit_item.setSubmenu(Some(&edit));
    bar.addItem(&edit_item);
    app.setMainMenu(Some(&bar));
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
}

pub(crate) fn leave_app_mode(mtm: MainThreadMarker) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

/// Welcome's second button.
pub(crate) const AGENT_BUTTON: &str = "Let my agent set it up";

/// The one line the user pastes into their coding agent: this app's own `setup` command and its guide (docs/setup.md).
pub(crate) fn agent_line(exe: &std::path::Path) -> String {
    format!("Set up Heimdall for me. Run \"{}\" setup --help and follow it.", exe.display())
}

/// Let my agent set it up: the line, and Copy (to the clipboard) or Done.
pub(crate) fn show_agent_line(mtm: MainThreadMarker) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "/Applications/Heimdall Alpha.app/Contents/MacOS/Heimdall".into());
    let line = agent_line(&exe);
    let alert = objc2_app_kit::NSAlert::new(mtm);
    alert.setMessageText(&ns("Paste this into your coding agent"));
    alert.setInformativeText(&ns(&line));
    alert.addButtonWithTitle(&ns("Copy"));
    alert.addButtonWithTitle(&ns("Done"));
    if alert.runModal() == objc2_app_kit::NSAlertFirstButtonReturn {
        let board = objc2_app_kit::NSPasteboard::generalPasteboard();
        board.clearContents();
        unsafe { board.setString_forType(&ns(&line), objc2_app_kit::NSPasteboardTypeString) };
    }
}

/// The words of the alert that closing an unfinished Setup shows: (message, informative, buttons).
pub(crate) const QUIT_ALERT: (&str, &str, [&str; 2]) = ("Quit Heimdall?", "Setup isn\u{2019}t finished.", ["Quit", "Cancel"]);

/// One native alert: Quit (true) or Cancel.
pub(crate) fn confirm_quit(mtm: MainThreadMarker) -> bool {
    let alert = objc2_app_kit::NSAlert::new(mtm);
    alert.setMessageText(&ns(QUIT_ALERT.0));
    alert.setInformativeText(&ns(QUIT_ALERT.1));
    for b in QUIT_ALERT.2 {
        alert.addButtonWithTitle(&ns(b));
    }
    alert.runModal() == objc2_app_kit::NSAlertFirstButtonReturn
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
    fn wizard_leaving_listening_sets_the_precision_then_starts_the_downloads() {
        let d = render::demo_decider();
        assert_eq!(leaving_listening(&d, None), vec![UiAction::StartModelDownloads]);
        assert_eq!(leaving_listening(&d, Some("8bit")), vec![UiAction::SetDeciderPrecision("8bit".into()), UiAction::StartModelDownloads]);
        // The public edition: a precision not on this Mac can be chosen; choosing it downloads it.
        let mut public = d.clone();
        public.options[1].present = false;
        assert_eq!(leaving_decider(&public, Some("8bit")), None, "personal: only what is here");
        public.downloadable = true;
        assert_eq!(leaving_decider(&public, Some("8bit")), Some(UiAction::SetDeciderPrecision("8bit".into())));
        assert_eq!(SetupStep::Models.title(""), "Getting ready");
        let at = |s: SetupStep| SetupStep::ALL.iter().position(|x| *x == s).unwrap();
        assert!(at(SetupStep::Vella) < at(SetupStep::Customise) && at(SetupStep::Customise) + 1 == at(SetupStep::Models), "Getting ready is last");
    }

    /// The pairing rule: a mark brings its name and Phonon S voice; a name the user typed stays, and so does a voice
    /// they picked on the Voice step (or one chosen in the menu that is no mark's).
    #[test]
    fn wizard_a_mark_brings_its_name_and_voice_unless_the_user_chose_their_own() {
        use crate::speech_voice::{demo_speech_voice, LocalVoice, SpeechVoiceAction};
        let sparkle = Icon::Symbol { name: "sparkle".into() };
        let heimdall = Identity { name: "Heimdall".into(), user_name: "Ada".into(), icon: Icon::Helmet };
        let phonon = demo_speech_voice(false, LocalVoice::Phonon, false);
        assert_eq!(voice_chosen(&phonon), Some("Edwin"));
        let all = vec![
            UiAction::Identity(IdentityAction::SetIcon(sparkle.clone())),
            UiAction::Identity(IdentityAction::SetName("Lumi".into())),
            UiAction::SpeechVoice(SpeechVoiceAction::SetPreset("Lottie".into())),
        ];
        assert_eq!(mark_choice(&heimdall, "Heimdall", &sparkle, &phonon, false), all, "the default name and voice follow the mark");
        assert_eq!(mark_choice(&heimdall, "", &sparkle, &phonon, false), all, "an empty field follows too");
        // Another mark's name in the field is that mark's default, not the user's: it follows.
        let lumi = Identity { name: "Lumi".into(), icon: sparkle.clone(), ..heimdall.clone() };
        assert_eq!(mark_choice(&lumi, "Lumi", &Icon::Helmet, &phonon, false)[1], UiAction::Identity(IdentityAction::SetName("Heimdall".into())));
        // A typed name is kept; the voice still follows.
        assert_eq!(mark_choice(&heimdall, "Jarvis", &sparkle, &phonon, false), [all[0].clone(), all[2].clone()]);
        // A voice picked on the Voice step is kept.
        assert_eq!(mark_choice(&heimdall, "", &sparkle, &phonon, true), all[..2]);
        // A voice chosen in the menu that is no mark's is kept; ElevenLabs or Drama 3 speaking: nothing to follow.
        let zoey = SpeechVoiceState { preset: "Zoey".into(), ..phonon.clone() };
        assert_eq!(mark_choice(&heimdall, "", &sparkle, &zoey, false), all[..2]);
        assert_eq!(mark_choice(&heimdall, "", &sparkle, &demo_speech_voice(true, LocalVoice::Phonon, true), false), all[..2]);
        // Every persona: a name that validates as the menu checks it, a voice Phonon S has, all different.
        let names: std::collections::BTreeSet<&str> = PERSONAS.iter().map(|p| p.1).collect();
        let voices: std::collections::BTreeSet<&str> = PERSONAS.iter().map(|p| p.2).collect();
        assert_eq!((names.len(), voices.len()), (12, 12), "every persona's name and voice are its own");
        for (key, name, _) in PERSONAS {
            assert!(super::super::personalise::commit(&Identity::default(), super::super::personalise::Field::Name, name).is_ok(), "{name}");
            assert!(key == "helmet" || crate::identity::BUNDLED.contains(&key), "{key} is a mark the grid offers");
        }
        assert_eq!(persona(&Icon::Helmet), Some(("Heimdall", "Edwin")), "Heimdall keeps Edwin");
        assert_eq!(persona(&Icon::Image { path: "/x.png".into() }), None);
    }

    /// Arsenal: nothing installs without Continue on Add; Use it turns it on, Not now off; without Homebrew, its page.
    #[test]
    fn wizard_arsenal_installs_only_on_continue_with_add_and_use_it_turns_it_on() {
        let found = ArsenalApp { path: "/Applications/Arsenal.app".into(), version: "1.0.2".into() };
        let missing = ArsenalSetup { found: None, brew: true, install: T3Install::Idle, on: false };
        assert!(arsenal_add_default(&missing));
        assert_eq!(leaving_arsenal(&missing, true), ArsenalLeave::Install);
        assert_eq!(leaving_arsenal(&missing, false), ArsenalLeave::Next(vec![]), "Not now does nothing");
        let no_brew = ArsenalSetup { brew: false, ..missing.clone() };
        assert!(!arsenal_add_default(&no_brew), "Add would only open a page: Not now by default");
        assert_eq!(leaving_arsenal(&no_brew, true), ArsenalLeave::OpenPage);
        assert_eq!(arsenal::arsenal_tiles(&no_brew), "Get Arsenal…");
        let working = ArsenalSetup { install: T3Install::Working("Installing Arsenal with Homebrew…".into()), ..missing.clone() };
        assert_eq!(leaving_arsenal(&working, true), ArsenalLeave::Next(vec![]), "one install at a time");
        let here = ArsenalSetup { found: Some(found), ..missing.clone() };
        assert_eq!(arsenal::arsenal_tiles(&here), "Use it");
        assert_eq!(leaving_arsenal(&here, true), ArsenalLeave::Next(vec![UiAction::SetArsenal(true)]));
        let on = ArsenalSetup { on: true, ..here.clone() };
        assert_eq!(leaving_arsenal(&on, true), ArsenalLeave::Next(vec![]), "already on");
        assert_eq!(leaving_arsenal(&on, false), ArsenalLeave::Next(vec![UiAction::SetArsenal(false)]));
        assert_eq!(SetupStep::Arsenal.title(""), "Do you want Arsenal?");
    }

    /// VM run 5 Nov: the window once opened at y = 1068 on a 1080-high screen. Placed, it is always on screen.
    #[test]
    fn wizard_window_is_always_placed_on_the_visible_screen() {
        use objc2_foundation::NSRect;
        let r = |x, y, w, h| NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
        let visible = r(0.0, 70.0, 1920.0, 985.0);
        let off = r(560.0, 1068.0, 800.0, 680.0);
        let centred = placed(off, visible, true);
        assert_eq!((centred.origin.x, centred.origin.y), (560.0, 70.0 + ((985.0f64 - 680.0) / 2.0).round()));
        let kept = placed(off, visible, false);
        assert!(kept.origin.y + kept.size.height <= 70.0 + 985.0 && kept.origin.y >= 70.0, "pulled back on screen");
        assert_eq!(kept.origin.x, 560.0, "only as far as needed");
        let small = placed(off, r(0.0, 0.0, 700.0, 500.0), true);
        assert_eq!((small.size.width, small.size.height), (700.0, 500.0), "never larger than the screen");
        // Allow: Screen Recording and Accessibility are optional, asked or not, until allowed.
        for state in [PermissionState::NotDetermined, PermissionState::Denied] {
            assert_eq!(permission_words(Permission::ScreenRecording, state), ("Optional", false));
            assert_eq!(permission_words(Permission::Accessibility, state), ("Optional", false));
        }
        assert_eq!(permission_words(Permission::Microphone, PermissionState::NotDetermined).0, "Not asked");
        // System audio is optional as well: listening runs on the microphone alone.
        for state in [PermissionState::NotDetermined, PermissionState::Unconfirmed, PermissionState::Denied] {
            assert_eq!(permission_words(Permission::SystemAudio, state), ("Optional", false));
        }
        assert_eq!(permission_words(Permission::SystemAudio, PermissionState::Granted), ("Allowed", true));
        assert_eq!(permission_words(Permission::Microphone, PermissionState::Denied).0, "Denied", "the microphone's denial is real");
        assert_eq!(permission_words(Permission::ScreenRecording, PermissionState::Granted), ("Allowed", true));
    }

    /// Review 5 Nov (merge): the meter's input stream is open only while the step shows; a closed or hidden window has
    /// none (`stop_meter` on every close path).
    #[test]
    fn wizard_the_level_meter_listens_only_while_its_step_shows() {
        let mic = |id: &str| Microphone { id: id.into(), name: id.into(), connected: true, built_in: false };
        let m = Microphones { devices: vec![mic("a"), mic("b")], in_use: Some("b".into()), ..Default::default() };
        assert_eq!(meter_wanted(true, true, true, &m).as_deref(), Some("b"));
        assert_eq!(meter_wanted(false, true, true, &m), None, "the window closed or hidden");
        assert_eq!(meter_wanted(true, false, true, &m), None, "another step");
        assert_eq!(meter_wanted(true, true, false, &m), None, "the microphone not allowed");
        assert_eq!(meter_wanted(true, true, true, &Microphones::default()), None, "no input");
    }

    /// VM run 5 Nov 08:18: a System Audio prompt mid-wizard (on T3, minutes after Allow All) left Return going nowhere
    /// when it closed. Any prompt that closes while Setup shows hands focus back, unless the user went to another app.
    #[test]
    fn wizard_takes_focus_back_after_any_prompt_closes() {
        let finder = Some("com.apple.finder");
        assert!(refocus_after_prompt(true, false, finder, false, false), "Finder after the prompt");
        assert!(refocus_after_prompt(true, false, Some("com.apple.UserNotificationCenter"), false, false));
        assert!(refocus_after_prompt(true, false, None, true, false), "Heimdall in front without Setup key");
        assert!(!refocus_after_prompt(true, false, None, true, true), "already key");
        assert!(!refocus_after_prompt(true, true, finder, false, false), "the prompt is still up");
        assert!(!refocus_after_prompt(false, false, finder, false, false), "no prompt: Finder is the user's");
        assert!(!refocus_after_prompt(true, false, Some("com.apple.Safari"), false, false), "the user went elsewhere");
        assert!(!refocus_after_prompt(true, false, Some("com.apple.systempreferences"), false, false), "Open Settings");
    }

    /// VM run 5 Nov 05:07: after the permission prompts closed, Finder was active and Return went to it on "Can you hear
    /// me?". Setup takes activation back from a closing prompt, and from Finder right after Allow All.
    #[test]
    fn wizard_takes_focus_back_after_a_permission_prompt() {
        use std::time::Duration;
        let ax = Some("com.apple.accessibility.universalAccessAuthWarn");
        let finder = Some("com.apple.finder");
        let soon = Some(Duration::from_secs(5));
        assert!(refocus_after(ax, finder, None, false), "a prompt handed activation on");
        assert!(!refocus_after(None, ax, soon, true), "never over the prompt itself");
        assert!(!refocus_after(ax, Some("com.apple.systempreferences"), soon, true), "the user's Open Settings");
        assert!(refocus_after(None, finder, soon, true), "Finder right after Allow All, on Allow or Microphone");
        assert!(!refocus_after(None, finder, soon, false), "on another step Finder is the user's choice");
        assert!(!refocus_after(None, finder, Some(Duration::from_secs(600)), true), "long after Allow All");
        assert!(!refocus_after(None, finder, None, true), "Allow All never clicked");
        assert!(!refocus_after(None, Some("com.apple.Safari"), soon, true));
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
        assert_eq!(titles, ["Local Splash Engine", "Custom API", "OpenCode Go subscription", "Claude Code subscription", "Codex subscription"]);
        // No Anthropic API key tile (readiness audit, 3 Nov); a server of the user's own is Custom (5 Nov).
        assert!(ProviderChoice::ALL.iter().all(|c| !matches!(c.menu_action(), ProviderAction::Connect("anthropic"))));
        // The tiles: every choice once, the subscriptions first, then this Mac and Custom; no card row is an API key.
        assert_eq!(BRAIN_ORDER.map(tile_title), ["Codex", "Claude Code", "OpenCode Go", "This Mac", "Custom"]);
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
        assert_eq!(SetupStep::ALL.last(), Some(&SetupStep::Models), "Getting ready, where Start waits for every row (5 Nov)");
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

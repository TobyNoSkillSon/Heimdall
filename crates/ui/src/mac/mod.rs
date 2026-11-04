//! The AppKit side: one main-thread `State`, one Objective-C `Controller` that receives every target/action and
//! delegate call, and the update queue.

mod agent;
// The island domain's renderer lives in src/island/mac/ (one folder per domain); module paths stay crate::mac::*.
pub(crate) mod agent_render;
pub(crate) mod bugs;
/// Background Tasks greyed under one warning triangle in emergency mode (docs/claude-relay.md).
pub(crate) mod background_paused;
/// Background Tasks ▸ Model: providers opening in place to their models and thinking levels, and its renders.
pub(crate) mod background_picker;
pub(crate) mod background_picker_render;
/// `--claude-relay`: the relay's menu renders and checks.
pub(crate) mod relay_render;
mod controls;
#[path = "../island/mac/draw/mod.rs"]
pub(crate) mod draw;
#[path = "../island/mac/drop/mod.rs"]
pub(crate) mod drop_target;
#[path = "../island/mac/gallery.rs"]
pub(crate) mod gallery;
mod helmet;
pub(crate) mod offscreen;
#[path = "../island/mac/island/mod.rs"]
pub(crate) mod island;
#[path = "../island/mac/lab.rs"]
pub(crate) mod lab;
#[path = "../island/mac/space.rs"]
pub(crate) mod overlay_space;
pub(crate) mod menu;
/// [chrome] Support the Developer's resting lighter row.
pub(crate) mod prominent;
/// [chrome] `--chrome`: the chrome's offscreen renders and checks.
pub(crate) mod chrome_render;
pub(crate) mod menu_rows;
pub mod permissions;
pub(crate) mod provider_key;
pub(crate) mod provider_rows;
pub(crate) mod model_choice;
/// Agent ▸ Model ▸ On This Mac's precision rows.
pub(crate) mod engine_rows;
/// `--engine-models`: the Engine section's models and precisions, rendered and checked.
pub(crate) mod engine_render;
/// The menu's Engine section: the engine's live state, the models on this Mac, Add Model ▸.
pub(crate) mod engine_section;
/// `--engine-section`: the Engine section's states, the Engine tile and the foldable sections, rendered and checked.
pub(crate) mod engine_section_render;
/// The menu's Decider section: the decider's version and its precision, click-then-Confirm, hot-swapped.
pub(crate) mod decider_section;
/// `--decider-section`: the Decider section's states and Setup's Decider step, rendered and checked.
pub(crate) mod decider_section_render;
/// `--model-choice`: Agent ▸ Model's click-then-confirm renders and checks.
pub(crate) mod model_choice_render;
/// `--menu-climb`: the whole menu in every state the UX climb judges (lab/climb/menu-ux).
pub(crate) mod menu_climb_render;
mod render;
pub(crate) mod setup;
// Global shortcuts (24 Oct): Carbon registration, the menu's suspension, macOS's own shortcuts.
pub(crate) mod shortcuts;
mod speaker;
mod friend_invite;
mod speakers;
// [speech] Speaking ▸ the voice rows and their target (night 1022 menu).
pub(crate) mod speaking_menu;
pub(crate) mod personalise;
#[path = "../island/mac/streams.rs"]
pub(crate) mod streams;
pub(crate) mod submenus_left;
pub(crate) mod menu_live;
pub(crate) mod menu_motion;
pub(crate) mod menu_group;
mod transcript;
pub(crate) mod util;

use std::cell::RefCell;
use std::sync::{Mutex, OnceLock};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy, NSButton, NSMenu,
    NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem, NSTextField,
    NSVariableStatusItemLength, NSWindow, NSAlertSecondButtonReturn, NSModalResponse,
};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSTimer};

use crate::island::object::{Event, Switch};
use crate::model::*;
use crate::Options;

/// Everything the UI knows, owned by the main thread.
pub(crate) struct State {
    mtm: MainThreadMarker,
    controller: Retained<Controller>,
    options: Options,
    on_action: Box<dyn Fn(UiAction)>,
    /// None only in offscreen renders, which build the real menu with nothing in the menu bar.
    status_item: Option<Retained<NSStatusItem>>,
    menu: Retained<NSMenu>,
    menu_open: bool,
    /// [menu lane, night 1027] A state change came while the menu was closed and its rows were not synced: the next
    /// open rebuilds it (`menuNeedsUpdate:`), or, if that could not take the state, syncs it (`menuWillOpen:`).
    menu_stale: bool,
    /// Background Tasks ▸ Model's submenu, laid again in place when its model set changes while the menu is open.
    background_menu: Option<Retained<NSMenu>>,
    /// The Agent submenu's radio buttons, in `Harness::ALL` order (kept in sync while the menu tracks).
    /// Microphone submenu controls: (button, is an extra-mic switch, device UID); a button's tag indexes this list.
    microphone_buttons: Vec<(Retained<NSButton>, bool, String)>,
    microphones: Microphones,
    /// App Audio submenu checkboxes: (button, app key); a button's tag indexes this list.
    app_buttons: Vec<(Retained<NSButton>, String)>,
    app_audio: AppAudio,
    /// Seeing ▸ Recording radio buttons: (button, its choice); a button's tag indexes this list.
    screen_buttons: Vec<(Retained<NSButton>, menu::ScreenChoice)>,
    /// The switch tiles, in `Tile::ALL` order.
    tile_views: Vec<Retained<controls::TileView>>,
    /// The last engine update (or the asked-for state after a tile click, until the app answers).
    engine: EngineStatus,
    /// The engine section's row (titled with the running model), its picker's radio buttons (button, model id; a
    /// button's tag indexes this list) and its fact lines, in `EngineStatus::facts` order.
    engine_item: Option<Retained<NSMenuItem>>,
    engine_buttons: Vec<(Retained<NSButton>, String)>,
    engine_facts: Vec<(Retained<NSMenuItem>, Retained<controls::LabelRow>)>,
    /// Inference Engine ▸ Thinking (the local model's level).
    engine_thinking: Option<Retained<objc2_app_kit::NSSegmentedControl>>,
    /// The Engine section's live rows (`engine_section`).
    pub(crate) engine_rows: engine_section::Rows,
    /// Decider ▸ the app's status (`UiUpdate::Decider`), the precision clicked and waiting for Confirm (dropped when the
    /// menu closes), and the section's rows.
    pub(crate) decider: crate::model::DeciderStatus,
    pub(crate) decider_pending: Option<String>,
    pub(crate) decider_rows: decider_section::Rows,
    /// Each foldable section's header and the rows it shows or hides (`menu::apply_sections`).
    pub(crate) sections: std::collections::BTreeMap<MenuSection, (Retained<controls::CheckRow>, Vec<Retained<NSMenuItem>>)>,
    /// The line under the tiles (hidden while there is nothing to say) and the Agent section's session line.
    status_line: Option<(Retained<NSMenuItem>, Retained<controls::LabelRow>)>,
    session_item: Option<(Retained<NSMenuItem>, Retained<controls::LabelRow>)>,
    login_row: Option<Retained<controls::CheckRow>>,
    /// Speaking ▸ Wake Sound (speech-policy, night 3).
    wake_sound_row: Option<Retained<controls::CheckRow>>,
    /// Background Tasks' toggles, one per job.
    background_task_rows: Vec<(crate::model::BackgroundTask, Retained<controls::CheckRow>)>,
    /// Their emergency-mode stand-in: the rows greyed under one warning triangle (`background_paused`).
    background_paused: Option<background_paused::Paused>,
    /// Who answers for the agent (`UiUpdate::AgentRelay`): the notch's emergency token and Background Tasks' pause.
    relay: crate::model::RelayIndicator,
    /// Agent ▸ Model ▸ Claude Code subscription's radio.
    relay_row: Option<Retained<objc2_app_kit::NSButton>>,
    /// Agent ▸ Model's pending choice: clicked, not confirmed yet (`model_choice`). Dropped when the menu closes.
    pending_choice: Option<model_choice::Choice>,
    /// The Claude models the relay can run (`UiUpdate::RelayModels`), its rows (radio, model id) under Claude Code subscription,
    /// and that row's disclosure with the rows it shows.
    relay_models: Vec<crate::model::RelayModel>,
    /// The harness instances T3 runs (`UiUpdate::T3Instances`): Background Tasks ▸ Model's T3 providers.
    t3_instances: Vec<crate::model::T3Instance>,
    /// Background tasks the fallback moved off their pick (`UiUpdate::BackgroundRoutes`): Background Tasks' sign.
    background_routes: Vec<(crate::model::BackgroundTask, String, String)>,
    relay_model_rows: Vec<(Retained<objc2_app_kit::NSButton>, String)>,
    /// The Claude accounts the relay can run on (`UiUpdate::RelayAccounts`) and their rows (radio, instance id;
    /// `None`: Automatic), under the models.
    relay_accounts: Vec<crate::model::RelayAccount>,
    relay_account_rows: Vec<(Retained<objc2_app_kit::NSButton>, Option<String>)>,
    relay_detail: (Option<Retained<objc2_app_kit::NSButton>>, Vec<Retained<NSMenuItem>>),
    /// The glowing Confirm row under the pending choice.
    confirm_row: Option<(Retained<NSMenuItem>, Retained<model_choice::ConfirmView>)>,
    islands_shown_row: Option<Retained<controls::CheckRow>>,
    /// Listening and Seeing as last told to the island (`sync_switches`), and whether the app has said how Listening is.
    switches_told: Option<(bool, bool)>,
    listen_known: bool,
    /// Shortcuts ▸ its items, for their checks (`menu::sync_shortcuts`).
    shortcut_items: Vec<(crate::shortcuts::Shortcut, Retained<NSMenuItem>)>,
    islands_on_streams_row: Option<Retained<controls::CheckRow>>,
    speed_row: Option<Retained<controls::SpeedRow>>,
    /// The Speed row's note while its sample waits for Jarvis (`UiUpdate::VoiceSample`), kept across menu rebuilds.
    voice_sample_note: Option<String>,
    /// [speech] Speaking ▸ the voice (`UiUpdate::SpeechVoice`) and its rows (night 1022 menu).
    speech_voice: crate::speech_voice::SpeechVoiceState,
    speaking_rows: speaking_menu::Rows,
    /// Personalise ▸ (the mark grid and the name fields) while the menu is built.
    pub(crate) personalise_rows: personalise::Rows,
    /// Listening clicked, not yet confirmed by a `Listen` update: the tile shows the asked-for state meanwhile.
    listen_pending: Option<bool>,
    /// Render pass only: draw the menus in Aqua.
    render_light: bool,

    listen: ListenState,
    /// Clock time Heimdall was turned off, for the fact line.
    off_since: Option<String>,
    component_status: std::collections::BTreeMap<String, String>,
    sources: Vec<Source>,
    agent: AgentState,
    permissions: Vec<(Permission, PermissionState)>,
    speakers_list: Vec<KnownSpeaker>,
    settings_value: Settings,
    runtime_settings: RuntimeSettings,
    /// Inference Engine ▸ API Providers as the app last reported it (with the asked-for selection meanwhile).
    api: ApiProviders,
    api_rows: menu::ApiRows,
    /// API model rows whose details the user opened; collapsed by default, kept for this run.
    api_expanded: std::collections::BTreeSet<(String, String)>,
    api_item: Option<Retained<NSMenuItem>>,
    key_panel: Option<provider_key::KeyPanel>,
    /// [menu lane] Each picker's Add Provider: the user's open/closed choice this run, and the header with its rows.
    add_provider_choice: std::collections::BTreeMap<menu::Picker, bool>,
    /// The folded lists header presses sent that the app has not echoed yet, oldest first, with when each was sent.
    /// While one is unanswered the menu keeps its own list: an echo of an earlier press never folds or opens it again.
    sections_sent: std::collections::VecDeque<(std::time::Instant, Vec<MenuSection>)>,
    add_provider_rows: std::collections::BTreeMap<menu::Picker, (Retained<controls::CheckRow>, Vec<Retained<NSMenuItem>>)>,
    /// [providers] Offscreen only: Background Tasks ▸ Model ▸ Add Model… asked for Agent ▸ Model (the live app pops it up).
    pub(crate) agent_models_requested: bool,
    /// [menu lane] Background Tasks ▸ Model's radios and the section's caption.
    background_rows: background_picker::Rows,
    /// Background Tasks: a task's own Model row under its toggle and the picker it opens (Islands, 30 Oct 03:37).
    task_pickers: Vec<background_picker::TaskPicker>,
    background_caption: Option<Retained<controls::LabelRow>>,

    agent_session: AgentSession,
    /// The live session's rows and run state, kept whether or not the window is open, so it opens on them.
    pub(crate) agent_view: crate::agent_view::AgentView,
    pub(crate) agent_window: Option<agent::AgentWindow>,
    /// Off in offscreen renders, which resize the window: its frame is not remembered.
    pub(crate) agent_autosave: bool,
    transcript: Option<transcript::TranscriptWindow>,
    setup: Option<setup::SetupWindow>,
    /// Setup's home and T3 Code states, kept so a wizard opened later starts on them.
    setup_home: HomeState,
    setup_t3: T3Setup,
    setup_vella: VellaSetup,
    setup_harnesses: HarnessSetup,
    /// The updater's menu row, shown above Support the Developer (public edition; the app sends it).
    pub(crate) update_row: Option<UpdateRow>,
    speakers: Option<speakers::SpeakersWindow>,
    bug_notes: Vec<BugNote>,
    /// Renders only: the Memory row's reading, instead of this process's own tree (`crate::memory::current`).
    memory_demo: Option<crate::memory::MemoryReport>,
    bug_window: Option<bugs::BugNotesWindow>,
    name_entry: speaker::NameEntry,
    /// Friends (personal edition): the friends the submenu lists, and the endpoint IDs its rows carry, in menu order.
    friends: Vec<crate::FriendRow>,
    friend_menu_ids: Vec<String>,
    friend_invite: friend_invite::InvitePanel,
    setup_timer: Option<Retained<NSTimer>>,
    _activation_observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Runs `f` on the state now, or, if AppKit called back while the state is borrowed, on the next main-queue turn.
pub(crate) fn with_state(f: impl FnOnce(&mut State) + 'static) {
    let pending = STATE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(state) = guard.as_mut() {
                f(state);
            }
            None
        }
        Err(_) => Some(f),
    });
    if let Some(f) = pending {
        let f = MainOnly(Box::new(f) as Box<dyn FnOnce(&mut State)>);
        DispatchQueue::main().exec_async(move || {
            let f = f;
            with_state(f.0)
        });
    }
}

/// Runs `f` on the state now; if AppKit called back while the state is borrowed, once more on the next main-queue turn,
/// then gives up. A delayed menu callback must not keep re-queuing itself while the menu tracks.
fn with_state_retry_once(f: impl FnOnce(&mut State) + 'static) {
    let pending = STATE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(state) = guard.as_mut() {
                f(state);
            }
            None
        }
        Err(_) => Some(f),
    });
    if let Some(f) = pending {
        let f = MainOnly(Box::new(f) as Box<dyn FnOnce(&mut State)>);
        DispatchQueue::main().exec_async(move || {
            let f = f;
            with_state_now(f.0);
        });
    }
}

/// A press on a precision row's controls (`State::precision`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Press { Download, Stop, Trash, Delete }

/// Runs `f` on the state on the next main-queue turn: a model row's click re-lays the rows it sits among, so the
/// clicked control is never taken out of its menu while AppKit is still handling its event (review 28 Oct).
pub(crate) fn with_state_after_event(f: impl FnOnce(&mut State) + 'static) {
    let f = MainOnly(Box::new(f) as Box<dyn FnOnce(&mut State)>);
    DispatchQueue::main().exec_async(move || {
        let f = f;
        with_state(f.0)
    });
}

/// Runs `f` on the state now and returns its result; `None` if the state is borrowed (an AppKit re-entry).
pub(crate) fn with_state_now<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.try_borrow_mut().ok().and_then(|mut guard| guard.as_mut().map(f)))
}

thread_local! {
    /// Updater alerts waiting for their timer (main thread only).
    static PENDING_ALERTS: std::cell::RefCell<std::collections::VecDeque<UpdateAlert>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    /// The updater alert on screen (non-modal) and whether its first button installs.
    static OPEN_ALERT: std::cell::RefCell<Option<(Retained<objc2_app_kit::NSAlert>, bool)>> = const { std::cell::RefCell::new(None) };
}

/// Quits now. Updater alerts are non-modal (`showUpdateAlert:`); should any modal session be open, it is aborted
/// first and the quit retried on the next main-queue turn, so nothing on screen blocks Quit (VM finding P-01).
fn terminate(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    if app.modalWindow().is_some() {
        app.abortModal();
        DispatchQueue::main().exec_async(|| {
            if let Some(mtm) = MainThreadMarker::new() { terminate(mtm) }
        });
        return;
    }
    app.terminate(None);
}

/// Moves main-thread-only values into a main-queue block. Only ever sent from the main thread to the main queue.
pub(crate) struct MainOnly<T>(pub T);
unsafe impl<T> Send for MainOnly<T> {}

pub(crate) fn post(update: UiUpdate) {
    if !updates().lock().unwrap().push(update) { return; }
    DispatchQueue::main().exec_async(|| {
        let pending = updates().lock().unwrap().drain();
        with_state(move |s| { for update in pending { s.apply(update); } });
    });
}

fn updates() -> &'static Mutex<crate::updates::UpdateQueue> {
    static UPDATES: OnceLock<Mutex<crate::updates::UpdateQueue>> = OnceLock::new();
    UPDATES.get_or_init(Default::default)
}

/// The island family follows the user across Spaces: whatever is showing (the pill, the question card, a speaker prompt)
/// is ordered in again on the Space he moves to. Event-driven: nothing runs between Space changes. The observer lives
/// as long as the app.
pub(crate) fn watch_spaces() {
    let on_space = block2::RcBlock::new(|_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
        DispatchQueue::main().exec_async(|| {
            island::space_changed();
            with_state(|s| s.name_entry.space_changed());
            with_state(|s| s.friend_invite.space_changed());
        });
    });
    let center = objc2_app_kit::NSWorkspace::sharedWorkspace().notificationCenter();
    let observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(Some(objc2_app_kit::NSWorkspaceActiveSpaceDidChangeNotification), None, None, &on_space)
    };
    std::mem::forget(observer);
}

pub(crate) fn run(options: Options, on_action: Box<dyn Fn(UiAction)>) -> ! {
    let mtm = MainThreadMarker::new().expect("ui::run must be called on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // Vella: NSApp.appearance = darkAqua.
    let dark = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua });
    app.setAppearance(dark.as_deref());

    // Every submenu opens to the left.
    submenus_left::install();
    menu_motion::install();
    menu_group::install();
    let controller = Controller::new(mtm);
    // A lab instance leaves the menu bar to the user's own Heimdall.
    let lab = options.lab;
    let status_item = (!lab).then(|| NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength));
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    if let Some(item) = &status_item {
        item.setMenu(Some(&menu));
    }

    let render_dir = options.render_dir.clone();
    let checks_only = options.render_checks_only;
    let center = objc2_app_kit::NSWorkspace::sharedWorkspace().notificationCenter();
    let callback = block2::RcBlock::new(|_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
        DispatchQueue::main().exec_async(|| with_state(|s| { if !s.options.fake_permissions { s.poll_permissions(); } }));
    });
    let observer = unsafe { center.addObserverForName_object_queue_usingBlock(
        Some(objc2_app_kit::NSWorkspaceDidActivateApplicationNotification), None, None, &callback) };
    watch_spaces();
    // The island's windows join a SkyLight overlay space when ordered in; it follows display changes and goes at quit.
    overlay_space::watch();
    // One callback, shared with the island's reports and drawing mode's Send.
    let on_action: std::rc::Rc<dyn Fn(UiAction)> = std::rc::Rc::from(on_action);
    let action = on_action.clone();
    let on_action_for_drawing = on_action.clone();
    let profile = options.system_profile.clone();
    let mut state = State::new(mtm, controller, options, Box::new(move |a| action(a)), status_item, menu, Some(observer));
    if !state.options.fake_permissions {
        for (p,s) in permissions::status() { state.set_permission(p,s); }
        state.set_permission(Permission::SystemAudio, permissions::system_audio());
    }
    state.name_entry.hidden = state.options.render_dir.is_some();
    state.friend_invite.hidden = state.options.render_dir.is_some();
    state.update_status_button();
    state.rebuild_menu();
    STATE.with(|cell| *cell.borrow_mut() = Some(state));
    // The island (one object: lines, the turn, the voice, questions, notes, the reader, drawing mode's keys and
    // light, pointing, health) exists from launch, its windows ordered out, so the screen capture filters can
    // exclude them. Its actions reach the app through `on_action`; a problem's fix button opens Settings or the
    // menu here first.
    island::install(mtm);
    // The drop target at the notch: ordered out until a drag starts anywhere (not in renders; a lab instance leaves
    // drags to the user's own Heimdall).
    drop_target::install(mtm, render_dir.is_none() && !lab);
    if lab {
        lab::shareable();
    }
    island::set_sink(Box::new(move |a| on_action(a)));
    island::set_health_handler(Box::new(|key, control| {
        with_state(move |s| s.health_clicked(key, control));
    }));
    // Drawing mode and pointing: windows ordered out from launch (the capture filters exclude them), pictures
    // rendered ahead, the system profile read (measured on a build's first launch).
    let sent = on_action_for_drawing.clone();
    draw::install(profile.as_deref(), std::rc::Rc::new(move |shot| sent(UiAction::DrawingSent(shot))), mtm);
    // ⌃⌘. ⌃⌘/ ⌃⌘M ⌃⌘,, the user's own Heimdall's: never in offscreen renders or a lab instance.
    if render_dir.is_none() && !lab {
        shortcuts::install(mtm);
    }

    if let Some(dir) = render_dir {
        render::start(dir, checks_only);
    }
    app.run();
    std::process::exit(0)
}

impl State {
    /// Everything at its launch value: the app's updates fill it in.
    pub(crate) fn new(
        mtm: MainThreadMarker,
        controller: Retained<Controller>,
        options: Options,
        on_action: Box<dyn Fn(UiAction)>,
        status_item: Option<Retained<NSStatusItem>>,
        menu: Retained<NSMenu>,
        _activation_observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    ) -> Self {
        State {
            mtm,
            controller,
            options,
            on_action,
            status_item,
            menu,
            menu_open: false,
            menu_stale: false,
            background_menu: None,
            microphone_buttons: Vec::new(),
            microphones: Microphones::default(),
            app_buttons: Vec::new(),
            app_audio: AppAudio::default(),
            screen_buttons: Vec::new(),
            tile_views: Vec::new(),
            engine: EngineStatus::default(),
            engine_item: None,
            engine_buttons: Vec::new(),
            engine_facts: Vec::new(),
            engine_thinking: None,
            engine_rows: Default::default(),
            decider: Default::default(),
            decider_pending: None,
            decider_rows: Default::default(),
            sections: Default::default(),
            status_line: None,
            session_item: None,
            login_row: None,
            wake_sound_row: None,
            background_task_rows: Vec::new(),
            background_paused: None,
            relay: crate::model::RelayIndicator::Off,
            relay_row: None,
            pending_choice: None,
            relay_models: Vec::new(),
            t3_instances: Vec::new(),
            background_routes: Vec::new(),
            relay_model_rows: Vec::new(),
            relay_accounts: Vec::new(),
            relay_account_rows: Vec::new(),
            relay_detail: (None, Vec::new()),
            confirm_row: None,
            islands_shown_row: None,
            switches_told: None,
            listen_known: false,
            shortcut_items: Vec::new(),
            islands_on_streams_row: None,
            speed_row: None,
            voice_sample_note: None,
            speech_voice: Default::default(),
            speaking_rows: Default::default(),
            personalise_rows: Default::default(),
            listen_pending: None,
            render_light: false,
            listen: ListenState::Listening,
            off_since: None,
            component_status: Default::default(),
            sources: Vec::new(),
            agent: AgentState { harness: Harness::Pi, session: SessionState::Starting },
            permissions: Permission::ALL.iter().map(|p| (*p, PermissionState::NotDetermined)).collect(),
            speakers_list: Vec::new(),
            settings_value: Settings::default(),
            runtime_settings: RuntimeSettings::default(),
            api: ApiProviders::default(),
            api_rows: Default::default(),
            api_expanded: Default::default(),
            add_provider_choice: Default::default(),
            sections_sent: Default::default(),
            add_provider_rows: Default::default(),
            agent_models_requested: false,
            background_rows: Default::default(),
            task_pickers: Vec::new(),
            background_caption: None,
            api_item: None,
            key_panel: None,
            agent_session: AgentSession { harness: Harness::Pi, model: None, session_id: None, agent_on: false, can_open: false },
            agent_view: Default::default(),
            agent_window: None,
            agent_autosave: true,
            transcript: None,
            setup: None,
            setup_home: HomeState::default(),
            setup_t3: T3Setup::default(),
            setup_vella: VellaSetup::default(),
            setup_harnesses: HarnessSetup::default(),
            update_row: None,
            speakers: None,
            bug_notes: Vec::new(),
            memory_demo: None,
            bug_window: None,
            name_entry: speaker::NameEntry::default(),
            friends: Vec::new(),
            friend_menu_ids: Vec::new(),
            friend_invite: friend_invite::InvitePanel::default(),
            setup_timer: None,
            _activation_observer,
        }
    }

    pub(crate) fn emit(&self, action: UiAction) {
        (self.on_action)(action);
    }

    /// The menu's Quit: the app shuts down and answers with Terminate.
    fn quit(&self) {
        self.emit(UiAction::Quit);
        // A real client must drain audio and durable deliveries before Terminate.
        // Keep the fallback only for the looks-only demo, which has no components.
        if self.options.fake_permissions { unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                2.0, &self.controller, sel!(quitFallback:), None, false,
            );
        }}
    }

    /// An updater notice as a native alert, shown non-modally from a run-loop timer (`showUpdateAlert:`), so the menu
    /// and Quit keep working while it is up (VM finding P-01, 28 Oct). An offer's Update installs.
    fn update_alert(&self, alert: UpdateAlert) {
        if !self.live() { return; }
        PENDING_ALERTS.with(|q| q.borrow_mut().push_back(alert));
        unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.0, &self.controller, sel!(showUpdateAlert:), None, false,
            );
        }
    }

    fn apply(&mut self, update: UiUpdate) {
        // Setup shows the providers' sign-in, the engine, the identity and the voice: it follows them after each.
        let setup_follows = matches!(update, UiUpdate::ApiProviders(_) | UiUpdate::Engine { .. } | UiUpdate::Settings(_) | UiUpdate::Identity(_) | UiUpdate::SpeechVoice(_) | UiUpdate::RuntimeSettings(_));
        self.apply_update(update);
        if setup_follows { self.sync_setup(); }
    }

    /// Setup's copies of what the menu knows: each provider tile's state, the identity, the voice.
    fn sync_setup(&mut self) {
        let Some(w) = self.setup.as_mut() else { return };
        let api = model_choice::drawn(&self.api, &self.settings_value, None);
        w.set_providers(ProviderChoice::ALL.iter().map(|c| (*c, setup::provider_status(*c, &api, &self.engine))).collect());
        let identity = crate::identity::current();
        if w.identity != identity { w.set_identity(&identity); }
        if w.voice != self.speech_voice { w.set_voice(&self.speech_voice); }
        w.set_setup_requested(self.runtime_settings.setup_requested);
    }

    fn apply_update(&mut self, update: UiUpdate) {
        match update {
            UiUpdate::Engine { state, model_id, model_label, models, port, context_limit, cache_used_bytes, cache_limit_bytes, memory_limit_bytes } => {
                let engine = EngineStatus { state, model_id, model_label, models, port, context_limit, cache_used_bytes, cache_limit_bytes, memory_limit_bytes };
                if self.engine == engine { return; }
                // The engine running or stopping moves it between Add Provider and Providers in Agent ▸ Model (when it is
                // not added): that picker is laid again whole. The Engine section lays its rows again when a precision
                // arrived, left, or started or ended a download (`engine_section::relay`); anything else syncs in place.
                let in_use = |e: &EngineStatus| provider_rows::engine_in_use(&model_choice::drawn(&self.api, &self.settings_value, None), e);
                let moved = in_use(&self.engine) != in_use(&engine);
                // This Mac's row shows the loaded model's context stops and thinking: another model (or its window
                // reported) lays those rows again.
                let held = |e: &EngineStatus| (menu::loaded_label(e), e.model_id.clone(), e.context_limit);
                let rows = held(&self.engine) != held(&engine);
                self.engine = engine;
                if !self.menu_open && !menu_on_screen(self.mtm) {
                    self.menu_stale = true;
                    return;
                }
                engine_section::relay(self);
                if moved { menu::relay_whole_agent_models(self); } else if rows { menu::relay_live_agent_models(self); }
                menu::sync_tracking(self);
            }
            UiUpdate::AgentWindow(update) => self.apply_agent_window(update),
            UiUpdate::AgentNotice(notice) => {
                // The full text stays in the Agent window; a question or Connect T3 takes the island's card; a
                // plain line is the island's own (icon, words, pull-down).
                let change = self.agent_view.notice(&notice, now_ms());
                if let Some(w) = &mut self.agent_window { w.apply(&change, &self.agent_view); }
                island::notice(notice);
            }
            UiUpdate::ClearAgentNotices => island::apply(Event::ClearNotices),
            UiUpdate::DismissAgentNotice(id) => island::apply(Event::NoticeClear(id)),
            UiUpdate::CloseQuestion { id, answer } => island::apply(Event::CloseQuestion { id, answer }),
            UiUpdate::AskSet { id, answer } => island::apply(Event::AskSet { id, answer }),
            UiUpdate::AgentSent => island::apply(Event::Sent),
            // The notch's emergency token and the menu's Background Tasks pause (docs/claude-relay.md).
            UiUpdate::AgentRelay(relay) => {
                if self.relay == relay { return; }
                let was = self.relay.emergency();
                self.relay = relay;
                if was != self.relay.emergency() { island::apply(Event::Emergency(self.relay.emergency())); }
                self.refresh_menu();
            }
            UiUpdate::RelayModels(models) => {
                if self.relay_models == models { return; }
                self.relay_models = models;
                self.rebuild_agent_models();
            }
            UiUpdate::T3Instances(instances) => {
                if self.t3_instances == instances { return; }
                self.t3_instances = instances;
                // Background Tasks ▸ Model lists them: an open picker lays its rows again in place.
                self.refresh_menu();
            }
            UiUpdate::BackgroundRoutes(routes) => {
                if self.background_routes == routes { return; }
                self.background_routes = routes;
                self.refresh_menu();
            }
            UiUpdate::RelayAccounts(accounts) => {
                if self.relay_accounts == accounts { return; }
                self.relay_accounts = accounts;
                self.rebuild_agent_models();
            }
            UiUpdate::IslandHold(ms) => island::apply(Event::Hold(ms.map_or(f64::INFINITY, |ms| f64::from(ms) / 1000.))),
            // `false` is the audio actually ending (the speaking look ends with it).
            UiUpdate::HeimdallSpeaking(on) => island::apply(Event::Speaking(on)),
            UiUpdate::HeimdallSpeechCut => island::apply(Event::SpeechCut),
            UiUpdate::SpokenText(text) => island::apply(Event::SpokenText(text)),
            UiUpdate::LastReply(text) => island::apply(Event::LastReply(text)),
            UiUpdate::WithdrawReply(text) => island::apply(Event::WithdrawReply(text)),
            UiUpdate::Chat(history) => island::apply(Event::Chat(history)),
            UiUpdate::FloatingNudge { question } => island::apply(Event::Nudge { question }),
            UiUpdate::Vella { on, keys } => island::apply(Event::Vella { on, keys }),
            UiUpdate::SpeechLevel(level) => island::apply(Event::SpeechLevel(level as f32 / 1000.)),
            UiUpdate::VoiceSample(note) => {
                if let Some(row) = &self.speed_row { row.set_note(note.as_deref()); }
                self.voice_sample_note = note;
            }
            // [speech] night 1022 menu.
            UiUpdate::SpeechVoice(voice) => {
                if self.speech_voice == voice { return; }
                self.speech_voice = voice;
                speaking_menu::sync(self);
                // A folded Personalisation header names the voice.
                menu::apply_sections(self);
            }
            UiUpdate::Decider(decider) => {
                if self.decider == decider { return; }
                // Another set of precisions (a new update's) lays the menu again; anything else syncs in place.
                let keys: Vec<String> = decider.options.iter().map(|o| o.key.clone()).collect();
                let relaid = keys != self.decider_rows.keys;
                // A pick waiting for Confirm that is now what runs, or no longer a pick, goes.
                if let Some(p) = self.decider_pending.clone() {
                    if decider_section::click(&decider, &p).is_none() { self.decider_pending = None; }
                }
                if let Some(w) = self.setup.as_mut() { w.set_decider(&decider); }
                self.decider = decider;
                if relaid { self.menu_stale = true; if !self.menu_open { return; } }
                self.refresh_decider();
            }
            UiUpdate::Identity(identity) => {
                if crate::identity::current() == identity { return; }
                crate::identity::set(identity);
                self.update_status_button();
                self.refresh_menu();
            }
            UiUpdate::Listen(listen) => {
                if self.listen_pending.take().is_some() { self.refresh_menu(); }
                self.listen_known = true;
                if self.listen == listen { self.sync_switches(None); return; }
                self.off_since = (listen == ListenState::Paused).then(clock_now);
                if listen != ListenState::Listening { island::apply(Event::ListeningOff); }
                self.listen = listen;
                self.sync_switches(None);
                self.update_status_button();
                self.refresh_menu();
            }
            UiUpdate::ComponentStatus { component, message } => {
                if self.component_status.get(&component) == message.as_ref() { return; }
                if let Some(message) = message { self.component_status.insert(component, message); }
                else { self.component_status.remove(&component); }
                self.refresh_menu();
            }
            UiUpdate::Sources(sources) => {
                if self.sources == sources { return; }
                self.sources = sources;
                self.update_status_button();
                self.refresh_menu();
            }
            UiUpdate::Agent(agent) => {
                if self.agent == agent { return; }
                self.agent = agent;
                self.refresh_menu();
            }
            UiUpdate::TranscriptRuns {id,lines} => {self.transcript_window().upsert_runs(id,&lines);}
            UiUpdate::Transcript(line) => {
                let window = self.transcript_window();
                window.upsert(&line);
            }
            UiUpdate::VoiceNameEntry { code, quote } => {
                let (mtm, target) = (self.mtm, self.target());
                self.name_entry.show(code, &quote, &target, mtm);
            }
            // The Friends submenu reads this list when the menu opens; nothing to redraw while it is shut.
            UiUpdate::Friends(rows) => {
                self.friends = rows;
                // An open menu shows the change (a Revoke keeps the menu open): Friends ▸ refilled in place.
                if self.menu_open || menu_on_screen(self.mtm) {
                    let item = self.menu.itemArray().iter().find(|i| i.title().to_string() == "Friends" && i.hasSubmenu());
                    if let Some(item) = item {
                        let target = self.target();
                        let (fresh, ids) = menu::friends_menu(&self.friends, Some(&*target), self.mtm);
                        menu::refill(&item, &fresh);
                        self.friend_menu_ids = ids;
                    }
                }
            }
            UiUpdate::FriendName(suggested) => {
                let (mtm, target) = (self.mtm, self.target());
                self.name_entry.show_friend(&suggested, &target, mtm);
            }
            UiUpdate::FriendReply { card_id, question } => {
                let (mtm, target) = (self.mtm, self.target());
                self.name_entry.show_reply(&card_id, &question, &target, mtm);
            }
            UiUpdate::FriendInvite(Some(invite)) => {
                let (mtm, target) = (self.mtm, self.target());
                if !self.friend_invite.show(&invite.label, &invite.png, &target, mtm) {
                    eprintln!("friends: the invite's QR code could not be drawn");
                }
            }
            UiUpdate::FriendInvite(None) => self.friend_invite.close(),
            UiUpdate::Speakers(list) => {
                // A call sends the same list on every diarization frame: rebuild only a change on screen (opening rebuilds).
                let visible = self.speakers.as_ref().is_some_and(|w| w.window.isVisible());
                let rebuild = speakers::rebuilds(&self.speakers_list, &list, visible);
                self.speakers_list = list;
                if let (true, Some(w)) = (rebuild, &mut self.speakers) {
                    w.show(&self.speakers_list, &self.controller, self.mtm);
                }
            }
            UiUpdate::Permissions(states) => {
                let mut changed = false;
                for (permission, state) in states {
                    changed |= self.set_permission(permission, state);
                }
                if changed { self.after_permissions_changed(); }
            }
            UiUpdate::Settings(mut settings) => {
                // At most one big section open (`menu::one_open`), whatever was saved.
                settings.collapsed = menu::one_open(&settings.collapsed);
                // A header press not yet answered: the menu's own list holds (an echo of an earlier press, or of
                // another setting saved meanwhile, carries the list from before it).
                settings.collapsed = menu::echoed_sections(&mut self.sections_sent, settings.collapsed, &self.settings_value.collapsed, std::time::Instant::now());
                if self.settings_value == settings { return; }
                streams::set(settings.islands_on_streams);
                // The relay added or removed (or on or off) moves it between Add Provider and the picker: Agent ▸ Model is
                // laid again whole. A section opened elsewhere shows its rows again.
                let relay_moved = (self.settings_value.agent_relay, self.settings_value.relay_added) != (settings.agent_relay, settings.relay_added);
                let opened: Vec<MenuSection> = self.settings_value.collapsed.iter().filter(|s| !settings.collapsed.contains(s)).copied().collect();
                self.settings_value = settings;
                for section in opened { self.show_section_rows(section); }
                self.refresh_menu();
                if relay_moved { self.rebuild_whole_agent_models(); }
            }
            UiUpdate::ApiProviders(api) => {
                if self.api == api { return; }
                // A provider connected or disconnected moves between Providers and Add Provider: the open Agent ▸
                // Model is rebuilt whole (Disconnect keeps the menu open); any other change syncs its rows.
                let connected = |a: &crate::ApiProviders| a.providers.iter().map(|p| (p.id.clone(), p.auth == ProviderAuth::Missing)).collect::<Vec<_>>();
                let moved = connected(&self.api) != connected(&api);
                self.api = api;
                if moved { self.rebuild_whole_agent_models(); } else { self.refresh_menu(); }
            }
            UiUpdate::ProviderLogin { provider, code, url } => {
                let Some(panel) = self.key_panel.as_mut().filter(|p| p.provider == provider && p.mode == provider_key::Mode::SignIn) else { return };
                panel.show_code(&code, &url);
                self.open_url(&url);
            }
            UiUpdate::ProviderConnected { provider, result } => {
                let Some(panel) = self.key_panel.as_mut().filter(|p| p.provider == provider) else { return };
                match result {
                    Ok(()) => panel.panel.close(),
                    Err(reason) => {
                        panel.set_busy(false);
                        panel.set_status(Some((&reason, true)));
                        // A spent code: the browser button restarts the sign-in instead of opening a dead link.
                        if panel.mode == provider_key::Mode::SignIn { panel.sign_in_failed(); }
                    }
                }
            }
            UiUpdate::RuntimeSettings(settings) => {
                if self.runtime_settings == settings { return; }
                self.runtime_settings=settings; self.sync_switches(None); self.refresh_menu();
            },
            UiUpdate::Microphones(microphones) => {
                if self.microphones == microphones { return; }
                self.microphones = microphones;
                self.refresh_menu();
            }
            UiUpdate::AppAudio(apps) => {
                if self.app_audio == apps { return; }
                self.app_audio = apps;
                self.refresh_menu();
            }
            UiUpdate::AgentSession(session) => {
                if self.agent_session == session { return; }
                self.agent_session = session;
                if let Some(w) = &mut self.agent_window { w.set_session(&self.agent_session); }
            }
            // The original viewer's feed. The window draws the live stream (`AgentWindow`), which carries the same
            // entries with stable ids; drawing both would show every finalized entry twice.
            UiUpdate::AgentHistory(_) | UiUpdate::AgentEntries(_) => {}
            UiUpdate::BugNotes(notes) => {
                if self.bug_notes == notes { return; }
                self.bug_notes = notes;
                if let Some(w) = &self.bug_window { w.show(&self.bug_notes); }
                self.refresh_menu();
            }
            // Listening off shows nothing.
            UiUpdate::Command(cue) => if self.listen == ListenState::Listening { island::apply(Event::Command(cue)) } else { island::not_taken(&cue, "listening off") },
            // Health shows whatever Listening is; a held problem turns the helmet red.
            UiUpdate::Health(cue) => {
                island::apply(Event::Health(cue.named()));
                self.update_status_button();
            }
            UiUpdate::ShowAgent => self.show_agent(),
            UiUpdate::ShowTranscript => self.show_transcript(),
            UiUpdate::ShowSetup => self.show_setup(),
            UiUpdate::ShowSetupAt(step) => self.show_setup_at(Some(step)),
            UiUpdate::Home(home) => {
                if let Some(w) = self.setup.as_mut() { w.set_home(&home); }
                self.setup_home = home;
            }
            UiUpdate::T3Setup(t3) => {
                if let Some(w) = self.setup.as_mut() { w.set_t3(&t3); }
                self.setup_t3 = t3;
            }
            UiUpdate::VellaSetup(vella) => {
                if let Some(w) = self.setup.as_mut() { w.set_vella(&vella); }
                self.setup_vella = vella;
            }
            UiUpdate::HarnessSetup(harnesses) => {
                if let Some(w) = self.setup.as_mut() { w.set_harnesses(&harnesses); }
                self.setup_harnesses = harnesses;
            }
            UiUpdate::Terminate => terminate(self.mtm),
            UiUpdate::Quit => self.quit(),
            UiUpdate::UpdateRow(row) => {
                if self.update_row == row { return; }
                self.update_row = row;
                self.refresh_menu();
            }
            UiUpdate::UpdateAlert(alert) => self.update_alert(alert),
            UiUpdate::FastRead(update) => island::apply(Event::Reader(update)),
        }
    }

    /// A tile click: the opposite of what it shows, through the same actions the menu rows used. Agent emits
    /// before anything else, so the app's callback closes delivery synchronously (the stop latch).
    fn toggle_tile(&mut self, tile: Tile) {
        // Every tile always switches (`tile_available`); the guard stays for a future unavailable state.
        if !self.tile_available(tile) {
            return;
        }
        let on = self.tile_on(tile);
        match tile {
            // On shows Starting at once (loading takes a while); Off drops the figures the engine reported.
            Tile::Engine => {
                self.emit(tile.toggle(on));
                // On with a model it can load: starting; with none picked on this Mac: waiting for a pick at once.
                let pick = !self.engine.model_id.is_empty() && engine_section::picks(&self.engine).iter().any(|p| p.loaded && p.fits);
                self.engine.state = if on { EngineState::Off } else if pick { EngineState::Starting } else { EngineState::Waiting };
                self.engine.context_limit = None;
                self.engine.cache_used_bytes = None;
                self.engine.cache_limit_bytes = None;
            }
            Tile::Agent => {
                self.emit(tile.toggle(on));
                self.runtime_settings.dry_run = on;
            }
            Tile::Listening => {
                // The clicked state shows until the app's next Listen, even an unchanged one.
                updates().lock().unwrap().expect_listen();
                self.emit(tile.toggle(on));
                self.listen_pending = Some(!on);
            }
            Tile::Seeing => {
                self.runtime_settings.seeing = !on;
                self.emit(tile.toggle(on));
            }
            Tile::Speaking => {
                self.settings_value.speak_replies = !on;
                self.emit(tile.toggle(on));
            }
        }
        match tile {
            Tile::Listening => self.sync_switches(Some(Switch::Listening)),
            Tile::Seeing => self.sync_switches(Some(Switch::Seeing)),
            _ => {}
        }
        self.refresh_menu();
    }

    /// Listening and Seeing as the island's token holds them (24 Oct: the user sees Heimdall is deaf or blind); `moment`:
    /// the one the user just turned, which shows its glyph turning. Nothing is told before the app's first Listen.
    fn sync_switches(&mut self, moment: Option<Switch>) {
        if self.switches_told.is_none() && moment.is_none() && !self.listen_known {
            return;
        }
        let now = (self.tile_on(Tile::Listening), self.tile_on(Tile::Seeing));
        let was = self.switches_told;
        for (switch, on, before) in [(Switch::Listening, now.0, was.map(|w| w.0)), (Switch::Seeing, now.1, was.map(|w| w.1))] {
            let m = moment == Some(switch);
            if m || before != Some(on) {
                island::apply(Event::Switch { switch, on, moment: m });
            }
        }
        self.switches_told = Some(now);
    }

    /// Whether a tile can be switched now: always. The five switches are independent, each turning off only
    /// itself, so Listening off leaves Seeing, Speaking and Agent switchable.
    fn tile_available(&self, _tile: Tile) -> bool {
        true
    }

    /// Whether a tile counts as on: its setting, or a Listening click the app has not confirmed yet.
    pub(crate) fn tile_on(&self, tile: Tile) -> bool {
        match (tile, self.listen_pending) {
            (Tile::Listening, Some(pending)) => pending,
            _ => tile.is_on(&self.listen, &self.runtime_settings, &self.settings_value, &self.engine.state),
        }
    }

    /// What a tile shows: on or off, and for the engine also Starting or a problem.
    fn tile_look(&self, tile: Tile) -> TileLook {
        match tile {
            Tile::Engine => self.engine.state.look(),
            // Emergency mode (the agent failed, cover not yet answering): the Agent tile says so, as its row does.
            Tile::Agent if self.tile_on(tile) && menu::finding_cover(self) => TileLook::Problem,
            _ if self.tile_on(tile) => TileLook::On,
            _ => TileLook::Off,
        }
    }

    /// A section's header: folded to it, or open again, in place; the choice goes to the app, which keeps it.
    pub(crate) fn toggle_section(&mut self, section: MenuSection) {
        let folded = !self.settings_value.collapsed.contains(&section);
        // The rows about to fold, while they still show: the menu on screen moves its rows with its window
        // (`menu_motion::will_change`).
        let next = menu::folded_after_click(section, folded);
        menu::announce_folds(self, &next);
        // One big section open at a time (UX climb, 30 Oct). The whole folded list goes in one action, saved and echoed
        // once: sent a fold at a time, the app echoed the list between them (every section folded) and the open menu
        // folded, then grew again, on every press that opened a section (31 Oct, recorded live).
        self.settings_value.collapsed = next;
        self.sections_sent.push_back((std::time::Instant::now(), self.settings_value.collapsed.clone()));
        self.emit(UiAction::SetSectionsCollapsed(self.settings_value.collapsed.clone()));
        // One relayout for the whole change, not one per row shown or hidden (climb 2, measured live: a header click
        // held the main thread 20-75 ms). MENU_UNBATCHED keeps AppKit's per-row updates, for the before measurement.
        let batch = std::env::var_os("MENU_UNBATCHED").is_none();
        #[allow(deprecated)]
        if batch { self.menu.setMenuChangedMessagesEnabled(false); }
        if !folded { self.show_section_rows(section); }
        // The syncs put back what an open section hides by itself (Agent off's caption, a paused task) and fold the rest.
        menu::sync_tracking(self);
        #[allow(deprecated)]
        if batch { self.menu.setMenuChangedMessagesEnabled(true); }
        // The open section's panel on the rows AppKit has now, and once more after it lays out the rows it added (the
        // syncs above repainted before the batch reached AppKit).
        if let Some(window) = self.sections.values().find_map(|(h, _)| h.window()) { menu_group::repaint_soon(&window); }
    }

    /// An opening section's rows all show; the next sync hides again the ones its own state hides.
    fn show_section_rows(&self, section: MenuSection) {
        if let Some((_, items)) = self.sections.get(&section) {
            for item in items { item.setHidden(false); }
        }
    }

    fn toggle_launch_at_login(&mut self) {
        self.settings_value.launch_at_login = !self.settings_value.launch_at_login;
        self.emit(UiAction::SetLaunchAtLogin(self.settings_value.launch_at_login));
        self.refresh_menu();
    }
    /// [menu lane] A picker's Add Provider header: its rows show or hide in place; the choice holds for this run.
    pub(crate) fn toggle_add_provider(&mut self, picker: menu::Picker) {
        let Some((row, items)) = self.add_provider_rows.get(&picker) else { return };
        let open = !row.is_on();
        row.set_on(open);
        // Its group labels stay hidden, as the pickers' (the user's rules: no eyebrow labels).
        for item in items { item.setHidden(!open || item.isSectionHeader()); }
        self.add_provider_choice.insert(picker, open);
    }
    /// Background Tasks ▸ a job's toggle.
    fn toggle_background_task(&mut self, task: crate::model::BackgroundTask) {
        let on = self.settings_value.background_off.contains(&task);
        if on { self.settings_value.background_off.retain(|t| *t != task); } else { self.settings_value.background_off.push(task); }
        self.emit(UiAction::SetBackgroundTask { task, on });
        self.refresh_menu();
    }

    /// Speaking ▸ Wake Sound (speech-policy, night 3).
    fn toggle_wake_sound(&mut self) {
        self.settings_value.wake_sound = !self.settings_value.wake_sound;
        self.emit(UiAction::SetWakeSound(self.settings_value.wake_sound));
        self.refresh_menu();
    }

    /// Show Islands and ⌃⌘,: every floating island hidden or shown at once.
    pub(crate) fn toggle_islands_shown(&mut self) {
        self.settings_value.islands_shown = !self.settings_value.islands_shown;
        self.emit(UiAction::SetIslandsShown(self.settings_value.islands_shown));
        island::apply(Event::Switch { switch: Switch::Islands, on: self.settings_value.islands_shown, moment: true });
        self.refresh_menu();
    }

    /// Show Islands on Streams: applied to the island's windows at once, then saved by the app.
    pub(crate) fn toggle_islands_on_streams(&mut self) {
        self.settings_value.islands_on_streams = !self.settings_value.islands_on_streams;
        streams::set(self.settings_value.islands_on_streams);
        self.emit(UiAction::SetIslandsOnStreams(self.settings_value.islands_on_streams));
        self.refresh_menu();
    }

    fn target(&self) -> Retained<AnyObject> {
        Retained::into_super(Retained::into_super(self.controller.clone()))
    }

    fn set_permission(&mut self, permission: Permission, state: PermissionState) -> bool {
        if let Some(entry) = self.permissions.iter_mut().find(|(p, _)| *p == permission) {
            if entry.1 == state { return false; }
            entry.1 = state;
            return true;
        }
        false
    }

    pub(crate) fn permission(&self, permission: Permission) -> PermissionState {
        self.permissions.iter().find(|(p, _)| *p == permission).map(|e| e.1).unwrap_or(PermissionState::NotDetermined)
    }

    fn all_granted(&self) -> bool {
        self.permissions.iter().all(|(_, s)| s.satisfied())
    }

    fn after_permissions_changed(&mut self) {
        if let Some(w) = self.setup.as_mut() {
            w.show(&self.permissions);
        }
        self.refresh_menu();
    }

    fn update_status_button(&self) {
        let Some(button) = self.status_item.as_ref().and_then(|i| i.button(self.mtm)) else { return };
        // A paused or blocked Heimdall, or one with every source off, crosses out its helmet at full contrast, the way
        // the system shows Wi-Fi or a microphone off: greying alone was too faint to read at a glance.
        let hearing = matches!(self.listen, ListenState::Listening) && self.sources.iter().any(|s| s.enabled);
        // Something Heimdall cannot fix by itself: the helmet in bright red until it is fixed.
        let alerting = island::alerting();
        let image = if alerting { helmet::menu_bar_attention_image() } else { helmet::menu_bar_image(!hearing) };
        button.setImage(Some(&image));
        button.setToolTip(Some(&util::ns(&status_tooltip(&crate::identity::name(), &self.listen, alerting))));
    }

    /// The island's red pill was clicked (it already collapsed to its glyph). Settings and the menu open here; crates/app
    /// hears every click and acts on relaunch, login and the archive.
    pub(crate) fn health_clicked(&mut self, key: String, control: crate::island::HealthControl) {
        use crate::island::HealthControl;
        match control {
            HealthControl::OpenSettings(permission) if !self.options.fake_permissions => permissions::open_system_settings(permission),
            HealthControl::OpenAgent => self.show_agent(),
            HealthControl::OpenMenu => {
                // The menu is rebuilt before it opens: a state change made while it was closed was not synced, and
                // menuNeedsUpdate: cannot take the state while this call holds it.
                self.rebuild_menu();
                if let Some(button) = self.status_item.as_ref().and_then(|i| i.button(self.mtm)) {
                    // The status item's own click, after the state is released: the menu opens under the helmet, as when
                    // the user clicks it.
                    let button = MainOnly(button);
                    DispatchQueue::main().exec_async(move || {
                        let button = button;
                        unsafe { button.0.performClick(None) };
                    });
                }
            }
            _ => {}
        }
        self.emit(UiAction::Health { key, control });
        self.update_status_button();
    }

    /// A model row clicked in Agent ▸ Model (`model_choice`): another row than the agent's choice becomes the
    /// pending one, its details open and Confirm glows under them; the choice itself only opens or closes its
    /// details. `key` names the row's details in `api_expanded`.
    fn choose(&mut self, clicked: model_choice::Choice, key: (String, String)) {
        let now = match clicked {
            model_choice::Choice::RelayAccount(_) => model_choice::current_account(&self.settings_value),
            _ => model_choice::current(&self.api, &self.settings_value),
        };
        let relay_default = self.relay_models.first().map(|m| m.id.clone());
        let pending = model_choice::click(now.as_ref(), clicked, relay_default.as_deref());
        if pending.is_some() {
            // Only the proposed row stands open: the rows an earlier click opened fold again.
            self.api_expanded.clear();
            self.api_expanded.insert(key);
        } else if self.pending_choice.is_none() && !self.api_expanded.remove(&key) {
            self.api_expanded.insert(key);
        }
        self.pending_choice = pending;
        self.rebuild_agent_models();
    }

    /// A press on precision row `index` of Engine ▸ Add Model (`engine_rows`).
    pub(crate) fn precision(&mut self, index: usize, press: Press) {
        let Some(p) = self.engine_rows.precisions.get(index) else { return };
        let (id, quant) = (p.id.clone(), p.quant.clone());
        match press {
            Press::Download => self.emit(UiAction::DownloadEngineModel { id, quant, then_select: false }),
            Press::Stop => self.emit(UiAction::CancelEngineDownload { id }),
            Press::Trash => {
                // Asks once: one trash armed at a time, shown as the red Delete pill.
                self.api_expanded.retain(|(k, _)| k != menu::TRASH_ARMED);
                self.api_expanded.insert((menu::TRASH_ARMED.to_string(), format!("{id}:{quant}")));
                self.relay_engine_section();
            }
            Press::Delete => {
                self.api_expanded.retain(|(k, _)| k != menu::TRASH_ARMED);
                self.emit(UiAction::DeleteEngineModel { id, quant });
                self.relay_engine_section();
            }
        }
    }

    /// A quick-pick row of the Engine section: that precision loads (the engine turns on); the agent keeps its model.
    /// The row the user clicked shows as the engine's at once, on its way; the app's update follows.
    pub(crate) fn load_pick(&mut self, index: usize) {
        let Some(p) = self.engine_rows.picks.get(index) else { return };
        let e = p.entry.clone();
        if e.kind != engine_section::PickKind::OnDisk || !e.fits { return; }
        if e.loaded && self.engine.state.holds_model() { self.refresh_menu(); return; }
        self.emit(UiAction::LoadEngineModel { id: e.id.clone(), quant: e.quant.clone() });
        if let Some(m) = self.engine.models.iter_mut().find(|m| m.id == e.id) { m.quant = e.quant.clone(); }
        self.engine.model_id = e.id.clone();
        self.engine.model_label = e.label.clone();
        self.engine.state = EngineState::Starting;
        self.engine.context_limit = None;
        self.refresh_menu();
    }

    /// The Engine section's rows laid again in place (an armed trash, a delete), then synced.
    fn relay_engine_section(&mut self) {
        if !self.menu_open && !menu_on_screen(self.mtm) {
            self.menu_stale = true;
            return;
        }
        engine_section::relay(self);
        menu::sync_tracking(self);
    }

    /// Confirm: the pending choice goes to the app and the menu shows it as the agent's at once.
    pub(crate) fn confirm_choice(&mut self) {
        let Some(choice) = self.pending_choice.take() else { return };
        let relay_was = self.settings_value.agent_relay;
        match &choice {
            model_choice::Choice::Local => {
                self.api.selected = None;
                self.api.unchosen = false;
                self.settings_value.agent_relay = false;
            }
            model_choice::Choice::Api { provider, id } => {
                self.api.selected = Some((provider.clone(), id.clone()));
                self.settings_value.agent_relay = false;
            }
            model_choice::Choice::Relay(model) => {
                self.settings_value.agent_relay = true;
                if model.is_some() { self.settings_value.agent_relay_model = model.clone(); }
            }
            model_choice::Choice::RelayAccount(account) => {
                self.settings_value.agent_relay = true;
                self.settings_value.agent_relay_account = account.clone();
            }
        }
        for action in model_choice::actions(&choice) { self.emit(action); }
        // The relay becoming the agent (or ending) greys or frees its Remove under Providers: the picker whole.
        if relay_was != self.settings_value.agent_relay { self.rebuild_whole_agent_models(); } else { self.rebuild_agent_models(); }
    }

    /// Agent ▸ Model's model rows laid again in place (a pending choice, its Confirm row, the relay's models), the
    /// providers and pickers below them kept.
    /// `rebuild_agent_models` for a change to the providers themselves: every section of the open Agent ▸ Model.
    fn rebuild_whole_agent_models(&mut self) {
        if !self.menu_open && !menu_on_screen(self.mtm) {
            self.menu_stale = true;
            return;
        }
        menu::relay_whole_agent_models(self);
        menu::sync_tracking(self);
    }

    fn rebuild_agent_models(&mut self) {
        if !self.menu_open && !menu_on_screen(self.mtm) {
            self.menu_stale = true;
            return;
        }
        menu::relay_live_agent_models(self);
        menu::sync_tracking(self);
    }

    /// Vella rebuilds the menu when it opens (`menuNeedsUpdate:`); a state change only syncs its rows in place. A rebuild costs 15–40 ms on the main thread (its controls size themselves through SwiftUI), the
    /// in-place sync about 0.2 ms, and agent state changes arrive exactly when the island starts moving
    /// (lab/postinstall, 19 Oct). While the menu is closed nothing is synced (night 1027): the next open rebuilds
    /// it from the state anyway.
    fn refresh_menu(&mut self) {
        // A menu on screen counts as open even if `menuWillOpen:` could not take the state to say so.
        if !self.menu_open && !menu_on_screen(self.mtm) {
            self.menu_stale = true;
            return;
        }
        menu::sync_tracking(self);
    }

    /// Brings a window forward and activates Heimdall (the user opened it from the menu). Render mode only orders it in,
    /// so rendering never takes focus from the app the user is using.
    fn present(&self, window: &NSWindow) {
        if self.options.render_dir.is_some() {
            window.setAlphaValue(0.0);
            window.setIgnoresMouseEvents(true);
            window.orderFront(None);
            return;
        }
        window.makeKeyAndOrderFront(None);
        #[allow(deprecated)]
        NSApplication::sharedApplication(self.mtm).activateIgnoringOtherApps(true);
    }

    fn transcript_window(&mut self) -> &mut transcript::TranscriptWindow {
        let mtm = self.mtm;
        self.transcript.get_or_insert_with(|| transcript::TranscriptWindow::new(mtm))
    }

    fn apply_agent_window(&mut self, update: AgentWindowUpdate) {
        match update {
            AgentWindowUpdate::State(state) => {
                if self.agent_view.state.as_ref() == Some(&state) { return; }
                // The turn ended (or the agent stopped): a resting working glyph goes.
                island::apply(Event::Activity(crate::island::Activity::of(&state.state)));
                // The menu's Agent caption says "Stopping after compaction…" while Agent off waits (menu::sync_session).
                let stopping = |s: Option<&AgentWindowState>| s.is_some_and(|s| matches!(s.state, AgentRunState::Stopping { .. }));
                let menu = stopping(self.agent_view.state.as_ref()) || stopping(Some(&state));
                self.agent_view.state = Some(state);
                if let Some(w) = &mut self.agent_window { w.set_state(&self.agent_view); }
                if menu { self.refresh_menu(); }
            }
            AgentWindowUpdate::History(entries) => {
                let changes = self.agent_view.history(entries);
                if let Some(w) = &mut self.agent_window { w.apply_changes(&changes, &self.agent_view); }
            }
            AgentWindowUpdate::Entries(entries) => {
                for change in self.agent_view.entries(entries) {
                    if let Some(w) = &mut self.agent_window { w.apply(&change, &self.agent_view); }
                }
            }
            AgentWindowUpdate::Session(stats) => {
                self.agent_view.session = Some(stats);
                if let Some(w) = &mut self.agent_window { w.refresh_telemetry(&self.agent_view); }
            }
        }
    }

    /// The Agent window, made on first use with everything known so far: its first frame shows the current
    /// state and history.
    pub(crate) fn ensure_agent_window(&mut self) -> &mut agent::AgentWindow {
        if self.agent_window.is_none() {
            let mut w = agent::AgentWindow::new(self.mtm, self.agent_autosave);
            w.set_session(&self.agent_session);
            w.reset(&self.agent_view);
            agent::observe_resize(&w);
            self.agent_window = Some(w);
        }
        self.agent_window.as_mut().unwrap()
    }

    fn show_agent(&mut self) {
        let session = self.agent_session.clone();
        let w = self.ensure_agent_window();
        w.set_session(&session);
        let window = w.window.clone();
        self.present(&window);
    }

    pub(crate) fn agent_send(&mut self, steer: bool) {
        let Some(action) = self.agent_window.as_mut().and_then(|w| w.send(steer)) else { return };
        self.emit(action);
    }

    pub(crate) fn agent_stop(&mut self) {
        if self.agent_window.as_ref().is_some_and(|w| w.can_stop()) {
            self.emit(UiAction::AgentWindow(AgentWindowAction::Stop));
        }
    }

    pub(crate) fn agent_reload(&mut self) {
        if self.agent_window.as_ref().is_some_and(|w| w.can_reload()) {
            self.emit(UiAction::AgentWindow(AgentWindowAction::Reload {
                reason: format!("User reloaded the agent from the Agent window at {}", clock_now()),
                abort: false,
            }));
        }
    }

    /// The pending reload's Reload Now (Debug Agent window): stop the turn and reload at once.
    pub(crate) fn agent_reload_now(&mut self) {
        let pending = self.agent_view.state.as_ref().is_some_and(|s| s.can_reload && s.reload.is_some());
        if pending {
            self.emit(UiAction::AgentWindow(AgentWindowAction::Reload {
                reason: format!("User reloaded the agent now from the Debug Agent window at {}", clock_now()),
                abort: true,
            }));
        }
    }

    pub(crate) fn agent_edit(&mut self, key: &str) {
        let Some(w) = self.agent_window.as_mut() else { return };
        w.begin_edit(key, &self.agent_view);
    }

    pub(crate) fn agent_resolve(&mut self, completed: bool) {
        let Some(action) = self.agent_window.as_mut().and_then(|w| w.resolve(completed)) else { return };
        self.emit(action);
    }

    pub(crate) fn agent_image_loaded(&mut self, id: &str, bytes: Option<Vec<u8>>) {
        if let Some(w) = &mut self.agent_window { w.image_loaded(id, bytes, &self.agent_view); }
    }

    /// Real app only: the offscreen renders and checks never open a browser or read the clipboard.
    fn live(&self) -> bool {
        self.status_item.is_some() && self.options.render_dir.is_none()
    }

    fn open_url(&self, url: &str) {
        if !self.live() { return; }
        if let Some(url) = objc2_foundation::NSURL::URLWithString(&util::ns(url)) {
            objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url);
        }
    }

    /// The clipboard's text, read only for the key field as it opens and never offscreen.
    fn clipboard(&self) -> Option<String> {
        if !self.live() { return None; }
        objc2_app_kit::NSPasteboard::generalPasteboard().stringForType(unsafe { objc2_app_kit::NSPasteboardTypeString }).map(|s| s.to_string())
    }

    /// Connect: the panel in sign-in mode; the app starts the sign-in and answers with the code (`ProviderLogin`).
    pub(crate) fn show_sign_in_panel(&mut self, index: usize) {
        let Some(p) = self.api.providers.get(index).cloned() else { return };
        self.menu.cancelTracking();
        if self.key_panel.as_ref().is_none_or(|k| k.provider != p.id) {
            let target = self.target();
            self.key_panel = Some(provider_key::KeyPanel::new(&p.id, &p.name, p.custom_url.then_some(p.key_required), &target, self.mtm));
            self.watch_key_panel();
        }
        if p.custom_url {
            // [providers] An OpenAI-compatible server or the user's own engine: its URL (the saved one to change) and key.
            let clipboard = self.clipboard();
            let panel = self.key_panel.as_mut().unwrap();
            panel.reset(clipboard.as_deref());
            panel.set_url(p.endpoint.as_deref().unwrap_or(""));
            let window = panel.panel.clone();
            self.present(&window);
            return;
        }
        if !p.sign_in {
            // A key-only provider (Anthropic): its API-keys page in the browser, and the paste field, filled from
            // the clipboard only while this panel is open (now, and whenever the user comes back to it).
            let clipboard = self.clipboard();
            let panel = self.key_panel.as_mut().unwrap();
            panel.reset(clipboard.as_deref());
            let window = panel.panel.clone();
            self.present(&window);
            self.open_url(&p.console_url);
            return;
        }
        let panel = self.key_panel.as_mut().unwrap();
        // [providers] ChatGPT: the consent finishes in the browser by itself; there is no code and no key to paste.
        if !p.key_fallback { panel.browser_only(); }
        panel.start_sign_in();
        let window = panel.panel.clone();
        self.present(&window);
        self.emit(UiAction::StartProviderLogin { provider: p.id });
    }

    /// Coming back to the key panel (after copying the key in the browser) fills an empty field from the clipboard.
    fn watch_key_panel(&mut self) {
        let Some(panel) = self.key_panel.as_mut() else { return };
        let window = panel.panel.clone();
        let block = block2::RcBlock::new(move |_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
            with_state(|s| {
                let clipboard = s.clipboard();
                if let Some(panel) = s.key_panel.as_mut().filter(|p| p.mode == provider_key::Mode::Key && p.key().trim().is_empty()) {
                    panel.fill_from(clipboard.as_deref());
                }
            });
        });
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        let observer = unsafe { center.addObserverForName_object_queue_usingBlock(Some(objc2_app_kit::NSWindowDidBecomeKeyNotification), Some(&window), None, &block) };
        panel.observer = Some(observer);
    }

    fn show_bug_notes(&mut self) {
        let mtm = self.mtm;
        let w = self.bug_window.get_or_insert_with(|| bugs::BugNotesWindow::new(mtm, true));
        w.show(&self.bug_notes);
        let window = w.window.clone();
        self.present(&window);
    }

    fn show_transcript(&mut self) {
        let window = self.transcript_window().window.clone();
        self.present(&window);
    }

    fn show_setup(&mut self) {
        self.show_setup_at(None);
    }

    /// What Continue, Start or Leave as Is decided: the page's actions sent (the names through Personalise, as the
    /// menu applies them), and on the last page Setup closed and done.
    fn after_setup_advance(&mut self, advance: setup::Advance) {
        let (actions, close) = match advance {
            setup::Advance::Next(actions) | setup::Advance::Wait(actions) => (actions, false),
            setup::Advance::Close(actions) => (actions, true),
            setup::Advance::Stay => return,
        };
        for action in actions {
            match action {
                UiAction::Identity(change) => personalise::apply(self, change),
                // Vella's install: Homebrew runs it; without Homebrew, Vella's page.
                UiAction::InstallVella if !self.setup_vella.brew => self.open_url(VELLA_PAGE),
                other => self.emit(other),
            }
        }
        self.sync_setup();
        if close {
            // Opened again later (the status line's Set Up… for a permission taken away): the permissions, not the tour.
            if let Some(w) = self.setup.as_mut() { w.close(); w.go_to(SetupStep::Permissions); }
            self.emit(UiAction::SetupDone);
        }
    }

    /// Setup at `step` (the menu's Home Folder…, a public first launch), or where it was (first: Welcome).
    fn show_setup_at(&mut self, step: Option<SetupStep>) {
        // Setup is a window: it never opens behind a menu that stays open (e.g. the Listening tile before setup).
        self.menu.cancelTracking();
        let (mtm, target) = (self.mtm, self.target());
        let created = self.setup.is_none();
        let setup = self.setup.get_or_insert_with(|| setup::SetupWindow::new(&target, mtm));
        if created {
            setup.set_home(&self.setup_home);
            setup.set_t3(&self.setup_t3);
            setup.set_vella(&self.setup_vella);
            setup.set_harnesses(&self.setup_harnesses);
            setup.set_decider(&self.decider);
        }
        setup.show(&self.permissions);
        self.sync_setup();
        let Some(setup) = self.setup.as_mut() else { return };
        if let Some(step) = step {
            setup.go_to(step);
        }
        let window = setup.window.clone();
        self.present(&window);
        self.emit(UiAction::SetupOpened);
        self.start_setup_polling();
    }


    fn show_speakers(&mut self) {
        let mtm = self.mtm;
        if self.speakers.is_none() {
            self.speakers = Some(speakers::SpeakersWindow::new(mtm));
        }
        let w = self.speakers.as_mut().unwrap();
        w.show(&self.speakers_list, &self.controller, mtm);
        let window = w.window.clone();
        self.present(&window);
        // No name starts selected: renaming begins with a click.
        window.makeFirstResponder(None);
    }

    /// TCC lacks general grant notifications: a five-second check exists only while unresolved Setup is open.
    fn start_setup_polling(&mut self) {
        if self.options.fake_permissions || self.setup_timer.is_some() {
            return;
        }
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                5.0,
                &self.controller,
                sel!(setupPoll:),
                None,
                true,
            )
        };
        self.setup_timer = Some(timer);
        self.poll_permissions();
    }

    fn poll_permissions(&mut self) {
        let setup_visible = self.setup.as_ref().is_some_and(|w| w.is_visible());
        // Unconfirmed System Audio keeps the poll (and its tap retry below) running while Setup is open.
        let settled = self.all_granted() && self.permission(Permission::SystemAudio) == PermissionState::Granted;
        if !setup_visible || settled {
            if let Some(timer) = self.setup_timer.take() {
                timer.invalidate();
            }
        }
        let mut changed = false;
        for (permission, state) in permissions::status() {
            updates().lock().unwrap().observe_permission(permission, state);
            if self.permission(permission) != state {
                self.set_permission(permission, state);
                changed = true;
            }
        }
        if changed {
            self.emit(UiAction::PermissionsChanged(self.permissions.clone()));
            self.after_permissions_changed();
        }
        // Recovery is a separate action, never a synthetic permission/status transition.
        if matches!(self.permission(Permission::SystemAudio), PermissionState::Unconfirmed | PermissionState::Denied) {
            self.emit(UiAction::RetrySystemAudio);
        }
    }
}

define_class!(
    /// Target of every control and delegate of the menu.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallUIController"]
    pub(crate) struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSMenuDelegate for Controller {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let menu = MainOnly(menu.retain());
            with_state_retry_once(move |s| {
                if std::ptr::eq(&*menu.0, &*s.menu) && !s.menu_open {
                    s.rebuild_menu();
                }
            });
        }

        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, menu: &NSMenu) {
            let menu = MainOnly(menu.retain());
            with_state_retry_once(move |s| {
                // SAFETY: reads the menu's parent; Heimdall's menus are alive on the main thread.
                let submenu = unsafe { menu.0.supermenu() }.is_some_and(|m| std::ptr::eq(&*m, &*s.menu));
                if std::ptr::eq(&*menu.0, &*s.menu) {
                    s.menu_open = true;
                    // menuNeedsUpdate: could not rebuild it (the state was busy): sync what changed while closed.
                    if s.menu_stale {
                        menu::sync_tracking(s);
                        s.menu_stale = false;
                    }
                    menu::log_lit_support(s);
                    // The spinners the close stopped (a rebuild started its own).
                    for p in engine_section::indicators(s).iter().chain(&decider_section::indicators(s)).filter(|p| p.isIndeterminate()) { unsafe { p.startAnimation(None) }; }
                    shortcuts::menu_opened();
                    s.emit(UiAction::RefreshApiCatalog);
                }
                // Drawing Mode chosen in the menu draws over a still of it (and of the submenu it is in).
                let main = std::ptr::eq(&*menu.0, &*s.menu);
                if main || submenu {
                    draw::still::menu_opened(main, s.mtm);
                }
            });
        }

        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, menu: &NSMenu) {
            // Any top-level menu of Heimdall's closing ends a menu still's life, even when the state is busy.
            // SAFETY: reads the menu's parent on the main thread.
            if unsafe { menu.supermenu() }.is_none() {
                draw::still::menu_closed();
            }
            // No row keeps the pointer's highlight past its menu (a press that closes it gets no exit).
            controls::menu_closed(menu);
            let menu = MainOnly(menu.retain());
            with_state(move |s| {
                if std::ptr::eq(&*menu.0, &*s.menu) {
                    s.menu_open = false;
                    // An unconfirmed click switches nothing.
                    if s.pending_choice.take().is_some() { s.menu_stale = true; }
                    // Nor does a precision clicked in Decider without Confirm (Escape closes the menu too).
                    if s.decider_pending.take().is_some() { s.menu_stale = true; }
                    // An armed trash does not survive the menu.
                    s.api_expanded.retain(|(k, _)| k != menu::TRASH_ARMED);
                    // Nothing animates in a closed menu: its spinners and bars stop until it opens again.
                    for p in engine_section::indicators(s).into_iter().chain(decider_section::indicators(s)) { unsafe { p.stopAnimation(None) }; }
                    shortcuts::menu_closed();
                    s.listen_pending = None;
                }
            });
        }
    }

    impl Controller {
        /// Agent ▸ Model ▸ This Mac ▸ Local Splash Engine: proposes the agent on the local engine (Confirm switches).
        #[unsafe(method(chooseLocalEngine:))]
        fn choose_local_engine(&self, _sender: &NSButton) {
            with_state_after_event(move |s| {
                let key = (menu::LOCAL_DETAIL.0.to_string(), menu::LOCAL_DETAIL.1.to_string());
                s.choose(model_choice::Choice::Local, key);
            });
        }

        /// Decider ▸ a precision: marked for Confirm, or the mark cleared (`State::pick_decider`).
        #[unsafe(method(pickDeciderPrecision:))]
        fn pick_decider_precision(&self, sender: &NSMenuItem) {
            let index = sender.tag() as usize;
            with_state_after_event(move |s| s.pick_decider(index));
        }

        /// Engine ▸ a model on this Mac: it loads (`State::load_pick`).
        #[unsafe(method(loadEnginePick:))]
        fn load_engine_pick(&self, sender: &NSMenuItem) {
            let index = sender.tag() as usize;
            with_state_after_event(move |s| s.load_pick(index));
        }

        /// Providers ▸ Claude Code subscription ▸ Remove: back in Add Provider (never while it is the agent).
        #[unsafe(method(removeClaudeViaT3:))]
        fn remove_claude_via_t3(&self, _sender: &NSMenuItem) {
            with_state(|s| {
                if s.settings_value.agent_relay { return; }
                s.settings_value.relay_added = false;
                // A relay click waiting for Confirm goes with its row.
                if matches!(s.pending_choice, Some(model_choice::Choice::Relay(_))) { s.pending_choice = None; }
                s.emit(UiAction::RemoveClaudeViaT3);
            });
            with_state_after_event(|s| s.rebuild_whole_agent_models());
        }

        /// API Providers ▸ an added model: the agent's model from now on. A model that cannot run is refused
        /// (its row says why); the selected one stays selected.
        #[unsafe(method(chooseApiModel:))]
        fn choose_api_model(&self, sender: &AnyObject) {
            let index: isize = unsafe { objc2::msg_send![sender, tag] };
            with_state_after_event(move |s| {
                let Some((_, provider, id)) = s.api_rows.models.get(index as usize).cloned() else { return };
                let available = s.api.models.iter().any(|m| m.provider == provider && m.id == id && m.unavailable.is_none());
                // A model that cannot run is never proposed; its row says why.
                if !available { s.refresh_menu(); return; }
                s.choose(model_choice::Choice::Api { provider: provider.clone(), id: id.clone() }, (provider, id));
            });
        }

        /// Agent ▸ Model ▸ Claude Code subscription: proposes the relay on its current model (or T3's first).
        #[unsafe(method(chooseAgentRelay:))]
        fn choose_agent_relay(&self, _sender: &AnyObject) {
            with_state_after_event(|s| {
                let model = s.settings_value.agent_relay_model.clone().or_else(|| s.relay_models.first().map(|m| m.id.clone()));
                s.choose(model_choice::Choice::Relay(model), (model_choice::RELAY_KEY.0.into(), model_choice::RELAY_KEY.1.into()));
            });
        }

        /// A precision's download (its size first): it starts now; the menu shows its progress.
        #[unsafe(method(downloadPrecision:))]
        fn download_precision(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state_after_event(move |s| s.precision(index, Press::Download));
        }

        /// A download's stop.
        #[unsafe(method(cancelPrecisionDownload:))]
        fn cancel_precision_download(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state_after_event(move |s| s.precision(index, Press::Stop));
        }

        /// A precision's trash, pressed once: it turns into the red Delete pill (one trash armed at a time).
        #[unsafe(method(trashPrecision:))]
        fn trash_precision(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state_after_event(move |s| s.precision(index, Press::Trash));
        }

        /// The red Delete pill: that precision's files go (the app refuses while the engine serves it).
        #[unsafe(method(deletePrecision:))]
        fn delete_precision(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state_after_event(move |s| s.precision(index, Press::Delete));
        }

        /// Agent ▸ Model ▸ Claude Code subscription ▸ a model: proposes the relay on it.
        #[unsafe(method(chooseRelayModel:))]
        fn choose_relay_model(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state_after_event(move |s| {
                let Some((_, id)) = s.relay_model_rows.get(index).cloned() else { return };
                s.choose(model_choice::Choice::Relay(Some(id)), (model_choice::RELAY_KEY.0.into(), model_choice::RELAY_KEY.1.into()));
            });
        }

        /// Agent ▸ Model ▸ Claude Code subscription ▸ an account (Automatic or a T3 Claude instance): proposes the relay on it.
        #[unsafe(method(chooseRelayAccount:))]
        fn choose_relay_account(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state_after_event(move |s| {
                let Some((_, id)) = s.relay_account_rows.get(index).cloned() else { return };
                s.choose(model_choice::Choice::RelayAccount(id), (model_choice::RELAY_KEY.0.into(), model_choice::RELAY_KEY.1.into()));
            });
        }

        /// Claude Code subscription's disclosure: its T3 models and accounts show or hide in place.
        #[unsafe(method(toggleRelayDetail:))]
        fn toggle_relay_detail(&self, sender: &NSButton) {
            let open = sender.state() == objc2_app_kit::NSControlStateValueOn;
            with_state(move |s| {
                let key = (model_choice::RELAY_KEY.0.to_string(), model_choice::RELAY_KEY.1.to_string());
                if open { s.api_expanded.insert(key); } else { s.api_expanded.remove(&key); }
                for item in &s.relay_detail.1 { item.setHidden(!open); }
            });
        }

        /// API Providers ▸ Add Model ▸ a model: checked adds it to the list, unchecked removes it.
        #[unsafe(method(toggleApiModel:))]
        fn toggle_api_model(&self, sender: &AnyObject) {
            let index: isize = unsafe { objc2::msg_send![sender, tag] };
            // The button already flipped when clicked directly; Return on the item flips it here.
            let direct = sender.downcast_ref::<NSButton>().is_some();
            with_state(move |s| {
                let Some((button, provider, id)) = s.api_rows.catalog.get(index as usize).cloned() else { return };
                let Some(row) = s.api.catalog.iter_mut().find(|r| r.provider == provider && r.id == id) else { return };
                let on = if direct { button.state() == objc2_app_kit::NSControlStateValueOn } else { !row.added };
                if on == row.added || (on && row.unavailable.is_some()) { s.refresh_menu(); return; }
                row.added = on;
                s.emit(UiAction::SetApiModelAdded { provider, id, on });
                s.refresh_menu();
            });
        }

        /// A model row's disclosure: its context slider and price, shown or hidden in place while the menu stays open.
        #[unsafe(method(toggleApiDetail:))]
        fn toggle_api_detail(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            let open = sender.state() == objc2_app_kit::NSControlStateValueOn;
            with_state(move |s| {
                let Some((_, provider, id)) = s.api_rows.models.get(index).cloned() else { return };
                if open { s.api_expanded.insert((provider, id)); } else { s.api_expanded.remove(&(provider, id)); }
                if let Some((_, items)) = s.api_rows.details.get(index) {
                    for item in items { item.setHidden(!open); }
                }
            });
        }

        /// A context slider: the label follows the knob; the choice goes to the app when the knob is let go (or set
        /// by keyboard or accessibility), not on every step of a drag.
        #[unsafe(method(apiContextSlider:))]
        fn api_context_slider(&self, sender: &objc2_app_kit::NSSlider) {
            let index = sender.tag() as usize;
            let stop = sender.integerValue().max(0) as usize;
            let dragging = objc2_app_kit::NSApplication::sharedApplication(MainThreadMarker::from(self)).currentEvent()
                .is_some_and(|e| matches!(e.r#type(), objc2_app_kit::NSEventType::LeftMouseDragged | objc2_app_kit::NSEventType::LeftMouseDown));
            with_state(move |s| {
                let Some(Some((_, field, steps))) = s.api_rows.sliders.get(index) else { return };
                field.setStringValue(&util::ns(&menu::slider_label(steps, stop)));
                let tokens = steps[stop.min(steps.len() - 1)];
                let Some((_, provider, id)) = s.api_rows.models.get(index).cloned() else { return };
                if dragging { return; }
                let Some(row) = s.api.models.iter_mut().find(|m| m.provider == provider && m.id == id) else { return };
                let Some(detail) = row.detail.as_mut().filter(|d| d.context != tokens) else { return };
                detail.context = tokens;
                s.emit(UiAction::SetApiModelContext { provider, id, tokens });
            });
        }

        /// A model's thinking selector: the level goes to the app at once (it applies from the agent's next request).
        #[unsafe(method(apiThinking:))]
        fn api_thinking(&self, sender: &objc2_app_kit::NSSegmentedControl) {
            let index = sender.tag() as usize;
            let segment = sender.selectedSegment();
            with_state(move |s| {
                let Some(Some((_, levels))) = s.api_rows.thinking.get(index) else { return };
                let Some(level) = usize::try_from(segment).ok().and_then(|i| levels.get(i)).cloned() else { return };
                let Some((_, provider, id)) = s.api_rows.models.get(index).cloned() else { return };
                let Some(row) = s.api.models.iter_mut().find(|m| m.provider == provider && m.id == id) else { return };
                let Some(detail) = row.detail.as_mut().filter(|d| d.thinking != level) else { return };
                detail.thinking = level.clone();
                s.emit(UiAction::SetApiModelThinking { provider, id, level });
            });
        }

        /// [menu lane] The built-in engine row's disclosure: its context slider and thinking, in place.
        #[unsafe(method(toggleEngineDetail:))]
        fn toggle_engine_detail(&self, sender: &NSButton) {
            let open = sender.state() == objc2_app_kit::NSControlStateValueOn;
            with_state(move |s| {
                for item in &s.api_rows.engine_detail.1 { item.setHidden(!open); }
                let key = (menu::LOCAL_DETAIL.0.to_string(), menu::LOCAL_DETAIL.1.to_string());
                if open { s.api_expanded.insert(key); } else { s.api_expanded.remove(&key); }
            });
        }

        /// [menu lane] The built-in engine's context slider: the label follows the knob; the stop goes to the app when
        /// let go (a running local agent restarts inside the new window).
        #[unsafe(method(engineContextSlider:))]
        fn engine_context_slider(&self, sender: &objc2_app_kit::NSSlider) {
            let stop = sender.integerValue().max(0) as usize;
            let dragging = objc2_app_kit::NSApplication::sharedApplication(MainThreadMarker::from(self)).currentEvent()
                .is_some_and(|e| matches!(e.r#type(), objc2_app_kit::NSEventType::LeftMouseDragged | objc2_app_kit::NSEventType::LeftMouseDown));
            with_state(move |s| {
                let Some((_, field, steps)) = &s.api_rows.engine_slider else { return };
                field.setStringValue(&util::ns(&menu::slider_label(steps, stop)));
                let tokens = steps[stop.min(steps.len() - 1)];
                if dragging || menu::engine_budget(steps, s.settings_value.engine_context) == tokens { return; }
                s.settings_value.engine_context = Some(tokens);
                s.emit(UiAction::SetEngineContext(tokens));
            });
        }

        /// Background Tasks ▸ Model ▸ a model: every background job's model from now on. A row that cannot run is
        /// refused (it says why); the chosen one stays chosen.
        #[unsafe(method(chooseBackgroundModel:))]
        fn choose_background_model(&self, sender: &AnyObject) {
            let index: isize = unsafe { objc2::msg_send![sender, tag] };
            let enabled = sender.downcast_ref::<NSButton>().is_none_or(|b| b.isEnabled());
            with_state(move |s| {
                let Some((_, provider, id)) = s.background_rows.models.get(index as usize).cloned() else { return };
                let key = (provider.clone(), id.clone());
                if enabled && !menu::background_row_on(&s.settings_value.background_model, &provider, &id) {
                    s.settings_value.background_model = key;
                    s.emit(UiAction::SetBackgroundModel { provider, id });
                }
                s.refresh_menu();
            });
        }

        /// Background Tasks ▸ Model ▸ a provider's disclosure (or a press on its row): its models show or hide in place.
        #[unsafe(method(toggleBackgroundProvider:))]
        fn toggle_background_provider(&self, sender: &NSButton) {
            let open = sender.state() == objc2_app_kit::NSControlStateValueOn;
            let sender = sender.retain();
            with_state(move |s| background_picker::toggle(s, &sender, open));
        }

        /// Background Tasks ▸ Model ▸ the chosen model's thinking selector: its level for background jobs, at once.
        #[unsafe(method(backgroundEffort:))]
        fn background_effort(&self, sender: &objc2_app_kit::NSSegmentedControl) {
            let sender = sender.retain();
            with_state(move |s| background_picker::level_picked(s, &sender));
        }

        /// Background Tasks ▸ a task's own Model ▸ a model, or Use Default Model (`UiAction::SetTaskModel`).
        #[unsafe(method(chooseTaskModel:))]
        fn choose_task_model(&self, sender: &NSButton) {
            let sender = sender.retain();
            with_state(move |s| background_picker::task_chosen(s, &sender));
        }

        /// Inference Engine ▸ Thinking for the local model.
        #[unsafe(method(engineThinking:))]
        fn engine_thinking(&self, sender: &objc2_app_kit::NSSegmentedControl) {
            let segment = sender.selectedSegment();
            with_state(move |s| {
                let Some(level) = usize::try_from(segment).ok().and_then(|i| crate::model::LOCAL_THINKING.get(i)).map(|l| l.to_string()) else { return };
                if s.settings_value.engine_thinking == level { return; }
                s.settings_value.engine_thinking = level.clone();
                s.emit(UiAction::SetEngineThinking(level));
            });
        }

        /// [providers] Add Model's search field: the provider's list filters as the user types.
        #[unsafe(method(apiModelSearch:))]
        fn api_model_search(&self, sender: &objc2_app_kit::NSSearchField) {
            let index = sender.tag() as usize;
            let query = sender.stringValue().to_string();
            with_state(move |s| {
                let Some(provider) = s.api.providers.get(index).map(|p| p.id.clone()) else { return };
                menu::filter_picker(&s.api_rows, &provider, &query);
            });
        }

        /// [providers] Background Tasks ▸ Model ▸ Add Model…: the menu closes, then Agent ▸ Model opens on its own under the menu
        /// bar item (a default-mode timer fires only once the menu's tracking has ended).
        #[unsafe(method(openAgentModels:))]
        fn open_agent_models(&self, _sender: &AnyObject) {
            with_state(|s| {
                s.menu.cancelTracking();
                if !s.live() { s.agent_models_requested = true; return; }
                unsafe {
                    NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(0.0, &s.controller, sel!(popUpAgentModels:), None, false);
                }
            });
        }

        /// `heimdall-ui-demo --submenus-left`: the next step inside the open menu.
        #[unsafe(method(submenusLeftStep:))]
        fn submenus_left_step(&self, _timer: &NSTimer) {
            submenus_left::check::step();
        }

        /// `heimdall-ui-demo --menu-live`: the next step inside the open menu.
        #[unsafe(method(menuLiveStep:))]
        fn menu_live_step(&self, _timer: &NSTimer) {
            menu_live::step();
        }

        #[unsafe(method(popUpAgentModels:))]
        fn pop_up_agent_models(&self, _timer: &NSTimer) {
            let Some((menu, button)) = with_state_now(|s| {
                let target = s.target();
                let menu = menu::agent_model_menu(s, Some(&target), s.mtm);
                menu.setDelegate(Some(ProtocolObject::from_ref(&*s.controller)));
                s.menu_open = true;
                (menu, s.status_item.as_ref().and_then(|i| i.button(s.mtm)))
            }) else { return };
            let Some(button) = button else { return };
            // Where the status menu itself opens, under the helmet; blocks until it closes.
            let height = button.bounds().size.height;
            menu.popUpMenuPositioningItem_atLocation_inView(None, objc2_foundation::NSPoint::new(0.0, height + 4.0), Some(&button));
            // The pop-up closed: an unconfirmed click in it switches nothing and does not come back in the menu.
            with_state(|s| { s.menu_open = false; if s.pending_choice.take().is_some() { s.menu_stale = true; } });
        }

        /// Connect <provider>… / Sign In Again…: the browser sign-in starts and the panel shows its code.
        #[unsafe(method(connectProvider:))]
        fn connect_provider(&self, sender: &objc2_app_kit::NSMenuItem) {
            let index = sender.tag() as usize;
            with_state(move |s| s.show_sign_in_panel(index));
        }

        /// Add Provider ▸ Local Splash Engine: it joins On This Mac and Providers; the agent's model stays (28 Oct
        /// 17:26: this turned the engine on and made its model the agent's, which then waited for a 27B to load).
        #[unsafe(method(addBuiltInEngine:))]
        fn add_built_in_engine(&self, _sender: &objc2_app_kit::NSMenuItem) {
            with_state(|s| {
                s.settings_value.engine_added = true;
                s.emit(UiAction::AddBuiltInEngine);
            });
            // The menu stays open: the provider moves between Add Provider and Providers in place, once the press that
            // asked for it has left the row it replaces.
            with_state_after_event(|s| s.rebuild_whole_agent_models());
        }

        /// Providers ▸ Local Splash Engine ▸ Remove.
        #[unsafe(method(removeBuiltInEngine:))]
        fn remove_built_in_engine(&self, _sender: &objc2_app_kit::NSMenuItem) {
            with_state(|s| {
                s.settings_value.engine_added = false;
                s.emit(UiAction::RemoveBuiltInEngine);
            });
            // The menu stays open: the provider moves between Add Provider and Providers in place, once the press that
            // asked for it has left the row it replaces.
            with_state_after_event(|s| s.rebuild_whole_agent_models());
        }

        /// Add Provider ▸ Claude Code subscription: the relay becomes the agent and leaves Add Provider.
        #[unsafe(method(claudeViaT3:))]
        fn claude_via_t3(&self, _sender: &objc2_app_kit::NSMenuItem) {
            // The Claude relay added as a provider (Settings::relay_added, docs/claude-relay.md): its row joins Agent ▸
            // Model's Cloud list and it leaves Add Provider; the agent keeps its model until the user picks the relay there.
            with_state(|s| {
                s.settings_value.relay_added = true;
                s.emit(UiAction::ClaudeViaT3);
            });
            // The menu stays open, as for the built-in engine: the row leaves Add Provider once the press has left it.
            with_state_after_event(|s| s.rebuild_whole_agent_models());
        }

        /// Local Splash Engine ▸ Licence…: the model's bundled licence text; outside the app bundle, nothing.
        #[unsafe(method(openModelLicence:))]
        fn open_model_licence(&self, sender: &objc2_app_kit::NSMenuItem) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some((_, file)) = s.engine.models.get(index).and_then(|m| provider_rows::model_licence(&m.id)) else { return };
                if !s.live() { return; }
                let bundle = objc2_foundation::NSBundle::mainBundle();
                let (Some(resources), path) = (bundle.resourcePath(), bundle.bundlePath()) else { return };
                let Some(file) = provider_rows::licence_path(std::path::Path::new(&path.to_string()), std::path::Path::new(&resources.to_string()), file) else { return };
                let url = objc2_foundation::NSURL::fileURLWithPath(&util::ns(&file.to_string_lossy()));
                objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url);
            });
        }

        /// The panel's Use API Key…: stop the sign-in and take a pasted key instead.
        #[unsafe(method(providerUseKey:))]
        fn provider_use_key(&self, _sender: &AnyObject) {
            with_state(|s| {
                let Some(provider) = s.key_panel.as_ref().map(|p| p.provider.clone()) else { return };
                s.emit(UiAction::CancelProviderLogin { provider });
                let clipboard = s.clipboard();
                if let Some(panel) = s.key_panel.as_mut() { panel.reset(clipboard.as_deref()); }
            });
        }

        /// The panel's Open Browser: the pending sign-in's page again; after a failure, Try Again restarts it.
        #[unsafe(method(providerLoginOpen:))]
        fn provider_login_open(&self, _sender: &AnyObject) {
            with_state(|s| {
                let Some(panel) = s.key_panel.as_mut() else { return };
                match panel.url.clone() {
                    Some(url) => s.open_url(&url),
                    None => {
                        let provider = panel.provider.clone();
                        panel.start_sign_in();
                        s.emit(UiAction::StartProviderLogin { provider });
                    }
                }
            });
        }

        #[unsafe(method(disconnectProvider:))]
        fn disconnect_provider(&self, sender: &objc2_app_kit::NSMenuItem) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some(p) = s.api.providers.get(index) else { return };
                s.emit(UiAction::DisconnectProvider { provider: p.id.clone() });
            });
        }

        #[unsafe(method(providerKeyConnect:))]
        fn provider_key_connect(&self, _sender: &AnyObject) {
            with_state(|s| {
                let Some(panel) = &s.key_panel else { return };
                let key = panel.key();
                let provider = panel.provider.clone();
                // [providers] A provider on the user's own URL connects with the URL and (unless keyless) the key.
                if let Some(key_required) = panel.custom {
                    let url = panel.url();
                    if url.trim().is_empty() { panel.set_status(Some(("Enter the server's URL", true))); return; }
                    if key_required && key.trim().is_empty() { panel.set_status(Some(("Paste an API key", true))); return; }
                    panel.set_status(Some(("Checking…", false)));
                    panel.set_busy(true);
                    s.emit(UiAction::ConnectProviderEndpoint { provider, url, key });
                    return;
                }
                if key.trim().is_empty() { panel.set_status(Some(("Paste an API key", true))); return; }
                panel.set_status(Some(("Checking…", false)));
                panel.set_busy(true);
                s.emit(UiAction::ConnectProviderKey { provider, key });
            });
        }

        #[unsafe(method(providerKeyCancel:))]
        fn provider_key_cancel(&self, _sender: &AnyObject) {
            with_state(|s| {
                let Some((provider, mode)) = s.key_panel.as_ref().map(|p| (p.provider.clone(), p.mode)) else { return };
                if mode == provider_key::Mode::SignIn { s.emit(UiAction::CancelProviderLogin { provider }); }
                if let Some(panel) = &s.key_panel { panel.panel.close(); }
            });
        }

        #[unsafe(method(providerKeyConsole:))]
        fn provider_key_console(&self, _sender: &AnyObject) {
            with_state(|s| {
                let Some(provider) = s.key_panel.as_ref().map(|p| p.provider.clone()) else { return };
                // [menu lane] ElevenLabs is a speech provider, not in the API list: its keys page.
                let url = if provider == crate::speech_voice::ELEVENLABS { Some(crate::speech_voice::ELEVENLABS_KEYS_URL.to_string()) } else { s.api.provider(&provider).map(|p| p.console_url.clone()) };
                if let Some(url) = url { s.open_url(&url); }
            });
        }

        #[unsafe(method(chooseMicrophone:))]
        fn choose_microphone(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some((_, false, id)) = s.microphone_buttons.get(index).cloned() else { return };
                let Some(device) = s.microphones.devices.iter().find(|m| m.id == id).cloned() else { return };
                if !device.connected { s.refresh_menu(); return; }
                let primary = (!device.built_in).then(|| id.clone());
                if s.microphones.primary != primary || s.microphones.in_use.as_deref() != Some(id.as_str()) {
                    s.microphones.primary = primary;
                    s.microphones.in_use = Some(id.clone());
                    s.emit(UiAction::SelectMicrophone { id });
                }
                s.refresh_menu();
            });
        }

        #[unsafe(method(toggleMicrophone:))]
        fn toggle_microphone(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some((_, true, id)) = s.microphone_buttons.get(index).cloned() else { return };
                let on = !s.microphones.extras.contains(&id);
                if on { s.microphones.extras.push(id.clone()); } else { s.microphones.extras.retain(|e| e != &id); }
                s.emit(UiAction::SetMicrophoneOn { id, on });
                s.refresh_menu();
            });
        }

        /// App Audio ▸ a checkbox: record this app from now on, or stop tapping it.
        #[unsafe(method(toggleApp:))]
        fn toggle_app(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some((_, id)) = s.app_buttons.get(index).cloned() else { return };
                let Some(app) = s.app_audio.apps.iter_mut().find(|a| a.id == id) else { return };
                app.recorded = !app.recorded;
                let on = app.recorded;
                s.emit(UiAction::SetAppRecorded { id, on });
                s.refresh_menu();
            });
        }

        #[unsafe(method(reload:))]
        fn reload(&self, _sender: &AnyObject) {
            with_state(|s| { if !s.runtime_settings.pending_reload.is_empty() { s.emit(UiAction::Reload); } });
        }

        #[unsafe(method(restartSession:))]
        fn restart_session(&self, _sender: &AnyObject) {
            with_state(|s| s.emit(UiAction::RestartSession));
        }

        #[unsafe(method(openAgent:))]
        fn open_agent(&self, _sender: &AnyObject) {
            with_state(|s| s.show_agent());
        }

        #[unsafe(method(chooseScreen:))]
        fn choose_screen(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some((_, choice)) = s.screen_buttons.get(index) else { return };
                let mut screen = s.runtime_settings.screen;
                match *choice {
                    menu::ScreenChoice::Resolution(r) => screen.resolution = r,
                    menu::ScreenChoice::Fps(fps) => screen.fps = fps,
                }
                if screen != s.runtime_settings.screen {
                    s.runtime_settings.screen = screen;
                    s.emit(UiAction::SetScreenRecording(screen));
                }
                s.refresh_menu();
            });
        }

        #[unsafe(method(openTranscript:))]
        fn open_transcript(&self, _sender: &AnyObject) {
            with_state(|s| s.show_transcript());
        }

        #[unsafe(method(openBugNotes:))]
        fn open_bug_notes(&self, _sender: &AnyObject) {
            with_state(|s| s.show_bug_notes());
        }

        #[unsafe(method(openSpeakers:))]
        fn open_speakers(&self, _sender: &AnyObject) {
            with_state(|s| s.show_speakers());
        }

        /// Launch at Login's item action (`performActionForItemAtIndex`); a click goes through `check_row_clicked`.
        #[unsafe(method(toggleLaunchAtLogin:))]
        fn toggle_launch_at_login(&self, _sender: &AnyObject) {
            with_state(|s| s.toggle_launch_at_login());
        }
        /// Show Islands on Streams' item action; a click goes through `check_row_clicked`.
        #[unsafe(method(toggleIslandsOnStreams:))]
        fn toggle_islands_on_streams(&self, _sender: &AnyObject) {
            with_state(|s| s.toggle_islands_on_streams());
        }

        /// Speaking ▸ Wake Sound's item action; a click goes through `check_row_clicked`.
        /// Shortcuts ▸ an action (its tag is the shortcut's id): the same as its key.
        #[unsafe(method(runShortcut:))]
        fn run_shortcut(&self, sender: &NSMenuItem) {
            let Some(s) = crate::shortcuts::Shortcut::from_id(sender.tag() as u32) else { return };
            if s == crate::shortcuts::Shortcut::Drawing {
                draw::still::chosen_in_menu();
            }
            if let Some(mtm) = MainThreadMarker::new() {
                shortcuts::run(s, mtm);
            }
        }

        #[unsafe(method(toggleIslandsShown:))]
        fn toggle_islands_shown(&self, _sender: &AnyObject) {
            with_state(|s| s.toggle_islands_shown());
        }

        #[unsafe(method(toggleWakeSound:))]
        fn toggle_wake_sound(&self, _sender: &AnyObject) {
            with_state(|s| s.toggle_wake_sound());
        }

        /// Background Tasks ▸ a job's item action (its tag is the row); a click goes through `check_row_clicked`.
        #[unsafe(method(toggleBackgroundTask:))]
        fn toggle_background_task(&self, sender: &NSMenuItem) {
            let Some(task) = menu::background_task_from_row(sender.tag() as usize) else { return };
            with_state(move |s| s.toggle_background_task(task));
        }

        #[unsafe(method(openSetup:))]
        fn open_setup(&self, _sender: &AnyObject) {
            with_state(|s| s.show_setup());
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: &AnyObject) {
            with_state(|s| s.quit());
        }

        /// The updater's row (public edition): Check for Updates…, or Update to X… once one is offered.
        #[unsafe(method(updateRow:))]
        fn update_row(&self, _sender: &AnyObject) {
            with_state(|s| s.emit(UiAction::UpdateRow));
        }

        /// The next queued updater alert (`State::update_alert`), as a NON-modal window: during a modal session AppKit
        /// holds the status menu's actions, so Quit did nothing while the alert was up and the install tool gave up on
        /// the app quitting (VM finding P-01, 28 Oct; a timer-run modal still failed in the VM). One alert at a time.
        #[unsafe(method(showUpdateAlert:))]
        fn show_update_alert(&self, _timer: &NSTimer) {
            let mtm = self.mtm();
            if OPEN_ALERT.with(|a| a.borrow().is_some()) { return }
            let Some(alert) = PENDING_ALERTS.with(|q| q.borrow_mut().pop_front()) else { return };
            let panel = objc2_app_kit::NSAlert::new(mtm);
            panel.setMessageText(&util::ns(&alert.title));
            panel.setInformativeText(&util::ns(&alert.message));
            panel.addButtonWithTitle(&util::ns(if alert.offer { "Update" } else { "OK" }));
            if alert.offer {
                // Escape is Later: the alert has no close button.
                panel.addButtonWithTitle(&util::ns("Later")).setKeyEquivalent(&util::ns("\u{1b}"));
            }
            panel.layout();
            for (i, button) in panel.buttons().iter().enumerate() {
                unsafe {
                    button.setTarget(Some(self));
                    button.setAction(Some(sel!(updateAlertButton:)));
                }
                button.setTag(i as isize);
            }
            let window = panel.window();
            // Above Setup and other app windows, which open later at the same centre and hid it in the VM.
            window.setLevel(objc2_app_kit::NSFloatingWindowLevel);
            window.center();
            NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
            window.makeKeyAndOrderFront(None);
            OPEN_ALERT.with(|a| *a.borrow_mut() = Some((panel, alert.offer)));
        }

        /// A button of the open updater alert: the offer's Update (tag 0) installs; every button closes it and shows
        /// the next queued alert.
        #[unsafe(method(updateAlertButton:))]
        fn update_alert_button(&self, sender: &NSButton) {
            let Some((panel, offer)) = OPEN_ALERT.with(|a| a.borrow_mut().take()) else { return };
            panel.window().orderOut(None);
            // Released on the next main-queue turn, never inside its own button's action (AppKit is still tracking it).
            let keep = MainOnly(panel);
            DispatchQueue::main().exec_async(move || drop(keep));
            if offer && sender.tag() == 0 {
                with_state(|s| s.emit(UiAction::InstallUpdate));
            }
            if PENDING_ALERTS.with(|q| !q.borrow().is_empty()) {
                unsafe {
                    NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                        0.0, self, sel!(showUpdateAlert:), None, false,
                    );
                }
            }
        }

        #[unsafe(method(quitFallback:))]
        fn quit_fallback(&self, _timer: &NSTimer) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }

        // The floating name entry's typed name.
        #[unsafe(method(voiceNameConfirm:))]
        fn voice_name_confirm(&self, _sender: &AnyObject) {
            with_state(|s| match s.name_entry.answer() {
                Some((speaker::Purpose::Voice(code), name)) => s.emit(UiAction::NameVoice { code, name }),
                // The same prompt names a friend to invite (Friends ▸ Invite a friend…).
                Some((speaker::Purpose::Friend, name)) => s.emit(UiAction::InviteFriendNamed { name }),
                // The same prompt types the reply to a friend's ask (its Answer… on the card).
                Some((speaker::Purpose::Reply(card_id), text)) => s.emit(UiAction::FriendReply { card_id, text }),
                None => {}
            });
        }

        #[unsafe(method(voiceNameCancel:))]
        fn voice_name_cancel(&self, _sender: &AnyObject) {
            with_state(|s| s.name_entry.close());
        }

        // Friends (personal edition): Invite a friend…, a friend's Revoke (the row's index in the menu as built), and
        // the invite panel's Done.
        #[unsafe(method(inviteFriend:))]
        fn invite_friend(&self, _sender: &AnyObject) {
            with_state(|s| s.emit(UiAction::InviteFriend));
        }

        #[unsafe(method(revokeFriend:))]
        fn revoke_friend(&self, sender: &AnyObject) {
            let row = sender.downcast_ref::<NSMenuItem>().map(|item| item.tag()).unwrap_or(-1);
            with_state(move |s| {
                if let Some(id) = usize::try_from(row).ok().and_then(|i| s.friend_menu_ids.get(i)).cloned() {
                    s.emit(UiAction::RevokeFriend { id });
                }
            });
        }

        #[unsafe(method(friendInviteDone:))]
        fn friend_invite_done(&self, _sender: &AnyObject) {
            with_state(|s| s.friend_invite.close());
        }

        // [chrome] The transcript's "Who is Speaker 2?" sheet.
        #[unsafe(method(transcriptNameConfirm:))]
        fn transcript_name_confirm(&self, _sender: &AnyObject) {
            with_state(|s| {
                let named = s.transcript.as_mut().and_then(|t| t.finish_naming(true));
                if let Some((span, name)) = named {
                    s.emit(UiAction::NameTranscriptSpeaker { line_id: span.line_id, source: span.source, speaker: span.speaker, name });
                }
            });
        }

        #[unsafe(method(transcriptNameCancel:))]
        fn transcript_name_cancel(&self, _sender: &AnyObject) {
            with_state(|s| { if let Some(t) = s.transcript.as_mut() { t.finish_naming(false); } });
        }

        /// Ask Again on an ignored voice: it leaves the list and is prompted for the next time it is heard.
        #[unsafe(method(speakerUnignore:))]
        fn speaker_unignore(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some(known) = s.speakers_list.get(index).filter(|k| k.ignored) else { return };
                let id = known.id.clone();
                s.speakers_list.remove(index);
                s.emit(UiAction::UnignoreSpeaker { id });
                let (mtm, controller) = (s.mtm, s.controller.clone());
                if let Some(w) = &mut s.speakers {
                    w.show(&s.speakers_list, &controller, mtm);
                }
            });
        }

        /// A row's listen switch: on, Heimdall acts on that voice's requests until it is turned off.
        #[unsafe(method(speakerListen:))]
        fn speaker_listen(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            let on = sender.state() == objc2_app_kit::NSControlStateValueOn;
            let switch = sender.retain();
            with_state(move |s| {
                let Some(known) = s.speakers_list.get_mut(index).filter(|k| matches!(k.listen, Listen::On | Listen::Off)) else { return };
                known.listen = if on { Listen::On } else { Listen::Off };
                switch.setToolTip(Some(&util::ns(&speakers::listen_tip(&known.name, known.listen))));
                let id = known.id.clone();
                s.emit(UiAction::ListenToSpeaker { id, on });
            });
        }

        // Speakers window.
        #[unsafe(method(speakerRenamed:))]
        fn speaker_renamed(&self, sender: &NSTextField) {
            let index = sender.tag() as usize;
            let name = sender.stringValue().to_string().trim().to_string();
            with_state(move |s| {
                let Some(known) = s.speakers_list.get_mut(index) else { return };
                if name.is_empty() || name == known.name {
                    return;
                }
                known.name = name.clone();
                let id = known.id.clone();
                s.emit(UiAction::RenameSpeaker { id, name });
            });
        }

        #[unsafe(method(speakerEdit:))]
        fn speaker_edit(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                if let Some(w) = &s.speakers {
                    w.edit(index);
                }
            });
        }

        /// Forget asks first (Vella confirms deletions with an NSAlert), as a sheet on the Speakers window.
        #[unsafe(method(speakerForget:))]
        fn speaker_forget(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some(known) = s.speakers_list.get(index) else { return };
                let Some(window) = s.speakers.as_ref().map(|w| w.window.clone()) else { return };
                let id = known.id.clone();
                let alert = speakers::forget_alert(&known.name, s.mtm);
                let handler = block2::RcBlock::new(move |response: NSModalResponse| {
                    if response != NSAlertSecondButtonReturn {
                        return;
                    }
                    let id = id.clone();
                    with_state(move |s| {
                        let Some(index) = s.speakers_list.iter().position(|k| k.id == id) else { return };
                        s.speakers_list.remove(index);
                        s.emit(UiAction::ForgetSpeaker { id });
                        let (mtm, controller) = (s.mtm, s.controller.clone());
                        if let Some(w) = &mut s.speakers {
                            w.show(&s.speakers_list, &controller, mtm);
                        }
                    });
                });
                alert.beginSheetModalForWindow_completionHandler(&window, Some(&handler));
            });
        }

        // Setup window.
        #[unsafe(method(setupAllow:))]
        fn setup_allow(&self, _sender: &AnyObject) {
            with_state(|s| {
                if s.all_granted() {
                    return;
                }
                // Heimdall On is ordered at the click, before any dialog: an Off clicked while the permission
                // dialogs are up is newer and wins. The completion below carries no On/Off intent.
                if let Some(w) = s.setup.as_mut() { w.allow_clicked(); }
                s.emit(UiAction::SetupAllowed);
                if s.options.fake_permissions {
                    // Looks-only: grant every permission so the demo shows the finished state.
                    for permission in Permission::ALL {
                        s.set_permission(permission, PermissionState::Granted);
                    }
                    s.emit(UiAction::RequestSystemAudio);
                    s.after_permissions_changed();
                    return;
                }
                permissions::request_all(|| post_action(UiAction::RequestSystemAudio));
                s.after_permissions_changed();
            });
        }

        /// Continue: the next step (the home folder kept, the user's name…), or on the last page Start: Setup closes and
        /// the first-run greeting follows.
        #[unsafe(method(setupNext:))]
        fn setup_next(&self, _sender: &AnyObject) {
            with_state(|s| {
                let Some(advance) = s.setup.as_mut().map(|w| w.advance()) else { return };
                s.after_setup_advance(advance);
            });
        }

        /// Setup ▸ Coding Agents ▸ Leave as Is: the next step; nothing is touched.
        #[unsafe(method(setupLeaveHarnesses:))]
        fn setup_leave_harnesses(&self, _sender: &AnyObject) {
            with_state(|s| {
                let Some(advance) = s.setup.as_mut().map(|w| w.leave_harnesses()) else { return };
                s.after_setup_advance(advance);
            });
        }

        /// Setup ▸ How should I sound? ▸ a voice: Phonon S with that preset, as Speaking's menu chooses it.
        #[unsafe(method(setupVoice:))]
        fn setup_voice(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some(action) = s.setup.as_ref().and_then(|w| w.voice_action(index)) else { return };
                speaking_menu::choose(s, action);
                let voice = s.speech_voice.clone();
                if let Some(w) = s.setup.as_mut() { w.set_voice(&voice); }
            });
        }

        /// Setup ▸ Customise me ▸ a mark: applied at once, as Personalise's grid applies it.
        #[unsafe(method(setupMark:))]
        fn setup_mark(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                let Some(UiAction::Identity(action)) = s.setup.as_ref().and_then(|w| w.mark_action(index)) else { return };
                personalise::apply(s, action);
                s.sync_setup();
            });
        }

        /// A tile that only shows the choice (listening's This Mac): nothing to do.
        #[unsafe(method(setupNothing:))]
        fn setup_nothing(&self, _sender: &AnyObject) {}

        /// Setup ▸ How should I listen? ▸ the decider's precision: chosen; Continue applies it.
        #[unsafe(method(setupDecider:))]
        fn setup_decider(&self, sender: &objc2_app_kit::NSSegmentedControl) {
            let segment = sender.selectedSegment();
            with_state(move |s| {
                let Some(w) = s.setup.as_mut() else { return };
                match w.decider_option_at(segment) {
                    Some(index) => w.choose_decider(index),
                    None => w.choose_decider(usize::MAX),
                }
            });
        }

        #[unsafe(method(setupBack:))]
        fn setup_back(&self, _sender: &AnyObject) {
            with_state(|s| if let Some(w) = s.setup.as_mut() { w.back() });
        }

        /// Home ▸ Choose…: a folder picker on the Setup window (folders only; New Folder allowed).
        #[unsafe(method(setupChooseHome:))]
        fn setup_choose_home(&self, _sender: &AnyObject) {
            with_state(|s| {
                let Some(window) = s.setup.as_ref().map(|w| w.window.clone()) else { return };
                if !s.live() { return; }
                let panel = objc2_app_kit::NSOpenPanel::openPanel(s.mtm);
                panel.setCanChooseDirectories(true);
                panel.setCanChooseFiles(false);
                panel.setCanCreateDirectories(true);
                panel.setAllowsMultipleSelection(false);
                panel.setPrompt(Some(&util::ns("Choose")));
                let home = s.setup_home.home.to_string_lossy().into_owned();
                let url = objc2_foundation::NSURL::fileURLWithPath(&util::ns(&home));
                panel.setDirectoryURL(Some(&url));
                let chosen = panel.clone();
                let handler = block2::RcBlock::new(move |response: objc2_app_kit::NSModalResponse| {
                    if response != objc2_app_kit::NSModalResponseOK { return; }
                    let Some(path) = chosen.URL().and_then(|u| u.path()) else { return };
                    let path = std::path::PathBuf::from(path.to_string());
                    with_state(move |s| s.emit(UiAction::ChooseHome(path)));
                });
                panel.beginSheetModalForWindow_completionHandler(&window, &handler);
            });
        }

        #[unsafe(method(setupConfirmMove:))]
        fn setup_confirm_move(&self, _sender: &AnyObject) {
            with_state(|s| s.emit(UiAction::ConfirmHomeMove));
        }

        /// Providers ▸ a row: shown as chosen, then what the Add Provider menu row of that name does: the engine on,
        /// the custom kinds' choice (a menu under the row, then the connect panel), Claude Code subscription, or the panel.
        #[unsafe(method(setupProvider:))]
        fn setup_provider(&self, sender: &NSButton) {
            let Some(choice) = ProviderChoice::ALL.get(sender.tag() as usize).copied() else { return };
            let kinds = with_state_now(move |s| {
                if let Some(w) = s.setup.as_mut() { w.choose_provider(choice); }
                match choice.menu_action() {
                    ProviderAction::Engine => s.emit(UiAction::SetEngine(true)),
                    ProviderAction::ClaudeViaT3 => s.emit(UiAction::ClaudeViaT3),
                    ProviderAction::Connect(id) => {
                        if let Some(index) = s.api.providers.iter().position(|p| p.id == id) { s.show_sign_in_panel(index) }
                    }
                    ProviderAction::CustomKinds => {
                        let target = s.target();
                        return s.live().then(|| provider_rows::custom_kinds_menu(&s.api, false, Some(&target), s.mtm)).flatten();
                    }
                }
                None
            }).flatten();
            // The kinds pop up under the row, outside the state borrow (the menu's tracking blocks until it closes).
            if let Some(menu) = kinds {
                let height = sender.bounds().size.height;
                menu.popUpMenuPositioningItem_atLocation_inView(None, objc2_foundation::NSPoint::new(0.0, height + 4.0), Some(sender));
            }
        }

        /// T3 Code's button: Connect (the only way Heimdall connects in the public edition), Install, or after a
        /// failed install the download page.
        #[unsafe(method(setupT3:))]
        fn setup_t3(&self, _sender: &AnyObject) {
            with_state(|s| {
                let t3 = s.setup_t3.clone();
                if t3.ready() {
                    s.emit(UiAction::ConnectT3);
                } else if matches!(t3.install, T3Install::Failed(_)) {
                    s.open_url(T3_DOWNLOAD_PAGE);
                } else {
                    s.emit(UiAction::InstallT3);
                }
            });
        }

        /// Vella's tiles: Add Vella (the default) or Not now; Continue acts on the choice.
        #[unsafe(method(setupVella:))]
        fn setup_vella(&self, sender: &NSButton) {
            let add = sender.tag() == 0;
            with_state(move |s| {
                if let Some(w) = s.setup.as_mut() { w.choose_vella(add) }
            });
        }

        /// The menu's Home Folder…: Setup at its home step.
        #[unsafe(method(openHomeFolder:))]
        fn open_home_folder(&self, _sender: &AnyObject) {
            with_state(|s| s.show_setup_at(Some(SetupStep::Home)));
        }

        #[unsafe(method(setupOpenSettings:))]
        fn setup_open_settings(&self, sender: &NSButton) {
            let index = sender.tag() as usize;
            with_state(move |s| {
                if s.options.fake_permissions {
                    return;
                }
                if let Some(permission) = Permission::ALL.get(index) {
                    permissions::open_system_settings(*permission);
                }
            });
        }

        #[unsafe(method(setupPoll:))]
        fn setup_poll(&self, _timer: &NSTimer) {
            with_state(|s| s.poll_permissions());
        }


        /// [chrome] Support the Developer: the website, in the default browser.
        #[unsafe(method(supportDeveloper:))]
        fn support_developer(&self, _sender: &AnyObject) {
            if let Some(url) = objc2_foundation::NSURL::URLWithString(&util::ns(menu::SUPPORT_URL)) {
                objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }

        #[unsafe(method(showArchive:))]
        fn show_archive(&self, _sender: &AnyObject) {
            with_state(|s| s.emit(UiAction::ShowArchive));
        }

        #[unsafe(method(renderStep:))]
        fn render_step(&self, _timer: &NSTimer) {
            render::step();
        }
    }
);

impl Controller {
    pub(crate) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}


/// A tile's click or accessibility press (main thread, inside the menu's tracking loop).
/// The menu-bar item's tooltip: the assistant's name and what it is doing ("Heimdall · listening").
pub(crate) fn status_tooltip(name: &str, listen: &ListenState, alerting: bool) -> String {
    let what = match listen {
        _ if alerting => "needs attention",
        ListenState::Listening => "listening",
        ListenState::Paused => "off",
        ListenState::NeedsSetup => "setup needed",
        ListenState::Failed(_) => "stopped",
    };
    format!("{name} · {what}")
}

/// Whether one of Heimdall's menus is on screen (its window is visible).
fn menu_on_screen(mtm: MainThreadMarker) -> bool {
    NSApplication::sharedApplication(mtm).windows().iter().any(|w| w.isVisible() && w.class().name().to_string_lossy().contains("Menu"))
}

pub(crate) fn tile_clicked(index: usize) {
    let Some(tile) = Tile::ALL.get(index).copied() else { return };
    with_state(move |s| s.toggle_tile(tile));
}

/// The status line's Set Up…: the menu closes and Setup opens, as the native row did.
pub(crate) fn status_clicked() {
    // [chrome] The line also retries settings that did not apply (the old Reload row's job).
    with_state(|s| {
        // Only Set Up… leaves the menu (Setup is a window); a retry happens in place.
        match menu::status_click(s) {
            menu::StatusClick::Setup => {
                s.menu.cancelTracking();
                DispatchQueue::main().exec_async(|| with_state(|s| s.show_setup()))
            }
            menu::StatusClick::Retry => s.emit(UiAction::Reload),
            menu::StatusClick::Nothing => {}
        }
    });
}

/// [chrome] A double-click on an unidentified speaker in the transcript: ask who it is.
pub(crate) fn transcript_name_requested(span: transcript::SpeakerSpan) {
    with_state(move |s| {
        let (mtm, target) = (s.mtm, s.target());
        s.transcript_window().begin_naming(span, &target, mtm);
    });
}

/// A `CheckRow`'s click or accessibility press.
pub(crate) fn check_row_clicked(row: usize) {
    if row == menu::LOGIN_ROW {
        with_state(|s| s.toggle_launch_at_login());
    } else if row == menu::WAKE_SOUND_ROW {
        with_state(|s| s.toggle_wake_sound());
    } else if let Some(task) = menu::background_task_from_row(row) {
        with_state(move |s| s.toggle_background_task(task));
    } else if row == menu::ISLANDS_SHOWN_ROW {
        with_state(|s| s.toggle_islands_shown());
    } else if row == menu::ISLANDS_ON_STREAMS_ROW {
        with_state(|s| s.toggle_islands_on_streams());
    } else if let Some(section) = menu::section_from_row(row) {
        with_state(move |s| s.toggle_section(section));
    } else if let Some(picker) = menu::Picker::from_row(row) {
        with_state(move |s| s.toggle_add_provider(picker));
    }
}

/// Speaking ▸ Speed released (or stepped by accessibility): keep it and tell the app, which saves it and plays a
/// sample at that speed.
pub(crate) fn voice_speed_released(percent: u16) {
    with_state(move |s| {
        s.settings_value.voice_speed_percent = percent;
        s.emit(UiAction::SetVoiceSpeed(percent));
    });
}

/// Emits an action from a completion handler that may run on any thread.
fn post_action(action: UiAction) {
    DispatchQueue::main().exec_async(move || with_state(move |s| s.emit(action)));
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Local "14:05".
fn clock_now() -> String {
    let date = objc2_foundation::NSDate::now();
    let formatter = objc2_foundation::NSDateFormatter::new();
    formatter.setDateFormat(Some(&util::ns("HH:mm")));
    formatter.stringFromDate(&date).to_string()
}

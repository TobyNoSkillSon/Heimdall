//! Agent ▸ Model's click-then-confirm: "Whenever I want to switch a model or a provider, I
//! click on it and the row uncollapses, and then I have to click Confirm. The Confirm button should be bouncing a
//! little bit, or glowing a little bit, so I know I have to click it, and only then does it actually switch."
//!
//! A click on a row that is not the agent's choice makes it the pending choice: its radio takes the check, its details
//! open and a Confirm pill glows under them. Nothing reaches the app until Confirm; closing the menu drops the pending
//! choice. A click on the current choice only opens or closes its details. The relay (Claude Code subscription) is one more row
//! with its T3 models under its disclosure, then its
//! accounts: Automatic and each T3 Claude instance. This Mac offers only the local engine itself, on whatever model the Engine
//! section has loaded (30 Oct): picking a local model or precision is the Engine section's, never the agent's.

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_app_kit::NSAttributedStringNSStringDrawing as _;
use objc2_app_kit::{NSColor, NSEvent, NSMenu, NSMenuItem, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
use std::cell::Cell;

use super::util::*;
use crate::{ApiProviders, Settings, UiAction};

/// What the agent runs, or what a click proposes it run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    /// The built-in engine, on whatever model the Engine section has loaded.
    Local,
    /// An added API model.
    Api { provider: String, id: String },
    /// The Claude relay in T3 on a model (`None`: the first T3 lists).
    Relay(Option<String>),
    /// The Claude relay on a T3 Claude instance (`None`: Automatic); it keeps its model.
    RelayAccount(Option<String>),
}

/// The agent's choice as the app last said it.
pub(crate) fn current(api: &ApiProviders, settings: &Settings) -> Option<Choice> {
    if settings.agent_relay { return Some(Choice::Relay(settings.agent_relay_model.clone())); }
    if let Some((provider, id)) = &api.selected { return Some(Choice::Api { provider: provider.clone(), id: id.clone() }); }
    api.local_chosen().then_some(Choice::Local)
}

/// What an account row is compared with: the relay's account while it runs.
pub(crate) fn current_account(settings: &Settings) -> Option<Choice> {
    settings.agent_relay.then(|| Choice::RelayAccount(settings.agent_relay_account.clone()))
}

/// A click on a row: the pending choice it leaves. Clicking what runs clears it (nothing to confirm); a relay model
/// row that is the relay's default counts as the relay as it runs.
pub(crate) fn click(current: Option<&Choice>, clicked: Choice, relay_default: Option<&str>) -> Option<Choice> {
    let same = |a: &Choice, b: &Choice| match (a, b) {
        (Choice::Relay(x), Choice::Relay(y)) => x.as_deref().or(relay_default) == y.as_deref().or(relay_default),
        _ => a == b,
    };
    if current.is_some_and(|c| same(c, &clicked)) { None } else { Some(clicked) }
}

/// What Confirm sends: one action, the agent's choice and nothing else (the local engine's choice never touches the
/// engine).
pub(crate) fn actions(choice: &Choice) -> Vec<UiAction> {
    match choice {
        Choice::Local => vec![UiAction::SelectLocalAgent],
        Choice::Api { provider, id } => vec![UiAction::SelectApiModel { provider: provider.clone(), id: id.clone() }],
        Choice::Relay(Some(model)) => vec![UiAction::SelectRelayModel(model.clone())],
        Choice::Relay(None) => vec![UiAction::SelectAgentRelay],
        Choice::RelayAccount(account) => vec![UiAction::SelectRelayAccount(account.clone())],
    }
}

/// The relay's row in the providers' selection: no model row matches it.
pub(crate) const RELAY_KEY: (&str, &str) = ("t3-relay", "claude");

/// The providers as Agent ▸ Model draws them: the pending choice (else the current one) selected, whether the
/// built-in engine is added and whether the relay is the agent (both then leave Add Provider).
pub(crate) fn drawn(api: &ApiProviders, settings: &Settings, pending: Option<&Choice>) -> ApiProviders {
    let mut api = api.clone();
    api.engine_added = settings.engine_added;
    api.relay_on = settings.agent_relay;
    api.relay_added = settings.relay_added || settings.agent_relay;
    match pending.cloned().or_else(|| current(&api, settings)) {
        Some(Choice::Local) => { api.selected = None; api.unchosen = false; }
        Some(Choice::Api { provider, id }) => api.selected = Some((provider, id)),
        Some(Choice::Relay(_) | Choice::RelayAccount(_)) => api.selected = Some((RELAY_KEY.0.into(), RELAY_KEY.1.into())),
        None => {}
    }
    api
}

/// The relay model whose radio is on: the pending one, else the one the relay runs (or T3's first).
pub(crate) fn relay_model_on<'a>(pending: Option<&'a Choice>, settings: &'a Settings, first: Option<&'a str>) -> Option<&'a str> {
    match pending {
        Some(Choice::Relay(m)) => m.as_deref().or(first),
        Some(Choice::RelayAccount(_)) => settings.agent_relay_model.as_deref().or(first),
        Some(_) => None,
        None if settings.agent_relay => settings.agent_relay_model.as_deref().or(first),
        None => None,
    }
}

/// The relay account whose radio is on (`Some(None)`: Automatic): the pending one, else the relay's own; `None` when
/// no account row is on (another row pending, or the relay off).
pub(crate) fn relay_account_on<'a>(pending: Option<&'a Choice>, settings: &'a Settings) -> Option<Option<&'a str>> {
    match pending {
        Some(Choice::RelayAccount(a)) => Some(a.as_deref()),
        Some(Choice::Relay(_)) => Some(settings.agent_relay_account.as_deref()),
        Some(_) => None,
        None => settings.agent_relay.then_some(settings.agent_relay_account.as_deref()),
    }
}

/// The Confirm pill's measures: its height, the glow's colour strength at its peak and its blur.
const PILL_HEIGHT: f64 = 22.0;
const GLOW_PEAK: f64 = 0.75;
const GLOW_LOW: f64 = 0.15;
const GLOW_BLUR: f64 = 7.0;
/// One breath of the glow, in and out.
pub(crate) const GLOW_PERIOD_S: f64 = 2.2;
pub(crate) const CONFIRM: &str = "Confirm";

pub(crate) struct ConfirmIvars {
    /// Offscreen renders: the glow drawn at this level (0 to 1) into the view itself, since a capture does not see
    /// the live layer's shadow. `None` live.
    still: Cell<Option<f64>>,
    /// The Decider section's Confirm (its pending precision) rather than Agent ▸ Model's.
    decider: Cell<bool>,
}

define_class!(
    /// The Confirm pill under a pending choice: the accent fill with white ink, like an on tile, breathing a soft accent
    /// glow until it is pressed or the menu closes. It acts on press (an open menu swallows the first release).
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallConfirmPill"]
    #[ivars = ConfirmIvars]
    pub(crate) struct ConfirmView;

    impl ConfirmView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool { true }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) { self.draw(); }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) { confirm(self.ivars().decider.get()); }

        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool { true }

        #[unsafe(method(accessibilityRole))]
        fn accessibility_role(&self) -> *mut objc2_foundation::NSString {
            Retained::autorelease_return(ns("AXButton"))
        }

        #[unsafe(method(accessibilityLabel))]
        fn accessibility_label(&self) -> *mut objc2_foundation::NSString {
            Retained::autorelease_return(ns(CONFIRM))
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool { confirm(self.ivars().decider.get()); true }
    }
);

fn confirm(decider: bool) {
    // After the press: Confirm's own row leaves the menu with the re-lay.
    if decider {
        super::with_state_after_event(|s| s.confirm_decider());
    } else {
        super::with_state_after_event(|s| s.confirm_choice());
    }
}

impl ConfirmView {
    fn pill(&self) -> NSRect {
        let width = text_width(CONFIRM, &menu_font()) + 28.0;
        let b = self.bounds();
        NSRect::new(NSPoint::new(b.size.width - 12.0 - width, (b.size.height - PILL_HEIGHT) / 2.0), NSSize::new(width, PILL_HEIGHT))
    }

    fn draw(&self) {
        use objc2_app_kit::{NSBezierPath, NSShadow};
        let pill = self.pill();
        let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, PILL_HEIGHT / 2.0, PILL_HEIGHT / 2.0);
        let accent = NSColor::controlAccentColor();
        if let Some(level) = self.ivars().still.get() {
            let glow = NSShadow::new();
            glow.setShadowColor(Some(&accent.colorWithAlphaComponent(GLOW_LOW + (GLOW_PEAK - GLOW_LOW) * level)));
            glow.setShadowBlurRadius(GLOW_BLUR);
            glow.setShadowOffset(NSSize::new(0.0, 0.0));
            objc2_app_kit::NSGraphicsContext::saveGraphicsState_class();
            glow.set();
            accent.setFill();
            path.fill();
            objc2_app_kit::NSGraphicsContext::restoreGraphicsState_class();
        }
        accent.setFill();
        path.fill();
        let font = objc2_app_kit::NSFont::systemFontOfSize_weight(menu_font().pointSize(), unsafe { objc2_app_kit::NSFontWeightSemibold });
        let text = attributed(CONFIRM, &font, &NSColor::whiteColor());
        let size = text.size();
        text.drawAtPoint(NSPoint::new((pill.origin.x + (pill.size.width - size.width) / 2.0).round(), (pill.origin.y + (PILL_HEIGHT - size.height) / 2.0).round()));
    }

    /// Offscreen renders: draw the glow at `level` (0 to 1) into the view.
    pub(crate) fn set_still(&self, level: f64) {
        self.ivars().still.set(Some(level.clamp(0.0, 1.0)));
        self.setNeedsDisplay(true);
    }
}

/// The live glow: the layer's accent shadow breathing between low and peak (render server, nothing redrawn).
fn breathe(view: &ConfirmView) {
    use objc2_quartz_core::{CABasicAnimation, CAMediaTiming, CAMediaTimingFunction};
    view.setWantsLayer(true);
    let Some(layer) = view.layer() else { return };
    layer.setMasksToBounds(false);
    layer.setShadowColor(Some(&NSColor::controlAccentColor().CGColor()));
    layer.setShadowRadius(GLOW_BLUR / 2.0);
    layer.setShadowOffset(objc2_core_foundation::CGSize::new(0.0, 0.0));
    layer.setShadowOpacity(GLOW_LOW as f32);
    let animation = CABasicAnimation::animationWithKeyPath(Some(&ns("shadowOpacity")));
    unsafe {
        animation.setFromValue(Some(&objc2_foundation::NSNumber::new_f64(GLOW_LOW)));
        animation.setToValue(Some(&objc2_foundation::NSNumber::new_f64(GLOW_PEAK)));
        animation.setTimingFunction(Some(&CAMediaTimingFunction::functionWithName(objc2_quartz_core::kCAMediaTimingFunctionEaseInEaseOut)));
    }
    animation.setDuration(GLOW_PERIOD_S / 2.0);
    animation.setAutoreverses(true);
    animation.setRepeatCount(f32::INFINITY);
    layer.addAnimation_forKey(&animation, Some(&ns("confirm-glow")));
}

/// The Confirm row at `width`: the pill at the trailing edge, glowing.
pub(crate) fn confirm_item(width: f64, mtm: MainThreadMarker) -> (Retained<NSMenuItem>, Retained<ConfirmView>) {
    confirm_item_for(width, false, mtm)
}

/// The same pill for the Decider section's pending precision (`decider_section`).
pub(crate) fn confirm_item_for(width: f64, decider: bool, mtm: MainThreadMarker) -> (Retained<NSMenuItem>, Retained<ConfirmView>) {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&ns(CONFIRM));
    let view: Retained<ConfirmView> = {
        let this = ConfirmView::alloc(mtm).set_ivars(ConfirmIvars { still: Cell::new(None), decider: Cell::new(decider) });
        unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, width, 32.0)] }
    };
    view.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
    view.setToolTip(Some(&ns(CONFIRM)));
    breathe(&view);
    item.setView(Some(&view));
    (item, view)
}

/// Puts the Confirm row into `menu` right after `anchor` (the pending row, or its last detail row).
pub(crate) fn insert_after(menu: &NSMenu, anchor: &NSMenuItem, item: &NSMenuItem) {
    let at = menu.indexOfItem(anchor);
    if at >= 0 { menu.insertItem_atIndex(item, at + 1); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(selected: Option<(&str, &str)>) -> ApiProviders {
        ApiProviders { selected: selected.map(|(p, i)| (p.into(), i.into())), ..Default::default() }
    }

    #[test]
    fn a_click_on_another_row_waits_for_confirm_and_a_click_on_the_choice_does_not() {
        let opus = Choice::Api { provider: "anthropic".into(), id: "claude-opus-5-5".into() };
        let luna = Choice::Api { provider: "openai".into(), id: "gpt-6-luna".into() };
        assert_eq!(click(Some(&luna), opus.clone(), None), Some(opus.clone()));
        assert_eq!(click(Some(&luna), luna.clone(), None), None);
        // The relay on T3's first model: its row and that model's row are the choice already.
        let relay = Choice::Relay(None);
        assert_eq!(click(Some(&relay), Choice::Relay(Some("claude-opus-5-5".into())), Some("claude-opus-5-5")), None);
        assert_eq!(click(Some(&relay), Choice::Relay(Some("claude-sonnet-5-5".into())), Some("claude-opus-5-5")), Some(Choice::Relay(Some("claude-sonnet-5-5".into()))));
    }

    /// 28 Oct 17:26: the user clicked Luna, then Claude Code subscription, and Luna loaded. Each confirmed choice sends exactly what
    /// was clicked: the relay's own action, never the model clicked before it.
    #[test]
    fn confirm_sends_what_was_clicked_back_and_forth() {
        let settings = Settings::default();
        let luna = Choice::Api { provider: "openai".into(), id: "gpt-6-luna".into() };
        let mut now = current(&api(Some(("openai", "gpt-6-luna"))), &settings);
        assert_eq!(now, Some(luna.clone()));
        let pending = click(now.as_ref(), Choice::Relay(Some("claude-opus-5-5".into())), None).unwrap();
        assert_eq!(actions(&pending), [UiAction::SelectRelayModel("claude-opus-5-5".into())]);
        now = Some(pending);
        let back = click(now.as_ref(), luna.clone(), None).unwrap();
        assert_eq!(actions(&back), [UiAction::SelectApiModel { provider: "openai".into(), id: "gpt-6-luna".into() }]);
        // This Mac: the agent on the local engine, one action; the engine's model is the Engine section's.
        assert_eq!(actions(&Choice::Local), [UiAction::SelectLocalAgent]);
    }

    #[test]
    fn the_menu_draws_the_pending_choice_and_the_relay_reads_as_its_own_row() {
        let settings = Settings { agent_relay: true, agent_relay_model: Some("claude-sonnet-5-5".into()), ..Default::default() };
        let base = api(Some(("openai", "gpt-6-luna")));
        assert_eq!(current(&base, &settings), Some(Choice::Relay(Some("claude-sonnet-5-5".into()))));
        assert_eq!(drawn(&base, &settings, None).selected, Some((RELAY_KEY.0.into(), RELAY_KEY.1.into())), "the relay is checked, Luna is not");
        let pending = Choice::Api { provider: "anthropic".into(), id: "claude-opus-5-5".into() };
        assert_eq!(drawn(&base, &settings, Some(&pending)).selected, Some(("anthropic".into(), "claude-opus-5-5".into())));
        assert!(drawn(&base, &settings, Some(&Choice::Local)).local_chosen());
        assert!(drawn(&base, &settings, None).relay_added, "the relay as the agent counts as added");
        assert_eq!(relay_model_on(None, &settings, Some("claude-opus-5-5")), Some("claude-sonnet-5-5"));
        assert_eq!(relay_model_on(Some(&pending), &settings, Some("claude-opus-5-5")), None);
        assert_eq!(relay_model_on(Some(&Choice::Relay(None)), &Settings::default(), Some("claude-opus-5-5")), Some("claude-opus-5-5"));
    }

    /// Agent ▸ Model ▸ Claude Code subscription ▸ an account: Automatic or a T3 Claude instance, click then
    /// Confirm; the relay keeps its model, and the account it runs on is the choice already.
    #[test]
    fn a_relay_account_is_clicked_then_confirmed_and_keeps_the_model() {
        let second = Some("claudeAgent_claude_code_2".to_string());
        let settings = Settings { agent_relay: true, agent_relay_model: Some("claude-sonnet-5-5".into()), ..Default::default() };
        assert_eq!(current_account(&settings), Some(Choice::RelayAccount(None)));
        assert_eq!(current_account(&Settings::default()), None, "the relay off: every account row proposes it");
        assert_eq!(click(current_account(&settings).as_ref(), Choice::RelayAccount(None), None), None, "Automatic runs already");
        let pending = click(current_account(&settings).as_ref(), Choice::RelayAccount(second.clone()), None).unwrap();
        assert_eq!(actions(&pending), [UiAction::SelectRelayAccount(second.clone())]);
        assert_eq!(relay_account_on(Some(&pending), &settings), Some(second.as_deref()), "the pending account's radio");
        assert_eq!(relay_model_on(Some(&pending), &settings, Some("claude-opus-5-5")), Some("claude-sonnet-5-5"), "the model stays checked");
        assert_eq!(drawn(&api(Some(("openai", "gpt-6-luna"))), &Settings::default(), Some(&pending)).selected, Some((RELAY_KEY.0.into(), RELAY_KEY.1.into())));
        assert_eq!(relay_account_on(None, &settings), Some(None), "Automatic is checked while the relay runs");
        assert_eq!(relay_account_on(None, &Settings::default()), None, "no account checked while it is off");
        assert_eq!(relay_account_on(Some(&Choice::Api { provider: "openai".into(), id: "gpt-6-luna".into() }), &settings), None);
        let back = click(current_account(&Settings { agent_relay_account: second.clone(), ..settings.clone() }).as_ref(), Choice::RelayAccount(None), None).unwrap();
        assert_eq!(actions(&back), [UiAction::SelectRelayAccount(None)], "back to Automatic");
    }
}

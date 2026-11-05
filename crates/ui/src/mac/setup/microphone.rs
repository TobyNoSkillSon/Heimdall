//! Can you hear me? The microphone, required (5 Nov): a card with the input in use (macOS's default input
//! until another is chosen), a level meter that moves while sound comes in from it, and a pop-up to choose another,
//! applied through the menu's own microphone choice (`UiAction::SelectMicrophone`). No input at all reads "No
//! microphone found", never a spinner.
//!
//! The meter is a short-lived capture session of the UI's own, on the input shown, only while this step is on screen
//! and the microphone is allowed; it polls the input's power and redraws only when the level moves. Nothing of it is
//! recorded, and it stops when the step is left.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{sel, MainThreadOnly};
use objc2_app_kit::{NSAutoresizingMaskOptions as Mask, NSPopUpButton, NSTextField};
use objc2_foundation::{MainThreadMarker, NSPoint};

use super::kit::*;
use super::pages::{left, Frame, CONTENT_Y, H1_Y};
use super::super::util::{ns, rect};
use super::{CARD, SIDE};
use crate::model::*;

const CARD_H: f64 = 74.0;
/// The level meter: 160 × 6.
const METER: (f64, f64) = (160.0, 6.0);

/// What the card says: (the input's name, whether there is one at all, the list the pop-up offers, its selection).
pub(crate) fn mic_card(m: &Microphones) -> (String, bool, Vec<(String, String)>, Option<usize>) {
    let inputs: Vec<(String, String)> = m.devices.iter().filter(|d| d.connected).map(|d| (d.id.clone(), d.name.clone())).collect();
    if inputs.is_empty() {
        return ("No microphone found".into(), false, inputs, None);
    }
    let at = m.in_use.as_ref().and_then(|id| inputs.iter().position(|(i, _)| i == id)).or(Some(0));
    let name = at.map(|i| inputs[i].1.clone()).unwrap_or_default();
    (name, true, inputs, at)
}

/// A level in decibels (AVFoundation's average power, about −60 to 0) as the meter's share.
pub(crate) fn level_share(db: f32) -> f64 {
    if !db.is_finite() { return 0.0; }
    (((db as f64) + 60.0) / 60.0).clamp(0.0, 1.0)
}

pub(super) struct MicPage {
    pub frame: Frame,
    pub card: Retained<Plate>,
    pub name: Retained<NSTextField>,
    pub track: Retained<Plate>,
    pub fill: Retained<Plate>,
    pub picker: Retained<NSPopUpButton>,
    /// The ids behind the pop-up's items.
    pub ids: Vec<String>,
    /// The level shown now (0 to 1).
    pub level: f64,
}

impl MicPage {
    pub(super) fn build(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let frame = Frame::scrolling(mtm);
        let doc = &frame.doc;
        heading(doc, &SetupStep::Microphone.title(""), 28.0, H1_Y, mtm);
        let card = Plate::new(rect(0.0, 0.0, CARD, CARD_H), 12.0, mtm);
        card.set_selected(false);
        model_icon(&card, "mic.fill", 16.0, (CARD_H - 40.0) / 2.0, 40.0, mtm);
        let font = semibold(15.0);
        let name = text(&card, "", &font, &t1(), 72.0, 16.0, CARD - 72.0 - 260.0, left(), mtm);
        name.setAutoresizingMask(Mask::ViewWidthSizable);
        // The meter under the name: its track, and the fill that follows the level.
        let track = Plate::new(rect(72.0, 46.0, METER.0, METER.1), 3.0, mtm);
        track.set_fill(Some(white(0.10)));
        let fill = Plate::new(rect(0.0, 0.0, 0.0, METER.1), 3.0, mtm);
        fill.set_fill(Some(hex(GREEN, 1.0)));
        track.addSubview(&fill);
        card.addSubview(&track);
        let picker = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), rect(CARD - 16.0 - 220.0, (CARD_H - 26.0) / 2.0, 220.0, 26.0), false);
        picker.setAutoresizingMask(Mask::ViewMinXMargin);
        unsafe {
            picker.setTarget(Some(target));
            picker.setAction(Some(sel!(setupMicrophone:)));
        }
        card.addSubview(&picker);
        Band::wrap(doc, &card, CONTENT_Y, CARD, SIDE, mtm);
        frame.set_height(CONTENT_Y + CARD_H);
        Self { frame, card, name, track, fill, picker, ids: vec![], level: 0.0 }
    }

    pub(super) fn sync(&mut self, m: &Microphones) {
        let (name, any, inputs, at) = mic_card(m);
        self.name.setStringValue(&ns(&name));
        self.name.setTextColor(Some(&*if any { t1() } else { hex(ORANGE, 1.0) }));
        self.track.setHidden(!any);
        self.picker.setHidden(!any);
        let ids: Vec<String> = inputs.iter().map(|(i, _)| i.clone()).collect();
        if ids != self.ids || self.picker.numberOfItems() as usize != inputs.len() {
            self.picker.removeAllItems();
            for (_, n) in &inputs {
                self.picker.addItemWithTitle(&ns(n));
            }
            self.ids = ids;
        }
        if let Some(i) = at { self.picker.selectItemAtIndex(i as isize); }
        // The name sits centred when there is no meter under it.
        let f = self.name.frame();
        self.name.setFrameOrigin(NSPoint::new(f.origin.x, if any { 16.0 } else { ((CARD_H - f.size.height) / 2.0).round() }));
        if !any { self.set_level(0.0); }
    }

    /// The meter at `share` (0 to 1); a change too small to see draws nothing.
    pub(super) fn set_level(&mut self, share: f64) {
        if (share - self.level).abs() < 0.02 && !(share == 0.0 && self.level != 0.0) { return; }
        self.level = share;
        self.fill.setFrame(rect(0.0, 0.0, (METER.0 * share).round(), METER.1));
    }

    /// The pop-up's selected input.
    pub(super) fn picked(&self) -> Option<String> {
        usize::try_from(self.picker.indexOfSelectedItem()).ok().and_then(|i| self.ids.get(i).cloned())
    }
}

/// The meter's capture session on one input: started on its own queue (`startRunning` blocks), polled on the main
/// thread, stopped when dropped.
pub(super) struct Meter {
    pub uid: String,
    session: Retained<objc2_av_foundation::AVCaptureSession>,
    output: Retained<objc2_av_foundation::AVCaptureAudioDataOutput>,
    timer: Retained<objc2_foundation::NSTimer>,
}

impl Meter {
    /// A session on `uid`, polled every 50 ms by `tick` (the Setup window's `mic_tick`). `None`: the device is gone or
    /// cannot be opened (its row on Getting ready says so).
    pub(super) fn start(uid: &str) -> Option<Self> {
        use objc2_av_foundation::{AVCaptureAudioDataOutput, AVCaptureDevice, AVCaptureDeviceInput, AVCaptureSession};
        unsafe {
            let device = AVCaptureDevice::deviceWithUniqueID(&ns(uid))?;
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device).ok()?;
            let session = AVCaptureSession::new();
            if !session.canAddInput(&input) { return None; }
            session.addInput(&input);
            let output = AVCaptureAudioDataOutput::new();
            if !session.canAddOutput(&output) { return None; }
            session.addOutput(&output);
            let running = super::super::MainOnly(session.clone());
            dispatch2::DispatchQueue::global_queue(dispatch2::GlobalQueueIdentifier::QualityOfService(dispatch2::DispatchQoS::UserInitiated))
                .exec_async(move || { let running = running; running.0.startRunning() });
            let block = block2::RcBlock::new(|_: std::ptr::NonNull<objc2_foundation::NSTimer>| {
                super::super::with_state(|s| if let Some(w) = s.setup.as_mut() { w.mic_tick() });
            });
            let timer = objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.05, true, &block);
            Some(Self { uid: uid.into(), session, output, timer })
        }
    }

    /// The input's power now, in decibels; `None` while no audio has come through yet.
    pub(super) fn power(&self) -> Option<f32> {
        unsafe {
            let connection = self.output.connections().firstObject()?;
            let channel = connection.audioChannels().firstObject()?;
            Some(channel.averagePowerLevel())
        }
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        self.timer.invalidate();
        let session = super::super::MainOnly(self.session.clone());
        dispatch2::DispatchQueue::global_queue(dispatch2::GlobalQueueIdentifier::QualityOfService(dispatch2::DispatchQoS::Utility))
            .exec_async(move || { let session = session; unsafe { session.0.stopRunning() } });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 Nov: the input in use by name, every connected input in the pop-up; none at all says so.
    #[test]
    fn wizard_the_microphone_card_names_the_input_or_says_there_is_none() {
        let mic = |id: &str, name: &str, connected| Microphone { id: id.into(), name: name.into(), connected, built_in: false };
        let m = Microphones { devices: vec![mic("a", "MacBook Pro Microphone", true), mic("b", "Shure MV7i", true), mic("c", "Old USB", false)], in_use: Some("b".into()), ..Default::default() };
        let (name, any, inputs, at) = mic_card(&m);
        assert_eq!((name.as_str(), any, inputs.len(), at), ("Shure MV7i", true, 2, Some(1)), "a saved input that is not connected is not offered");
        let none = mic_card(&Microphones::default());
        assert_eq!((none.0.as_str(), none.1), ("No microphone found", false));
        assert_eq!(level_share(-60.0), 0.0);
        assert_eq!(level_share(0.0), 1.0);
        assert!((level_share(-30.0) - 0.5).abs() < 1e-9);
        assert_eq!(level_share(f32::NEG_INFINITY), 0.0, "silence");
    }
}

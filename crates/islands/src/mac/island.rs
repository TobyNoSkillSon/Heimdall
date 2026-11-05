//! One island: a borderless, non-activating panel holding one black shape. The shape and everything in it move on
//! Core Animation springs in the render server; the main thread only sets targets. At rest nothing runs: no loops, no
//! breathing; things move only when they change, are carried, open or close (Swift `IslandController.swift`).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, DefinedClass, Message};
use objc2_app_kit::{NSColor, NSEvent, NSEventPhase, NSScreen, NSView, NSWindowOrderingMode};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{MainThreadMarker, NSArray, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CAAnimation, CAGradientLayer, CALayer, CAMediaTiming, CAShapeLayer, CATextLayer, CATransaction, CATransition};
use serde_json::Value;

use super::app::{app, App};
use super::confirm::{self, Hit};
use super::draw;
use super::look::*;
use super::overlay::OverlaySpace;
use super::panel::{FlippedView, IslandPanel, IslandView};
use super::parts::{build_body, file_dates, Body, Control};
use crate::model::glance::Series;
use super::sys::{self, Repeat, Work1};
use super::viewer::FileViewer;
use crate::model::due::Cal;
use crate::model::geometry::{Point, Rect, Size, Vector};
use crate::model::{j, Attention, IslandFile, ItemDiff, Json, OpenRoute, OpenTarget, Paths, Plan, PlanList, Responses, ViewerKind};
use crate::model::{Island, Presets};
use crate::model::toggle::timers;

pub const CLOSE_ROOM: f64 = 30.0;
pub const WORK_INTERVAL: f64 = 4.0;

thread_local! {
    /// Lab renders: windows are built but never ordered on screen.
    pub static OFFSCREEN: Cell<bool> = const { Cell::new(false) };
    /// Lab: called right after each attention pulse starts.
    pub static LAB_PULSE: RefCell<Option<Box<dyn Fn(&Rc<Ctl>)>>> = const { RefCell::new(None) };
}

/// Lab tracing (FI_DEBUG=1).
pub fn debug(s: &str) {
    if std::env::var_os("FI_DEBUG").is_some() {
        eprintln!("[{:.3}] {s}", sys::uptime());
    }
}

thread_local! {
    /// A row or chip click has no picture: a snapshot of the clicked element, by the viewer's file path, for the
    /// viewer's first frame (review, 30 Oct: it started as a blank pill over the chip).
    static SNAPSHOTS: RefCell<HashMap<String, draw::Image>> = RefCell::new(HashMap::new());
}

pub fn offscreen() -> bool {
    OFFSCREEN.with(|o| o.get())
}

/// Where an island's window is on screen now (lab checks: at rest it is exactly the shape).
pub fn rect_of_window(c: &Ctl) -> Rect {
    rect_from(c.panel.frame())
}

/// The system's Reduce Motion setting (Accessibility ▸ Display), read at each hide or show. Lab renders may set
/// FLOATING_ISLANDS_REDUCE_MOTION=1 to see the plain fade.
pub fn reduce_motion() -> bool {
    if offscreen() && std::env::var_os("FLOATING_ISLANDS_REDUCE_MOTION").is_some() {
        return true;
    }
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("main thread")
}

pub struct St {
    pub island: Island,
    file_viewer: Option<Rc<FileViewer>>,
    /// The open viewer's file watch (only while it is open).
    file_watch: Option<Rc<super::filewatch::FileWatch>>,
    outside_monitor: Option<Retained<AnyObject>>,
    close_layer: Option<Retained<CALayer>>,
    /// An open viewer's drag handle (`share.rs`): its file's icon left of the close button.
    share: Option<Rc<super::share::Handle>>,
    icon: Option<Retained<CALayer>>,
    ring: Option<Retained<CAShapeLayer>>,
    spinner: Option<Retained<CAShapeLayer>>,
    pulse: Option<(Retained<CALayer>, Retained<CAAnimation>)>,
    title_layer: Option<Retained<CATextLayer>>,
    trailing_layer: Option<Retained<CATextLayer>>,
    trailing_in_pill: bool,
    body: Option<Body>,
    /// The numbers the island showed over its rewrites, for its live graphics (`model::glance`).
    series: Series,
    clock_timer: Option<sys::Ticker>,
    pub anchor: Point,
    screen_frame: Rect,
    row_top: f64,
    pub shape_rect: Rect,
    pub expanded: bool,
    generation: u64,
    placed: bool,
    pub removing: bool,
    down_screen: Option<Point>,
    down_anchor: Point,
    dragging: bool,
    down_pill_anchor: Point,
    samples: Vec<(f64, Point)>,
    pending_velocity: Option<Vector>,
    slider_drag: Option<String>,
    pressed: Option<Control>,
    open_items: HashSet<String>,
    pub attention_shown: Option<String>,
    icon_attention: Option<String>,
    seen_attention: Option<String>,
    attention_timer: Option<Repeat>,
    work_timer: Option<Repeat>,
    glow: Option<Retained<CALayer>>,
    pub pulse_count: usize,
    pub born_from: Option<Rect>,
    /// A viewer opened by a click: the island it came from (closing shrinks back into it while it is on screen), the
    /// clicked picture laid over the shape for the first frames, and the corner the motion starts from.
    pub born_island: Option<String>,
    ghost: Option<Retained<CALayer>>,
    from_radius: Option<f64>,
    /// The ghost is a clicked row's snapshot, not a picture of the content.
    row_ghost: bool,
    /// Lab (FI_LAB_SCRUB): the media time the opening motion was frozen at, so a render can draw any moment of it.
    lab_t0: Option<f64>,
    pub from_notch: bool,
    pub height_cap: f64,
    pub scroll_y: f64,
    scroll_indicator: Option<Retained<CALayer>>,
    scroll_fade: Option<Retained<CAGradientLayer>>,
    scroll_end_work: Option<Work1>,
    indicator_work: Option<Work1>,
    momentum_spent: bool,
    /// Counts rewrites: an update held for its HTML bitmaps applies only if no newer one came meanwhile.
    html_gen: u64,
    confirming: Option<(Control, confirm::Row, Work1)>,
    press_layer: Option<Retained<CALayer>>,
    /// The due labels the open body was built with (a timer redraws the list only when one changes).
    due_sig: String,
    /// On screen for news while every island is hidden (`App::flash_run`, `model/flash.rs`).
    flashed: bool,
    /// When the attention pulses began (their first comes 0.4 s later).
    attention_since: f64,
    /// Hide and show of every island (`set_screen_hidden`): which toggle is the latest, and whether it is shrinking
    /// away now (still on screen until the window goes out).
    toggle_gen: u64,
    leaving: bool,
    /// An open viewer shown as its picture for a hide or show (`freeze_viewer`): the live view comes back without a
    /// fade, the picture being the same.
    frozen: bool,
}

pub struct Ctl {
    pub id: String,
    pub seq: usize,
    pub panel: Retained<IslandPanel>,
    pub view: Retained<IslandView>,
    container: Retained<FlippedView>,
    pub root: Retained<CALayer>,
    pub shape: Retained<CALayer>,
    header: Retained<CALayer>,
    body_host: Retained<CALayer>,
    /// Holds the open content; scaled about the shape's top centre on the shape's own spring, so the content always
    /// fits the growing or shrinking shape.
    body_group: Retained<CALayer>,
    pub st: RefCell<St>,
    me: RefCell<Weak<Ctl>>,
    cal: Cal,
}

fn local(r: Rect, w: Rect) -> Rect {
    Rect::new(r.min_x() - w.min_x(), w.max_y() - r.max_y(), r.w, r.h)
}

fn ns_rect(r: Rect) -> NSRect {
    NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h))
}

fn rect_from(r: NSRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn screen_rects() -> Vec<Rect> {
    NSScreen::screens(mtm()).iter().map(|s| rect_from(s.frame())).collect()
}

fn main_screen() -> Option<Retained<NSScreen>> {
    NSScreen::mainScreen(mtm())
}

impl Ctl {
    pub fn new(island: Island, seq: usize) -> Rc<Ctl> {
        let m = mtm();
        let panel = IslandPanel::make(m);
        let view = IslandView::make(m);
        let container = FlippedView::make(m, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(28.0, 28.0)));
        let root = layer();
        root.setGeometryFlipped(true);
        let shape = layer();
        let header = layer();
        let body_host = layer();
        let body_group = layer();
        let mut series = Series::default();
        series.record(&island);
        let st = St {
            series,
            island: island.clone(),
            file_viewer: None,
            file_watch: None,
            outside_monitor: None,
            close_layer: None,
            share: None,
            icon: None,
            ring: None,
            spinner: None,
            pulse: None,
            title_layer: None,
            trailing_layer: None,
            trailing_in_pill: false,
            body: None,
            clock_timer: None,
            anchor: Point::default(),
            screen_frame: Rect::ZERO,
            row_top: f64::MAX,
            shape_rect: Rect::ZERO,
            expanded: false,
            generation: 0,
            placed: false,
            removing: false,
            down_screen: None,
            down_anchor: Point::default(),
            dragging: false,
            down_pill_anchor: Point::default(),
            samples: vec![],
            pending_velocity: None,
            slider_drag: None,
            pressed: None,
            open_items: HashSet::new(),
            attention_shown: None,
            icon_attention: None,
            seen_attention: None,
            attention_timer: None,
            work_timer: None,
            glow: None,
            pulse_count: 0,
            born_from: None,
            from_notch: false,
            born_island: None,
            ghost: None,
            from_radius: None,
            lab_t0: None,
            row_ghost: false,
            height_cap: f64::MAX,
            scroll_y: 0.0,
            scroll_indicator: None,
            scroll_fade: None,
            scroll_end_work: None,
            indicator_work: None,
            momentum_spent: false,
            html_gen: 0,
            confirming: None,
            press_layer: None,
            due_sig: String::new(),
            flashed: false,
            attention_since: 0.0,
            toggle_gen: 0,
            leaving: false,
            frozen: false,
        };
        let c = Rc::new(Ctl {
            id: island.id.clone(),
            seq,
            panel,
            view,
            container,
            root,
            shape,
            header,
            body_host,
            body_group,
            st: RefCell::new(st),
            me: RefCell::new(Weak::new()),
            cal: Cal::current(),
        });
        *c.me.borrow_mut() = Rc::downgrade(&c);
        *c.view.ivars().ctl.borrow_mut() = Rc::downgrade(&c);
        *c.panel.ivars().ctl.borrow_mut() = Rc::downgrade(&c);
        // Layer-hosting view: the root layer first, then wantsLayer.
        c.view.setLayer(Some(&c.root));
        c.view.setWantsLayer(true);
        c.container.setWantsLayer(true);
        c.view.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable);
        c.container.addSubview(&c.view);
        c.panel.setContentView(Some(&c.container));
        set_bg(&c.shape, &black());
        c.shape.setBorderColor(Some(&hairline()));
        c.shape.setBorderWidth(HAIRLINE_WIDTH);
        continuous(&c.shape);
        c.shape.setMasksToBounds(true);
        c.root.addSublayer(&c.shape);
        c.header.setAnchorPoint(pt(0.0, 0.0));
        c.shape.addSublayer(&c.header);
        c.body_host.setAnchorPoint(pt(0.0, 0.0));
        c.body_host.setOpacity(0.0);
        c.body_group.setAnchorPoint(pt(0.5, 0.0));
        c.body_group.addSublayer(&c.body_host);
        c.shape.addSublayer(&c.body_group);
        c.build_header(None);
        c.draw_html_ahead(&c.island());
        c
    }

    fn me(&self) -> Weak<Ctl> {
        self.me.borrow().clone()
    }

    // MARK: Readers

    pub fn island(&self) -> Island {
        self.st.borrow().island.clone()
    }
    pub fn expanded(&self) -> bool {
        self.st.borrow().expanded
    }
    pub fn removing(&self) -> bool {
        self.st.borrow().removing
    }
    pub fn shape_rect(&self) -> Rect {
        self.st.borrow().shape_rect
    }
    pub fn anchor(&self) -> Point {
        self.st.borrow().anchor
    }
    pub fn attention_shown(&self) -> Option<String> {
        self.st.borrow().attention_shown.clone()
    }
    pub fn pulse_count(&self) -> usize {
        self.st.borrow().pulse_count
    }
    pub fn born_from(&self) -> Option<Rect> {
        self.st.borrow().born_from
    }
    pub fn is_viewer(&self) -> bool {
        self.st.borrow().island.look() == "viewer"
    }
    pub fn set_from_notch(&self, v: bool) {
        self.st.borrow_mut().from_notch = v;
    }
    pub fn set_height_cap(&self, cap: f64) {
        let changed = {
            let mut s = self.st.borrow_mut();
            let changed = s.height_cap != cap;
            s.height_cap = cap;
            changed && s.body.is_some() && s.island.look() != "viewer"
        };
        if changed {
            self.layout_viewport();
        }
    }

    pub fn scale(&self) -> f64 {
        self.panel.screen().map(|s| s.backingScaleFactor()).or_else(|| main_screen().map(|s| s.backingScaleFactor())).unwrap_or(2.0)
    }

    // MARK: Sizes

    fn header_width(&self) -> f64 {
        let s = self.st.borrow();
        ICON_SIZE + HEADER_GAP + s.title_layer.as_ref().map(|t| t.bounds().size.width).unwrap_or(0.0)
    }

    fn trailing_width(&self) -> f64 {
        self.st.borrow().trailing_layer.as_ref().map(|t| t.bounds().size.width).unwrap_or(0.0)
    }

    pub fn pill_size(&self) -> Size {
        let in_pill = self.st.borrow().trailing_in_pill;
        let w = self.header_width() + if in_pill { self.trailing_width() + 7.0 } else { 0.0 } + 2.0 * PILL_PAD;
        Size::new(PILL_H.max(w).ceil(), PILL_H)
    }

    pub fn expanded_width(&self) -> f64 {
        let header_need = self.header_width() + 2.0 * PAD + CLOSE_ROOM;
        let i = self.island();
        let mut want = i.width().unwrap_or(EXPANDED_WIDTH);
        let parts = i.parts();
        if i.width().is_none()
            && ["note", "list"].contains(&i.look())
            && i.summary().is_none()
            && i.steps().is_empty()
            && i.items().is_empty()
            && i.facts().is_empty()
            && i.image().is_none()
            && i.open().is_none()
            && !parts.is_empty()
            && parts.iter().all(|p| j::str(p.get("type")).unwrap_or("text") == "text")
        {
            let bf = body_font();
            let widest = parts
                .iter()
                .flat_map(|p| {
                    j::str(p.get("value").or(p.get("text"))).unwrap_or("").split('\n').map(str::to_owned).collect::<Vec<_>>()
                })
                .map(|l| text_width(&l, &bf).ceil())
                .fold(0.0, f64::max);
            want = want.min((widest + 2.0 * PAD + 4.0).max(200.0));
        }
        want.max(header_need).max(self.pill_size().w).ceil()
    }

    pub fn expanded_size(&self) -> Size {
        if self.is_viewer() {
            return self.viewer_size();
        }
        let h = self.ensure_body();
        let cap = self.st.borrow().height_cap;
        Size::new(self.expanded_width(), (BODY_TOP + h + BOTTOM_PAD).ceil().min(cap.floor()))
    }

    fn viewport_height(&self) -> f64 {
        (self.expanded_size().h - BODY_TOP - BOTTOM_PAD).max(0.0)
    }

    pub fn max_scroll(&self) -> f64 {
        let bh = self.st.borrow().body.as_ref().map(|b| b.height).unwrap_or(0.0);
        (bh.ceil() - self.viewport_height()).max(0.0)
    }

    pub fn scrolls(&self) -> bool {
        !self.is_viewer() && self.max_scroll() > 0.5
    }

    pub fn size(&self) -> Size {
        if self.expanded() {
            self.expanded_size()
        } else {
            self.pill_size()
        }
    }

    /// What its dock reserves: the island as it is now (an open one pushes the rest down); an open viewer leaves the
    /// column for the middle of the screen, its slot stays a pill.
    pub fn slot_size(&self) -> Size {
        if self.is_viewer() {
            self.pill_size()
        } else {
            self.size()
        }
    }

    fn rect_for(&self, size: Size) -> Rect {
        let (sf, row_top, anchor, expanded, op) = {
            let s = self.st.borrow();
            (s.screen_frame, s.row_top, s.anchor, s.expanded, j::obj(s.island.raw.get("openPosition")).cloned())
        };
        if self.is_viewer() && expanded && !sf.is_empty() {
            if let Some(p) = op {
                if let (Some(x), Some(y)) = (j::num(p.get("x")), j::num(p.get("y"))) {
                    let r = Rect::new(sf.mid_x() + x - size.w / 2.0, sf.max_y() - y - size.h, size.w, size.h);
                    return Rect::new(
                        r.min_x().max(sf.min_x() + 8.0).min(sf.max_x() - 8.0 - r.w).round(),
                        r.min_y().max(sf.min_y() + 8.0).min(sf.max_y().min(row_top) - r.h).round(),
                        r.w,
                        r.h,
                    );
                }
            }
            return Rect::new((sf.mid_x() - size.w / 2.0).round(), (sf.mid_y() - size.h / 2.0 + 20.0).round(), size.w, size.h);
        }
        let mut r = Rect::new(anchor.x - size.w / 2.0, anchor.y - size.h, size.w, size.h);
        if !sf.is_empty() {
            r.x = r.min_x().max(sf.min_x() + 8.0).min(sf.max_x() - 8.0 - r.w);
            r.y = r.min_y().max(sf.min_y() + 8.0).min(sf.max_y().min(row_top) - r.h);
        }
        Rect::new(r.min_x().round(), r.min_y().round(), r.w, r.h)
    }

    // MARK: Header (icon · title · trailing value)

    fn build_header(&self, previous: Option<&Island>) {
        let s = self.scale();
        let island = self.island();
        let look = island.look().to_owned();
        let (has_ring, icon_attention, attention_shown) = {
            let st = self.st.borrow();
            (st.ring.is_some(), st.icon_attention.clone(), st.attention_shown.clone())
        };
        let prog_kind_same = previous.map(|p| p.progress().is_none() == island.progress().is_none());
        let parts_changed = |p: &Island| !j::opt_eq(p.raw.get("parts").filter(|v| v.is_array()), island.raw.get("parts").filter(|v| v.is_array()));
        let keep_ring = previous.map(|p| p.look()) == Some("task") && look == "task" && has_ring && prog_kind_same == Some(true);
        if keep_ring && island.progress().is_some() {
            let ring = self.st.borrow().ring.clone();
            if let (Some(ring), Some(p)) = (ring, island.progress()) {
                spring_to(&ring, "strokeEnd", any(&*num(p)), Spring::GROW);
            }
        } else if previous.is_none()
            || previous.map(|p| p.look()) != Some(look.as_str())
            || previous.map(|p| p.icon()) != Some(island.icon())
            || prog_kind_same == Some(false)
            || (look == "list" && previous.is_some_and(parts_changed))
            || (look == "todo" && previous.map(|p| p.waits_on_owner()) != Some(island.waits_on_owner()))
            || icon_attention != attention_shown
        {
            if let Some(old) = self.st.borrow().icon.clone() {
                let o2 = old.clone();
                transaction(false, Some(Box::new(move || o2.removeFromSuperlayer())), || {
                    let sc = basic("transform.scale", None, Some(any(&*num(0.3))), 0.14);
                    let op = basic("opacity", None, Some(any(&*num(0.0))), 0.14);
                    let g = group(&[&sc, &op], 0.14);
                    // SAFETY: a framework constant.
                    g.setFillMode(unsafe { objc2_quartz_core::kCAFillModeForwards });
                    g.setRemovedOnCompletion(false);
                    add(&old, &g, "replaceOut");
                });
            }
            {
                let mut st = self.st.borrow_mut();
                st.pulse = None;
                st.spinner = None;
                st.ring = None;
                st.icon_attention = attention_shown.clone();
            }
            // Only the attention changed (bell or question mark on or off): the waiting dot's arrival pulse is not
            // replayed, nothing new has arrived.
            let attention_only = previous.is_some_and(|p| {
                p.look() == look
                    && p.icon() == island.icon()
                    && (p.progress().is_none() == island.progress().is_none())
                    && icon_attention != attention_shown
                    && !(look == "list" && parts_changed(p))
                    && !(look == "todo" && p.waits_on_owner() != island.waits_on_owner())
            });
            let l = self.make_icon(&look, s);
            if attention_only {
                if let Some((halo, _)) = self.st.borrow().pulse.clone() {
                    halo.remove_anim("pulse");
                }
            }
            l.setPosition(pt(ICON_SIZE / 2.0, PILL_H / 2.0));
            self.header.addSublayer(&l);
            let placed = self.st.borrow().placed;
            self.st.borrow_mut().icon = Some(l.clone());
            if previous.is_some() || placed {
                arrive(&l, &look);
            }
        }

        let title = island.title();
        let old_title = self.st.borrow().title_layer.clone();
        if old_title.as_ref().map(|t| text_of(t).as_deref() != Some(title.as_str())).unwrap_or(true) {
            let t = text1(&title, &title_font(), &white(1.0), MAX_PILL_TITLE, s);
            t.setPosition(pt(ICON_SIZE + HEADER_GAP, (PILL_H - t.bounds().size.height) / 2.0));
            if let Some(old) = &old_title {
                crossfade(old, &t, &self.header);
            } else {
                self.header.addSublayer(&t);
            }
            self.st.borrow_mut().title_layer = Some(t);
        }
        self.header.setBounds(cgr(Rect::new(0.0, 0.0, self.header_width(), PILL_H)));
        self.refresh_working();

        let mut trailing: Option<String> = None;
        let mut in_pill = false;
        match look.as_str() {
            "task" => {
                trailing = island.progress().map(|p| format!("{}%", (p * 100.0).round() as i64)).or_else(|| island.stage());
                in_pill = trailing.is_some();
            }
            "todo" => {
                // Settled counts the last seven days' finished items (its big number); the other lists what is still
                // open. Goals counts nothing: no count of what is left (motivation, not obligation).
                let open = if island.list().as_deref() == Some("settled") {
                    let now = crate::model::time::now();
                    let raw = &island.raw;
                    crate::model::onesys::settled_count(&crate::model::onesys::settled_items(raw, now), crate::model::onesys::slate(raw), now, &self.cal).proved
                } else {
                    island.todo_items().iter().filter(|t| !t.done).count()
                };
                trailing = (open > 0 && island.list().as_deref() != Some("goals")).then(|| open.to_string());
                in_pill = trailing.is_some();
            }
            "result" => {
                trailing = island.value().or_else(|| {
                    island.parts().iter().find(|p| j::str(p.get("type")) == Some("stat")).and_then(|p| match p.get("value") {
                        Some(Value::String(s)) => Some(s.clone()),
                        v => j::num(v).map(crate::model::format_number),
                    })
                });
                in_pill = trailing.is_some();
            }
            _ => {}
        }
        self.st.borrow_mut().trailing_in_pill = in_pill;
        let old = self.st.borrow().trailing_layer.clone();
        let old_text = old.as_ref().and_then(|l| text_of(l));
        if old_text != trailing {
            if let Some(tr) = &trailing {
                let font = if island.progress().is_none() && look == "task" { title_font() } else { trailing_font() };
                let t = text1(tr, &font, &*if look == "result" { white(1.0) } else { ink2() }, 10_000.0, s);
                if let Some(old) = &old {
                    t.setPosition(old.position());
                    t.setOpacity(old.opacity());
                    crossfade(old, &t, &self.shape);
                } else {
                    t.setOpacity(0.0);
                    self.shape.addSublayer(&t);
                }
                self.st.borrow_mut().trailing_layer = Some(t);
            } else {
                if let Some(o) = &old {
                    o.removeFromSuperlayer();
                }
                self.st.borrow_mut().trailing_layer = None;
            }
        }
    }

    fn make_icon(&self, look: &str, s: f64) -> Retained<CALayer> {
        let island = self.island();
        let attention = self.st.borrow().attention_shown.clone();
        if attention.as_deref() == Some("question") {
            return sym("questionmark.circle.fill", ICON_SIZE + 1.0, &[white(1.0), blue()], s);
        }
        if attention.as_deref() == Some("waiting") {
            return sym("bell.fill", ICON_SIZE, &[orange()], s);
        }
        if look == "todo" {
            if let Some(list) = island.list() {
                let color = if list == "settled" {
                    green()
                } else if island.waits_on_owner() {
                    orange()
                } else {
                    white(1.0)
                };
                return sym(PlanList::icon(&list), ICON_SIZE, &[color], s);
            }
        }
        let picked = island.chosen().is_some() || island.parts().iter().any(|p| j::str(p.get("type")) == Some("list") && p.get("selected").is_some());
        if let Some(name) = island.icon() {
            let color = match look {
                "task" => Some(white(1.0)),
                "waiting" => Some(orange()),
                "list" => Some(if picked { white(1.0) } else { orange() }),
                "note" | "viewer" => Some(white(1.0)),
                _ => None,
            };
            if let Some(color) = color {
                if let Some(l) = icon_layer(&name, ICON_SIZE, &color, s) {
                    return l;
                }
            }
        }
        match look {
            "task" => {
                let c = layer();
                c.setBounds(cgr(Rect::new(0.0, 0.0, ICON_SIZE, ICON_SIZE)));
                let path = draw::CgPath::new();
                path.move_to(ICON_SIZE / 2.0, ICON_SIZE / 2.0 - (ICON_SIZE / 2.0 - 1.6));
                path.arc(ICON_SIZE / 2.0, ICON_SIZE / 2.0, ICON_SIZE / 2.0 - 1.6, -std::f64::consts::FRAC_PI_2, 1.5 * std::f64::consts::PI, false);
                let mk = |color: &objc2_app_kit::NSColor, cap: bool| {
                    let a = CAShapeLayer::new();
                    no_implicit(&a);
                    a.setPath(Some(path.cg()));
                    a.setLineWidth(2.8);
                    if cap {
                        // SAFETY: a framework constant.
                        a.setLineCap(unsafe { objc2_quartz_core::kCALineCapRound });
                    }
                    a.setStrokeColor(Some(&cg(color)));
                    a.setFillColor(None);
                    a.setFrame(c.bounds());
                    c.addSublayer(&a);
                    a
                };
                let _track = mk(&white(0.22), false);
                let arc = mk(&white(1.0), true);
                if let Some(p) = island.progress() {
                    arc.setStrokeEnd(p);
                    self.st.borrow_mut().ring = Some(arc);
                } else {
                    arc.setStrokeEnd(0.26);
                    self.st.borrow_mut().spinner = Some(arc);
                }
                c
            }
            "waiting" => {
                let c = layer();
                c.setBounds(cgr(Rect::new(0.0, 0.0, ICON_SIZE, ICON_SIZE)));
                let d = 11.0;
                let o = (ICON_SIZE - d) / 2.0;
                let halo = layer();
                halo.setFrame(cgr(Rect::new(o, o, d, d)));
                halo.setCornerRadius(d / 2.0);
                set_bg(&halo, &orange());
                halo.setOpacity(0.0);
                c.addSublayer(&halo);
                let dot = layer();
                dot.setFrame(cgr(Rect::new(o, o, d, d)));
                dot.setCornerRadius(d / 2.0);
                set_bg(&dot, &orange());
                c.addSublayer(&dot);
                let grow = basic("transform.scale", Some(any(&*num(1.0))), Some(any(&*num(2.3))), 1.8);
                let fade = basic("opacity", Some(any(&*num(0.55))), Some(any(&*num(0.0))), 1.8);
                let g = group(&[&grow, &fade], 1.8);
                g.setRepeatCount(2.0);
                g.setTimingFunction(Some(&ease_out()));
                add(&halo, &g, "pulse");
                self.st.borrow_mut().pulse = Some((halo, Retained::into_super(g)));
                c
            }
            "result" => sym("checkmark.circle.fill", ICON_SIZE + 1.0, &[white(1.0), green()], s),
            "todo" => sym("checklist", ICON_SIZE, &[if island.waits_on_owner() { orange() } else { white(1.0) }], s),
            "failed" => sym("xmark.circle.fill", ICON_SIZE + 1.0, &[white(1.0), red()], s),
            "viewer" => {
                let kind = self
                    .st
                    .borrow()
                    .file_viewer
                    .as_ref()
                    .map(|v| v.kind())
                    .unwrap_or_else(|| ViewerKind::detect(std::path::Path::new(&crate::model::island::viewer_path(&island.path().unwrap_or_default()))));
                sym(FileViewer::symbol(&kind), ICON_SIZE, &[white(1.0)], s)
            }
            "list" => sym("list.bullet", ICON_SIZE, &[if picked { white(1.0) } else { orange() }], s),
            _ => sym("pin.fill", ICON_SIZE - 1.0, &[white(1.0)], s),
        }
    }

    /// Running: every `WORK_INTERVAL` seconds the arc turns once (or the icon or ring takes one breath); nothing moves
    /// in between, and nothing at all once it stops running.
    fn refresh_working(&self) {
        let run = {
            let s = self.st.borrow();
            timers(self.on_screen(), s.removing, s.island.look() == "task", s.attention_shown.is_some(), false).work
        };
        if !run {
            self.st.borrow_mut().work_timer = None;
            return;
        }
        if self.st.borrow().work_timer.is_some() {
            return;
        }
        let me = self.me();
        let t = Repeat::new(WORK_INTERVAL, WORK_INTERVAL, move || {
            if let Some(c) = me.upgrade() {
                c.work_pulse();
            }
        });
        self.st.borrow_mut().work_timer = Some(t);
    }

    pub fn work_pulse(&self) {
        let (spinner, target) = {
            let s = self.st.borrow();
            if s.island.look() != "task" || !s.placed || s.removing {
                return;
            }
            (s.spinner.clone(), s.ring.as_ref().map(|r| Retained::into_super(r.clone())).or_else(|| s.icon.clone()))
        };
        if let Some(arc) = spinner.filter(|a| a.superlayer().is_some()) {
            let spin = basic("transform.rotation.z", Some(any(&*num(0.0))), Some(any(&*num(2.0 * std::f64::consts::PI))), 0.9);
            spin.setTimingFunction(Some(&ease_in_out()));
            add(&arc, &spin, "work");
        } else if let Some(t) = target {
            let a = basic("opacity", Some(any(&*num(1.0))), Some(any(&*num(0.45))), 0.45);
            a.setAutoreverses(true);
            a.setTimingFunction(Some(&ease_in_out()));
            add(&t, &a, "work");
        }
    }

    /// One sign of life when the agent changes a running or waiting island.
    fn nudge(&self) {
        let (spinner, pulse, icon, ring) = {
            let s = self.st.borrow();
            (s.spinner.clone(), s.pulse.clone(), s.icon.clone(), s.ring.is_some())
        };
        if let Some(arc) = spinner.filter(|a| a.superlayer().is_some()) {
            let spin = basic("transform.rotation.z", Some(any(&*num(0.0))), Some(any(&*num(2.0 * std::f64::consts::PI))), 0.9);
            spin.setTimingFunction(Some(&ease_in_out()));
            add(&arc, &spin, "spin");
        } else if let Some((halo, g)) = pulse.filter(|(h, _)| h.superlayer().is_some()) {
            add(&halo, &g, "pulse");
        } else if let (Some(icon), false) = (icon, ring) {
            let a = basic("opacity", Some(any(&*num(1.0))), Some(any(&*num(0.4))), 0.45);
            a.setAutoreverses(true);
            a.setTimingFunction(Some(&ease_in_out()));
            add(&icon, &a, "breath");
        }
    }

    // MARK: Body

    /// The body's height, built if needed.
    fn ensure_body(&self) -> f64 {
        if let Some(b) = &self.st.borrow().body {
            return b.height;
        }
        if self.is_viewer() {
            self.st.borrow_mut().body = Some(Body::empty());
            return 0.0;
        }
        let (born, written) = file_dates(&Paths::file(&self.id));
        let island = self.island();
        let (open_items, series, ring) = {
            let s = self.st.borrow();
            (s.open_items.clone(), s.series.clone(), s.ring.is_some())
        };
        let mut b = build_body(&island, self.expanded_width() - 2.0 * PAD, self.scale(), born, written, &open_items, &self.cal, &series, ring);
        self.tell_seen(&island);
        // HTML not drawn yet: its fallback shows now, the bitmap replaces it when it lands.
        for f in std::mem::take(&mut b.pending_html) {
            let me = self.me();
            super::html::request(&f, self.expanded_width() - 2.0 * PAD, self.scale(), move |d| {
                if let (Some(_), Some(me)) = (d, me.upgrade()) {
                    me.html_arrived();
                }
            });
        }
        if let Some(subs) = unsafe { self.body_host.sublayers() } {
            for l in subs.iter() {
                l.removeFromSuperlayer();
            }
        }
        self.body_host.addSublayer(&b.layer);
        self.body_host.setPosition(pt(PAD, BODY_TOP));
        let h = b.height;
        let sig = due_sig(&island, &self.cal);
        let mut st = self.st.borrow_mut();
        st.body = Some(b);
        st.due_sig = sig;
        drop(st);
        self.layout_viewport();
        self.reattach_confirm();
        h
    }

    fn body_layer(&self) -> Option<Retained<CALayer>> {
        self.st.borrow().body.as_ref().map(|b| b.layer.clone())
    }

    // MARK: Scrolling

    fn layout_viewport(&self) {
        let Some(bl) = self.body_layer() else { return };
        let scrolls = self.scrolls();
        let (max, vh) = (self.max_scroll(), self.viewport_height());
        quiet(|| {
            if scrolls {
                let y = { self.st.borrow().scroll_y }.max(0.0).min(max);
                self.st.borrow_mut().scroll_y = y;
                self.body_host.setMasksToBounds(true);
                self.body_host.setBounds(cgr(Rect::new(0.0, y, bl.bounds().size.width, vh)));
            } else {
                self.st.borrow_mut().scroll_y = 0.0;
                self.body_host.setMasksToBounds(false);
                self.body_host.setBounds(bl.bounds());
            }
        });
        self.update_scroll_edges();
    }

    fn update_scroll_edges(&self) {
        if !self.scrolls() {
            unsafe { self.body_host.setMask(None) };
            self.st.borrow_mut().scroll_fade = None;
            return;
        }
        let existing = self.st.borrow().scroll_fade.clone();
        let f = existing.clone().unwrap_or_else(|| {
            let g = CAGradientLayer::new();
            no_implicit(&g);
            g
        });
        let (sy, max) = (self.st.borrow().scroll_y, self.max_scroll());
        quiet(|| {
            let b = self.body_host.bounds();
            f.setFrame(b);
            let h = b.size.height.max(1.0);
            let edge = (18.0 / h).min(0.2);
            let top = if sy > 1.0 { 0.0 } else { 1.0 };
            let bottom = if sy < max - 1.0 { 0.0 } else { 1.0 };
            let c = |a: f64| gray(0.0, a).CGColor();
            let cols = [c(top), c(1.0), c(1.0), c(bottom)];
            let objs: Vec<&AnyObject> = cols.iter().map(|c| unsafe { &*(&**c as *const objc2_core_graphics::CGColor as *const AnyObject) }).collect();
            let arr = NSArray::from_slice(&objs);
            // SAFETY: an array of CGColors.
            unsafe { f.setColors(Some(&arr)) };
            let locs: Vec<Retained<NSNumber>> = [0.0, edge, 1.0 - edge, 1.0].iter().map(|v| num(*v)).collect();
            f.setLocations(Some(&NSArray::from_retained_slice(&locs)));
        });
        if existing.is_none() {
            unsafe { self.body_host.setMask(Some(&f)) };
            self.st.borrow_mut().scroll_fade = Some(f);
        }
    }

    /// Lab and pointer: put the body at `y` (no animation).
    pub fn set_scroll(&self, y: f64) {
        self.st.borrow_mut().scroll_y = y;
        let fade = self.st.borrow().scroll_fade.clone();
        quiet(|| {
            self.body_host.remove_anim("scroll");
            if let Some(f) = &fade {
                f.remove_anim("scroll");
            }
            let mut b = self.body_host.bounds();
            b.origin.y = y;
            self.body_host.setBounds(b);
        });
        self.update_scroll_edges();
        self.show_indicator();
    }

    fn animate_scroll(&self, y: f64, s: Spring) {
        let from = shown(&self.body_host).bounds().origin.y;
        self.st.borrow_mut().scroll_y = y;
        quiet(|| {
            let mut b = self.body_host.bounds();
            b.origin.y = y;
            self.body_host.setBounds(b);
        });
        let a = s.animation("bounds.origin.y", any(&*num(from)), any(&*num(y)), 0.0);
        timed(&a, &self.body_host);
        add(&self.body_host, &a, "scroll");
        self.update_scroll_edges();
        if let Some(f) = self.st.borrow().scroll_fade.clone() {
            let h = f.bounds().size.height / 2.0;
            let a = s.animation("position.y", any(&*num(from + h)), any(&*num(y + h)), 0.0);
            timed(&a, &f);
            add(&f, &a, "scroll");
        }
        self.show_indicator();
    }

    /// A scroll event over the island: true when the open body took it.
    pub fn scroll_wheel(self: &Rc<Self>, e: &NSEvent) -> bool {
        if !self.expanded() || !self.scrolls() || self.st.borrow().dragging {
            return false;
        }
        let max_y = self.max_scroll();
        if e.hasPreciseScrollingDeltas() {
            let (phase, momentum) = (e.phase(), e.momentumPhase());
            if phase.contains(NSEventPhase::Began) || phase.contains(NSEventPhase::MayBegin) {
                let mut s = self.st.borrow_mut();
                if let Some(w) = s.scroll_end_work.take() {
                    w.cancel();
                }
                s.momentum_spent = false;
            }
            if !momentum.is_empty() && self.st.borrow().momentum_spent {
                return true;
            }
            let d = e.scrollingDeltaY();
            let sy = self.st.borrow().scroll_y;
            let over = sy < 0.0 || sy > max_y;
            if !momentum.is_empty() && (over || sy - d < 0.0 || sy - d > max_y) {
                self.st.borrow_mut().momentum_spent = true;
                let raw = sy - d;
                let end = raw.max(0.0).min(max_y);
                self.set_scroll(end + ((raw - end) * 0.4).clamp(-24.0, 24.0));
                self.settle_scroll();
                return true;
            }
            let mut y = sy - if over { d * 0.4 } else { d };
            let limit = self.viewport_height() * 0.18;
            y = y.max(-limit).min(max_y + limit);
            if d != 0.0 {
                self.set_scroll(y);
            }
            let ended = phase.contains(NSEventPhase::Ended)
                || phase.contains(NSEventPhase::Cancelled)
                || momentum.contains(NSEventPhase::Ended)
                || momentum.contains(NSEventPhase::Cancelled);
            if ended {
                if let Some(w) = self.st.borrow_mut().scroll_end_work.take() {
                    w.cancel();
                }
                let me = self.me();
                let w = sys::after_cancellable(if momentum.contains(NSEventPhase::Ended) { 0.0 } else { 0.06 }, move || {
                    if let Some(c) = me.upgrade() {
                        c.settle_scroll();
                    }
                });
                self.st.borrow_mut().scroll_end_work = Some(w);
            } else if momentum.contains(NSEventPhase::Began) || momentum.contains(NSEventPhase::Changed) {
                if let Some(w) = self.st.borrow_mut().scroll_end_work.take() {
                    w.cancel();
                }
            }
        } else {
            let step = e.scrollingDeltaY() * 24.0;
            if step == 0.0 {
                return true;
            }
            let sy = self.st.borrow().scroll_y;
            self.animate_scroll((sy - step).max(0.0).min(max_y), Spring::CALM);
        }
        true
    }

    fn settle_scroll(&self) {
        let sy = self.st.borrow().scroll_y;
        let y = sy.max(0.0).min(self.max_scroll());
        if y != sy {
            self.animate_scroll(y, Spring::GROW);
        }
    }

    fn scroll_to_show(&self, r: Rect) {
        if !self.scrolls() {
            return;
        }
        let vh = self.viewport_height();
        let mut y = self.st.borrow().scroll_y;
        if r.max_y() > y + vh {
            y = r.max_y() - vh + 4.0;
        }
        if r.min_y() < y {
            y = r.min_y() - 4.0;
        }
        y = y.max(0.0).min(self.max_scroll());
        if y != self.st.borrow().scroll_y {
            self.animate_scroll(y, Spring::GROW);
        }
    }

    fn show_indicator(&self) {
        if !self.scrolls() {
            return;
        }
        let Some(total) = self.st.borrow().body.as_ref().map(|b| b.height.max(1.0)) else { return };
        let i = { let existing = self.st.borrow().scroll_indicator.clone(); existing }.unwrap_or_else(|| {
            let i = layer();
            set_bg(&i, &white(0.42));
            continuous(&i);
            i.setOpacity(0.0);
            i
        });
        self.st.borrow_mut().scroll_indicator = Some(i.clone());
        if i.superlayer().map(|s| Retained::as_ptr(&s)) != Some(Retained::as_ptr(&self.shape)) {
            self.shape.addSublayer(&i);
        }
        let vh = self.viewport_height();
        let max = self.max_scroll();
        let sy = self.st.borrow().scroll_y;
        let visible = sy.max(0.0).min(max) - sy;
        let len = (vh * vh / total - visible.abs()).max(28.0);
        let travel = vh - len;
        let k = if max > 0.0 { (sy / max).clamp(0.0, 1.0) } else { 0.0 };
        let w = self.expanded_size().w;
        quiet(|| {
            i.setFrame(cgr(Rect::new(w - 6.0, BODY_TOP + travel * k, 3.0, len)));
            i.setCornerRadius(1.5);
        });
        if i.opacity() < 1.0 {
            fade(&i, 1.0, 0.1, 0.0);
        }
        if let Some(w) = self.st.borrow_mut().indicator_work.take() {
            w.cancel();
        }
        let me = self.me();
        let w = sys::after_cancellable(0.8, move || {
            if let Some(c) = me.upgrade() {
                if let Some(i) = c.st.borrow().scroll_indicator.clone() {
                    fade(&i, 0.0, 0.3, 0.0);
                }
            }
        });
        self.st.borrow_mut().indicator_work = Some(w);
    }

    fn reset_scroll(&self) {
        let mut s = self.st.borrow_mut();
        if let Some(w) = s.scroll_end_work.take() {
            w.cancel();
        }
        if let Some(w) = s.indicator_work.take() {
            w.cancel();
        }
        if let Some(i) = s.scroll_indicator.take() {
            i.removeFromSuperlayer();
        }
        s.scroll_y = 0.0;
        drop(s);
        self.body_host.remove_anim("scroll");
    }

    fn show_body(&self, on: bool) {
        if on {
            self.ensure_body();
            self.body_host.setPosition(pt(PAD, BODY_TOP));
            self.scale_body(true);
            fade(&self.body_host, 1.0, 0.14, 0.02);
        } else {
            self.scale_body(false);
            fade(&self.body_host, 0.0, 0.09, 0.0);
        }
        self.run_clock();
    }

    /// While open, the "started 12 min ago" line is redrawn once a minute (one timer; none while it is a pill).
    fn run_clock(&self) {
        let has_clock = {
            let s = self.st.borrow();
            timers(self.on_screen(), s.removing, false, false, s.expanded && s.body.as_ref().is_some_and(|b| b.clock.is_some())).clock
        };
        self.st.borrow_mut().clock_timer = None;
        if !has_clock {
            return;
        }
        let me = self.me();
        // On the minute, with leeway: every island's clock (and the system's own) can fire together.
        let t = sys::ticker(60.0, 2.0, move || {
            let Some(c) = me.upgrade() else { return };
            let s = c.st.borrow();
            let Some((l, text)) = s.body.as_ref().and_then(|b| b.clock.as_ref()) else { return };
            let t = text();
            if text_of(l).as_deref() != Some(t.as_str()) {
                let l = l.clone();
                drop(s);
                quiet(|| retext(&l, &t));
            }
        });
        self.st.borrow_mut().clock_timer = t;
    }

    /// The open content grows from the pill's width to its own on the shape's spring.
    fn scale_body(&self, open: bool) {
        let open0 = self.expanded_size();
        let pill = self.pill_size();
        let k = (pill.w / open0.w.max(1.0)).min(1.0);
        self.body_group.setBounds(cgr(Rect::new(0.0, 0.0, open0.w, open0.h)));
        let s = if open { Spring::GROW } else { Spring::CALM };
        let in_flight = self.body_group.has_anim("scale");
        let p = shown(&self.body_group);
        let from_pos = if in_flight { p.position() } else { pt((if open { pill.w } else { open0.w }) / 2.0, 0.0) };
        let from_scale = if in_flight {
            p.key_value("transform.scale").map(|v| unsafe { msg_send![&*v, doubleValue] }).unwrap_or(1.0)
        } else if open {
            k
        } else {
            1.0
        };
        let to_pos = pt((if open { open0.w } else { pill.w }) / 2.0, 0.0);
        let to_scale = if open { 1.0 } else { k };
        self.body_group.setPosition(to_pos);
        self.body_group.set_key_value("transform.scale", any(&*num(to_scale)));
        add(&self.body_group, &s.animation("position", any(&*point_val(from_pos)), any(&*point_val(to_pos)), 0.0), "position");
        let sa = s.animation("transform.scale", any(&*num(from_scale)), any(&*num(to_scale)), 0.0);
        sa.setRemovedOnCompletion(true);
        add(&self.body_group, &sa, "scale");
    }

    // MARK: Placement and motion

    /// Put the island at `anchor` (its top centre) on the given screen.
    pub fn place(self: &Rc<Self>, a: Point, screen: Rect, top: f64, spring: Spring) {
        if self.st.borrow().dragging {
            return;
        }
        {
            let mut s = self.st.borrow_mut();
            s.anchor = Point::new(a.x, a.y.min(top));
            s.screen_frame = screen;
            s.row_top = top;
        }
        let target = self.rect_for(self.size());
        if !self.st.borrow().placed {
            self.st.borrow_mut().placed = true;
            let island = self.island();
            // A frame origin counts only for a viewer that arrives while the app runs; one still in a file at launch
            // (the app quit mid-open) is stale: it goes, and the island appears in place.
            let (frame, stale) = crate::model::island::frame_on_arrival(&island, self.st.borrow().from_notch);
            if stale {
                let id = self.id.clone();
                sys::after(0.0, move || { IslandFile::patch(&id, |o| { o.remove("origin"); }); });
            }
            let notch = frame.is_none() && (island.born_from_notch() || self.st.borrow().from_notch);
            let from;
            let mut grow = Spring::GROW;
            if let Some(fo) = frame {
                // Opened by a click on an island: it grows out of the clicked element, showing its picture first.
                from = fo.rect;
                if offscreen() && std::env::var("FI_LAB_SCRUB").is_ok() {
                    // Lab: freeze the motion's clock so the render can draw it moment by moment.
                    let t0 = sys::media_time();
                    self.root.setSpeed(0.0);
                    self.root.setTimeOffset(t0);
                    self.st.borrow_mut().lab_t0 = Some(t0);
                }
                grow = Spring::ZOOM;
                {
                    let mut s = self.st.borrow_mut();
                    s.born_from = Some(from);
                    s.born_island = fo.island.clone();
                    s.from_radius = Some(10.0);
                }
                let snapshot = island.path().and_then(|p| SNAPSHOTS.with(|s| s.borrow_mut().remove(&p)));
                let from_row = fo.image.is_none() && snapshot.is_some();
                let ghost_img = fo.image.as_deref().and_then(|p| super::parts::load_thumbnail(p, ((EXPANDED_WIDTH - 2.0 * PAD) * self.scale()) as usize)).or(snapshot);
                if let Some(img) = ghost_img {
                    let g = layer();
                    // SAFETY: a CGImage as contents.
                    unsafe { g.setContents(Some(img.object())) };
                    // SAFETY: a framework constant.
                    g.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityResizeAspectFill });
                    g.setMasksToBounds(true);
                    g.setCornerRadius(10.0);
                    continuous(&g);
                    self.root.addSublayer(&g);
                    self.st.borrow_mut().ghost = Some(g.clone());
                    // The thumbnail gives way to the full content as it arrives (a short crossfade once the motion is
                    // nearly over, so the two pictures never show apart; no blank frame). A clicked row is no picture of
                    // the content: it gives way early, and the viewer grows on as itself.
                    if from_row {
                        // SAFETY: a framework constant.
                        g.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityResize });
                        g.setCornerRadius(0.0);
                        fade(&g, 0.0, 0.14, 0.04);
                        self.st.borrow_mut().row_ghost = true;
                    } else {
                        fade(&g, 0.0, 0.1, 0.26);
                    }
                    let me = self.me();
                    sys::after(0.8, move || {
                        if let Some(c) = me.upgrade() {
                            if let Some(g) = c.st.borrow_mut().ghost.take() { g.removeFromSuperlayer() }
                        }
                    });
                }
                let id = self.id.clone();
                sys::after(0.7, move || {
                    IslandFile::patch(&id, |o| {
                        o.remove("origin");
                    });
                });
            } else if notch {
                let scr = NSScreen::screens(mtm()).iter().find(|s| rect_from(s.frame()) == screen).or_else(main_screen);
                from = notch_rect(scr.as_deref());
                self.st.borrow_mut().born_from = Some(from);
                if island.born_from_notch() {
                    let id = self.id.clone();
                    sys::after(0.7, move || {
                        IslandFile::patch(&id, |o| {
                            o.remove("origin");
                        });
                    });
                }
            } else {
                from = Rect::new(target.mid_x() - PILL_H / 2.0, target.max_y() - PILL_H, PILL_H, PILL_H);
                self.shape.setOpacity(0.0);
                fade(&self.shape, 1.0, 0.14, 0.0);
            }
            self.header.setOpacity(0.0);
            self.layout_content(false, grow);
            fade(&self.header, 1.0, 0.22, if notch || grow == Spring::ZOOM { 0.18 } else { 0.1 });
            self.apply_level();
            if !offscreen() && app().on_screen(&self.id) {
                self.panel.orderFrontRegardless();
                OverlaySpace::adopt(self.panel.windowNumber());
            }
            let expanded = self.expanded();
            if self.is_viewer() && expanded {
                self.viewer_opened(true);
            }
            self.morph(target, grow, Some(from), None);
            if expanded {
                self.show_body(true);
                if grow == Spring::ZOOM {
                    // Behind the clicked picture until it has arrived: the content comes in as the picture goes. From
                    // a row, it comes in as the motion ends (laid out for the final size, it would show cut while the
                    // shape grows).
                    let delay = if self.st.borrow().row_ghost || self.st.borrow().ghost.is_none() { 0.22 } else { 0.26 };
                    fade_from(&self.body_host, 0.0, 1.0, 0.1, delay);
                }
            }
            self.refresh_attention();
            return;
        }
        let v = self.st.borrow_mut().pending_velocity.take();
        if target != self.shape_rect() {
            self.morph(target, if v.is_none() { spring } else { Spring::TOSS }, None, v);
        }
    }

    /// The one motion: the window becomes the union of where the shape is and where it goes, the shape springs inside
    /// it (render server), and the window shrinks to the shape when the spring settles.
    fn morph(self: &Rc<Self>, target: Rect, s: Spring, explicit: Option<Rect>, velocity: Option<Vector>) {
        let from = explicit.unwrap_or_else(|| self.visible_shape_rect());
        for k in ["position", "position.x", "position.y"] {
            self.shape.remove_anim(k);
        }
        let g = {
            let mut st = self.st.borrow_mut();
            st.shape_rect = target;
            st.generation += 1;
            st.generation
        };
        app().state_changed();
        let room = velocity.map(|v| (0.015 * v.length()).max(60.0)).unwrap_or(0.0);
        let union = from.union(&target).inset(-room, -room);
        let me = self.me();
        sys::after(if velocity.is_none() { 1.0 } else { 1.8 }, move || {
            let Some(c) = me.upgrade() else { return };
            let (gen, removing) = {
                let s = c.st.borrow();
                (s.generation, s.removing)
            };
            // The morph's own motion only: a hide or show in flight (`toggle_motion`) runs on.
            let keys: Vec<String> = c.shape.animationKeys().map(|k| k.iter().map(|k| k.to_string()).collect()).unwrap_or_default();
            let morph: Vec<&String> = keys.iter().filter(|k| k.as_str() != crate::model::toggle::TOGGLE_SCALE).collect();
            if g != gen || removing || morph.is_empty() {
                return;
            }
            for k in morph {
                c.shape.remove_anim(k);
            }
            c.body_group.removeAllAnimations();
            c.settle(g);
        });
        let me = self.me();
        transaction(
            true,
            Some(Box::new(move || {
                if let Some(c) = me.upgrade() {
                    c.settle(g);
                }
            })),
            || {
                if rect_from(self.panel.frame()) != union {
                    self.panel.setFrame_display(ns_rect(union), false);
                }
                self.view.setFrame(ns_rect(Rect::new(0.0, 0.0, union.w, union.h)));
                let f = local(from, union);
                let t = local(target, union);
                self.shape.setBounds(cgr(Rect::new(0.0, 0.0, t.w, t.h)));
                self.shape.setPosition(pt(t.mid_x(), t.mid_y()));
                self.shape.setCornerRadius(radius(t.h));
                let from_radius = self.st.borrow_mut().from_radius.take().unwrap_or_else(|| radius(f.h));
                if from.size() != target.size() {
                    add(
                        &self.shape,
                        &s.animation("bounds", any(&*rect_val(cgr(Rect::new(0.0, 0.0, f.w, f.h)))), any(&*rect_val(self.shape.bounds())), 0.0),
                        "bounds",
                    );
                    add(&self.shape, &s.animation("cornerRadius", any(&*num(from_radius)), any(&*num(radius(t.h))), 0.0), "cornerRadius");
                }
                if let Some(v) = velocity {
                    for (key, a, b, speed) in [("position.x", f.mid_x(), t.mid_x(), v.dx), ("position.y", f.mid_y(), t.mid_y(), -v.dy)] {
                        if (b - a).abs() < 0.5 {
                            continue;
                        }
                        let vel = if (b - a).abs() < 2.0 { 0.0 } else { (speed / (b - a)).clamp(-40.0, 40.0) };
                        add(&self.shape, &s.animation(key, any(&*num(a)), any(&*num(b)), vel), key);
                    }
                } else if from.origin() != target.origin() || from.size() != target.size() {
                    add(
                        &self.shape,
                        &s.animation("position", any(&*point_val(pt(f.mid_x(), f.mid_y()))), any(&*point_val(self.shape.position())), 0.0),
                        "position",
                    );
                }
                // The clicked picture rides the same spring from its own frame to where the viewer shows the picture
                // (the content area), so the crossfade to the full-resolution picture lines up; on the render server,
                // no app frames.
                // A clicked row stays where it was and fades as the viewer grows out from under it (scaled up, its
                // text would swell across the viewer).
                if let Some(g) = self.st.borrow().ghost.clone().filter(|_| self.st.borrow().row_ghost) {
                    g.setBounds(cgr(Rect::new(0.0, 0.0, f.w, f.h)));
                    g.setPosition(pt(f.mid_x(), f.mid_y()));
                } else if let Some(g) = self.st.borrow().ghost.clone() {
                    let c = Rect::new(t.x + PAD, t.y + BODY_TOP, (t.w - 2.0 * PAD).max(1.0), (t.h - BODY_TOP - BOTTOM_PAD).max(1.0));
                    g.setBounds(cgr(Rect::new(0.0, 0.0, c.w, c.h)));
                    g.setPosition(pt(c.mid_x(), c.mid_y()));
                    add(&g, &s.animation("bounds", any(&*rect_val(cgr(Rect::new(0.0, 0.0, f.w, f.h)))), any(&*rect_val(g.bounds())), 0.0), "bounds");
                    add(&g, &s.animation("position", any(&*point_val(pt(f.mid_x(), f.mid_y()))), any(&*point_val(g.position())), 0.0), "position");
                }
            },
        );
    }

    fn settle(self: &Rc<Self>, g: u64) {
        let (gen, removing, sr, expanded) = {
            let s = self.st.borrow();
            (s.generation, s.removing, s.shape_rect, s.expanded)
        };
        if g != gen || removing {
            return;
        }
        app().state_changed();
        if rect_from(self.panel.frame()) != sr {
            quiet(|| {
                self.panel.setFrame_display(ns_rect(sr), false);
                self.view.setFrame(ns_rect(Rect::new(0.0, 0.0, sr.w, sr.h)));
                self.shape.setBounds(cgr(Rect::new(0.0, 0.0, sr.w, sr.h)));
                self.shape.setPosition(pt(sr.w / 2.0, sr.h / 2.0));
            });
        }
        // Mid hide or show the live viewer stays out: it comes back when the growth is over (`live_viewer_after`).
        let toggling = self.leaving() || self.shape.has_anim(crate::model::toggle::TOGGLE_SCALE);
        if self.is_viewer() && expanded && !toggling {
            self.show_scroll();
        }
    }

    /// Where the shape is on screen now (its presentation), screen coordinates.
    pub fn visible_shape_rect(&self) -> Rect {
        if !self.st.borrow().placed {
            return self.shape_rect();
        }
        let p = shown(&self.shape);
        let b = p.bounds().size;
        let c = p.position();
        let wf = rect_from(self.panel.frame());
        Rect::new(wf.min_x() + c.x - b.width / 2.0, wf.max_y() - c.y - b.height / 2.0, b.width, b.height)
    }

    /// Header and trailing positions for the current state; the header glides from centred (pill) to the leading edge
    /// (open) on the shape's own spring.
    fn layout_content(&self, animated: bool, _s: Spring) {
        let w = self.size().w;
        let (in_pill, expanded, trailing) = {
            let s = self.st.borrow();
            (s.trailing_in_pill, s.expanded, s.trailing_layer.clone())
        };
        let hw = self.header_width();
        let tw = self.trailing_width();
        let total = hw + if in_pill { tw + 7.0 } else { 0.0 };
        let hx = if expanded { PAD } else { (w - total) / 2.0 };
        let tx = (w - total) / 2.0 + hw + 7.0;
        let ty = (PILL_H - trailing.as_ref().map(|t| t.bounds().size.height).unwrap_or(0.0)) / 2.0;
        let t_op: f32 = if !expanded && in_pill { 1.0 } else { 0.0 };
        if animated {
            hop(&self.header, pt(hx, 0.0), 1.0, expanded);
            if let Some(tl) = &trailing {
                hop(tl, pt(tx, ty), t_op, expanded);
            }
        } else {
            self.header.setPosition(pt(hx, 0.0));
            if let Some(tl) = &trailing {
                tl.setPosition(pt(tx, ty));
                tl.setOpacity(t_op);
            }
        }
        self.layout_close(animated);
    }

    /// Open islands sit above pills (a viewer above both).
    fn apply_level(&self) {
        let base = objc2_app_kit::NSStatusWindowLevel;
        let lvl = if self.expanded() { base + if self.is_viewer() { 2 } else { 1 } } else { base };
        self.panel.setLevel(lvl);
    }

    pub fn set_initial_expanded(&self) {
        self.st.borrow_mut().expanded = true;
    }

    /// Every island opens: a bare one shows its state, stage and times.
    pub fn set_expanded(self: &Rc<Self>, e: bool) {
        if e == self.expanded() {
            return;
        }
        self.st.borrow_mut().expanded = e;
        if !e {
            self.cancel_confirm();
            self.reset_scroll();
            quiet(|| self.layout_viewport());
        }
        if !self.st.borrow().placed {
            return;
        }
        if self.is_viewer() {
            self.viewer_opened(e);
        }
        self.apply_level();
        let s = if e { Spring::GROW } else { Spring::CALM };
        CATransaction::begin();
        self.layout_content(true, s);
        self.show_body(e);
        if !self.is_viewer() {
            app().layout_now(s);
        } else {
            let r = self.rect_for(self.size());
            self.morph(r, s, None, None);
        }
        CATransaction::commit();
        if e && !offscreen() && app().on_screen(&self.id) {
            self.panel.orderFrontRegardless();
        }
        self.refresh_attention();
        // Its labels are visible now (or no longer): the one label timer follows them.
        app().review_due(true);
    }

    // MARK: Updates from the file

    pub fn update(self: &Rc<Self>, new: Island) {
        self.update_held(new, true)
    }

    /// `hold`: an open island may wait for its HTML bitmaps first (`hold_for_html`); the held update itself comes back
    /// with `false`, so it applies whatever is drawn by then and never waits twice (review, 3 Nov).
    fn update_held(self: &Rc<Self>, new: Island, hold: bool) {
        let old = self.island();
        if j::obj_eq(&new.raw, &old.raw) {
            return;
        }
        // A viewer already on screen opened again by a click: it now belongs to that island (closing shrinks back into
        // it), and the origin is used up at once, so a relaunch never grows it from a stale frame.
        let placed = self.st.borrow().placed;
        if let Some(fo) = crate::model::island::frame_on_update(&new, placed) {
            self.st.borrow_mut().born_island = fo.island.clone();
            let id = self.id.clone();
            sys::after(0.0, move || { IslandFile::patch(&id, |o| { o.remove("origin"); }); });
        }
        self.st.borrow_mut().html_gen += 1;
        if hold && self.hold_for_html(&new) {
            return;
        }
        let (mut a, mut b) = (old.raw.clone(), new.raw.clone());
        a.remove("origin");
        b.remove("origin");
        if j::obj_eq(&a, &b) {
            self.st.borrow_mut().island = new;
            return;
        }
        {
            let mut st = self.st.borrow_mut();
            st.island = new.clone();
            st.series.record(&new);
        }
        if old.path() != new.path() || !j::opt_eq(old.raw.get("actions"), new.raw.get("actions")) {
            self.reset_viewer();
            if self.is_viewer() && self.expanded() && self.st.borrow().placed {
                self.viewer_opened(true);
                let r = self.rect_for(self.size());
                self.morph(r, Spring::GROW, None, None);
            }
        } else if self.is_viewer() && self.file_viewer().is_some_and(|v| v.is_stale()) {
            // Shown again (or rewritten) with the same path while the file changed: read it again (bug 22 Oct 20:46).
            self.reload_viewer();
        }
        CATransaction::begin();
        self.build_header(if self.is_viewer() && old.path() != new.path() { None } else { Some(&old) });
        let shown_keys = [
            "parts", "chosen", "progress", "width", "stage", "value", "summary", "steps", "items", "goals", "kv", "image", "open", "state", "preset",
            "updated", "started", "ended", "title", "details",
        ];
        let body_changed = old.look() != new.look() || shown_keys.iter().any(|k| !j::opt_eq(old.raw.get(*k), new.raw.get(*k)));
        if ["task", "waiting"].contains(&new.look()) && old.look() == new.look() && body_changed {
            self.nudge();
        }
        if new.look() == "todo" && !self.expanded() && (!j::opt_eq(old.raw.get("items"), new.raw.get("items")) || !j::opt_eq(old.raw.get("goals"), new.raw.get("goals"))) {
            self.nudge();
        }
        if body_changed {
            self.cancel_confirm();
            let was_shown = self.st.borrow().body.is_some() && self.expanded();
            let plan_before = is_plan(&old).then(|| Plan::display_order(&old.raw, &old.list().unwrap_or_else(|| "todo".into())));
            let captured = if was_shown { self.capture_rows() } else { HashMap::new() };
            self.st.borrow_mut().body = None;
            if was_shown {
                self.ensure_body();
                self.pin_body_open();
                if let (Some(before), true, true) = (&plan_before, is_plan(&new), old.look() == new.look()) {
                    let after = Plan::display_order(&new.raw, &new.list().unwrap_or_else(|| "todo".into()));
                    self.transition_rows(captured, ItemDiff::new(before, &after));
                } else {
                    let t = CATransition::new();
                    // SAFETY: a framework constant.
                    t.setType(unsafe { objc2_quartz_core::kCATransitionFade });
                    t.setDuration(0.18);
                    add(&self.body_host, &t, "swap");
                }
                // A new look arrives whole; the same look moves only what the rewrite changed (a sparkline's newest
                // step, a bar from its last value).
                let moving: Vec<*const dyn Fn()> = self
                    .st
                    .borrow()
                    .body
                    .as_ref()
                    .map(|b| if old.look() != new.look() { &b.arrivals } else { &b.updates }.iter().map(|a| &**a as *const dyn Fn()).collect())
                    .unwrap_or_default();
                for a in moving {
                    // SAFETY: the body (and its closures) lives in `st` while these run; none of them touch `st`.
                    unsafe { (*a)() };
                }
            } else if let Some(subs) = unsafe { self.body_host.sublayers() } {
                for l in subs.iter() {
                    l.removeFromSuperlayer();
                }
            }
            self.run_clock();
        }
        CATransaction::commit();
        if new.expanded() != self.expanded() {
            self.set_expanded(new.expanded());
        } else {
            CATransaction::begin();
            self.layout_content(true, Spring::GROW);
            CATransaction::commit();
        }
        self.refresh_attention();
        app().relayout();
    }

    /// Model HTML (`mac/html.rs`): an island on screen open waits for its new bitmaps (a few ms on a warm web view, at
    /// most `html::DEADLINE`) and then changes once, with no fallback flashing in between; a closed one changes at once
    /// and its bitmaps are drawn behind it, so it opens ready. Returns whether the update is held.
    fn hold_for_html(self: &Rc<Self>, new: &Island) -> bool {
        let w = self.expanded_width() - 2.0 * PAD;
        let todo = super::parts::html_to_draw(new, w, self.scale());
        if todo.is_empty() {
            return false;
        }
        if !(self.expanded() && self.st.borrow().body.is_some()) {
            self.draw_html_ahead(new);
            return false;
        }
        let gen = self.st.borrow().html_gen;
        debug(&format!("update of {} held for {} html bitmaps", self.id, todo.len()));
        let left = Rc::new(Cell::new(todo.len()));
        for f in todo {
            let (me, new, left) = (self.me(), new.clone(), left.clone());
            super::html::request(&f, w, self.scale(), move |_| {
                left.set(left.get() - 1);
                if left.get() > 0 {
                    return;
                }
                let Some(me) = me.upgrade() else { return };
                if me.st.borrow().html_gen == gen && !me.removing() {
                    me.update_held(new, false);
                }
            });
        }
        true
    }

    /// Draw an island's HTML bitmaps ahead, while it is closed.
    fn draw_html_ahead(&self, island: &Island) {
        let w = self.expanded_width() - 2.0 * PAD;
        for f in super::parts::html_to_draw(island, w, self.scale()) {
            super::html::request(&f, w, self.scale(), |_| {});
        }
    }

    /// A bitmap the open body drew a fallback for has landed: the body is built again with it.
    fn html_arrived(self: &Rc<Self>) {
        debug(&format!("html arrived for {}", self.id));
        if self.st.borrow().body.is_none() || self.removing() {
            return;
        }
        let shown = self.expanded();
        self.st.borrow_mut().body = None;
        if !shown {
            if let Some(subs) = unsafe { self.body_host.sublayers() } {
                for l in subs.iter() {
                    l.removeFromSuperlayer();
                }
            }
            return;
        }
        CATransaction::begin();
        self.ensure_body();
        self.pin_body_open();
        let t = CATransition::new();
        // SAFETY: a framework constant.
        t.setType(unsafe { objc2_quartz_core::kCATransitionFade });
        t.setDuration(0.18);
        add(&self.body_host, &t, "swap");
        self.run_clock();
        CATransaction::commit();
        CATransaction::begin();
        self.layout_content(true, Spring::GROW);
        CATransaction::commit();
        app().relayout();
    }

    /// Redraw the open list when one of its due labels changed: those rows cross-fade, nothing else moves.
    /// Also when what it reads of the other islands changed (`world::Ctx`): a job island starts or stops running under
    /// a goal, a goal is renamed. A job island with `sent` redraws the same way (going quiet, its minutes).
    pub fn refresh_due_labels(self: &Rc<Self>) {
        if !self.expanded() || self.st.borrow().body.is_none() {
            return;
        }
        let island = self.island();
        let plan = is_plan(&island);
        if !plan && crate::model::onesys::sent(&island).is_none() {
            return;
        }
        if due_sig(&island, &self.cal) == self.st.borrow().due_sig {
            return;
        }
        if plan {
            self.rebuild_plan();
        } else {
            self.rebuild_job();
        }
    }

    /// A job island's body built again where it is, cross-faded.
    fn rebuild_job(self: &Rc<Self>) {
        CATransaction::begin();
        self.cancel_confirm();
        self.st.borrow_mut().body = None;
        self.ensure_body();
        self.pin_body_open();
        let t = CATransition::new();
        // SAFETY: a framework constant.
        t.setType(unsafe { objc2_quartz_core::kCATransitionFade });
        t.setDuration(0.18);
        add(&self.body_host, &t, "swap");
        self.run_clock();
        CATransaction::commit();
        CATransaction::begin();
        self.layout_content(true, Spring::GROW);
        CATransaction::commit();
        app().relayout();
    }

    /// The next moment this open island reads differently by the clock (one-system labels), if any.
    pub fn next_label_change(&self, now: f64) -> Option<f64> {
        if !self.expanded() {
            return None;
        }
        let i = self.island();
        match i.list().as_deref() {
            Some("goals") => crate::model::onesys::next_expiry(&i.raw, now),
            Some("scheduled") => super::scheduled::next_change(&i.raw, now, &self.cal),
            Some(_) => None,
            None => crate::model::onesys::job_next(&i, now),
        }
    }

    /// A new local day: Settled's pill count and, when open, its week are redrawn (`App::arm_day`).
    pub fn refresh_day(self: &Rc<Self>) {
        if !self.st.borrow().placed {
            return;
        }
        let island = self.island();
        CATransaction::begin();
        self.build_header(Some(&island));
        CATransaction::commit();
        self.refresh_due_labels();
        app().relayout();
    }

    // MARK: Plan rows: keyed layers animated by id

    fn pin_body_open(&self) {
        self.body_host.setOpacity(1.0);
        let sz = self.expanded_size();
        quiet(|| {
            self.body_group.removeAllAnimations();
            self.body_group.setBounds(cgr(Rect::new(0.0, 0.0, sz.w, sz.h)));
            self.body_group.setPosition(pt(sz.w / 2.0, 0.0));
            self.body_group.set_key_value("transform.scale", any(&*num(1.0)));
        });
    }

    fn capture_rows(&self) -> HashMap<String, (Retained<CALayer>, CGPoint, String)> {
        let s = self.st.borrow();
        let mut out = HashMap::new();
        if let Some(b) = &s.body {
            for (k, (l, sig)) in &b.rows {
                out.insert(k.clone(), (l.clone(), shown(l).position(), sig.clone()));
            }
        }
        out
    }

    /// From the old rows to the new on the grow spring: kept rows slide from where they were; a row whose look
    /// changed cross-fades while it slides; a new row slides in under a highlight; a removed row fades where it was.
    fn transition_rows(&self, old: HashMap<String, (Retained<CALayer>, CGPoint, String)>, diff: ItemDiff) {
        let s = Spring::GROW;
        let Some((rows, blayer)) = self.st.borrow().body.as_ref().map(|b| (b.rows.clone(), b.layer.clone())) else { return };
        let added: HashSet<String> = diff.added.iter().map(|k| format!("row:{k}")).collect();
        let moved: HashSet<String> = diff.moved.iter().map(|k| format!("row:{k}")).collect();
        let mut crossing = moved.clone();
        crossing.extend(diff.ticked.iter().map(|k| format!("row:{k}")));
        let adopt = |l: &CALayer, p: CGPoint| {
            quiet(|| {
                l.removeFromSuperlayer();
                l.remove_anim("slide");
                l.setPosition(p);
                blayer.insertSublayer_atIndex(l, 0);
            });
        };
        let fade_away = |l: &Retained<CALayer>, d: f64| {
            let l2 = l.clone();
            transaction(false, Some(Box::new(move || l2.removeFromSuperlayer())), || fade(l, 0.0, d, 0.0));
        };
        let slide = |l: &CALayer, from: CGPoint, to: CGPoint| {
            if from.x == to.x && from.y == to.y {
                return;
            }
            let a = s.animation("position", any(&*point_val(from)), any(&*point_val(to)), 0.0);
            timed(&a, l);
            add(l, &a, "slide");
        };
        CATransaction::begin();
        for (key, (layer, sig)) in &rows {
            let target = layer.position();
            layer.setZPosition(if crossing.contains(key) { 1.0 } else { 0.0 });
            if let Some((o, from, osig)) = old.get(key) {
                slide(layer, *from, target);
                if key == "tray" {
                    // Goals' asks tray grows or shrinks with its asks on the same spring (it is never cross-faded).
                    let a = s.animation("bounds", any(&*rect_val(o.bounds())), any(&*rect_val(layer.bounds())), 0.0);
                    timed(&a, layer);
                    add(layer, &a, "grow");
                }
                if osig != sig {
                    adopt(o, *from);
                    o.setPosition(target);
                    slide(o, *from, target);
                    fade_away(o, 0.16);
                    layer.setOpacity(0.0);
                    fade(layer, 1.0, 0.26, 0.02);
                }
                if moved.contains(key) {
                    highlight(layer, 0.09);
                }
            } else {
                layer.setOpacity(0.0);
                fade(layer, 1.0, 0.24, 0.04);
                if added.contains(key) {
                    slide(layer, pt(target.x - 18.0, target.y), target);
                    highlight(layer, 0.14);
                } else if key.starts_with("prop:") || key.starts_with("gtask:") {
                    // A proposal, or a goal's finished tasks shown: they come down out of the row above.
                    slide(layer, pt(target.x, target.y - 8.0), target);
                }
            }
        }
        for (key, (o, from, _)) in &old {
            if !rows.contains_key(key) {
                adopt(o, *from);
                fade_away(o, 0.2);
            }
        }
        CATransaction::commit();
    }

    /// A proposal drawn to the user, its list open and on screen: one `seen` response per proposal per run, so Heimdall
    /// starts its two days then (`docs/one-system.md`; never offscreen renders, never while every island is hidden).
    pub fn tell_seen(&self, island: &Island) {
        if offscreen() || !self.expanded() || !app().on_screen(&self.id) { return }
        let Some(list) = island.list().filter(|l| l == "goals" || l == "scheduled") else { return };
        let Some((p, _)) = crate::model::onesys::front(&island.raw, crate::model::time::now()) else { return };
        thread_local! { static TOLD: RefCell<std::collections::HashSet<String>> = RefCell::new(Default::default()); }
        if TOLD.with(|t| t.borrow_mut().insert(p.id.clone())) && p.shown.is_none() {
            IslandFile::respond_object(&crate::jobj! {"island": list, "kind": "seen", "value": p.id.clone()});
        }
    }

    fn rebuild_plan(self: &Rc<Self>) {
        if !self.expanded() || self.st.borrow().body.is_none() || !(is_plan(&self.island()) || has_timeline(&self.island())) {
            return;
        }
        let captured = self.capture_rows();
        self.st.borrow_mut().body = None;
        self.ensure_body();
        self.pin_body_open();
        self.transition_rows(captured, ItemDiff::default());
        app().layout_now(Spring::GROW);
    }

    // MARK: Attention

    pub fn refresh_attention(self: &Rc<Self>) {
        self.refresh_attention_inner();
        // Up for news while hidden: open, or a question not seen yet, holds it (`App::flash_sync`).
        app().flash_sync(self);
    }

    fn refresh_attention_inner(self: &Rc<Self>) {
        if self.removing() {
            return;
        }
        let island = self.island();
        let (sig, kind) = (Attention::signature(&island), Attention::of(&island));
        let expanded = self.expanded();
        if sig.is_none() || expanded {
            self.st.borrow_mut().seen_attention = sig.clone();
        }
        let show = if kind.is_some() && !expanded && sig != self.st.borrow().seen_attention { kind } else { None };
        if show != self.attention_shown() {
            self.st.borrow_mut().attention_shown = show.clone();
            if self.st.borrow().placed {
                CATransaction::begin();
                self.build_header(Some(&island));
                CATransaction::commit();
                app().relayout();
            }
        }
        self.arm_attention();
    }

    /// The attention pulse's timer, armed while a question or bell shows on a pill on screen (none while hidden).
    fn arm_attention(&self) {
        let want = {
            let s = self.st.borrow();
            timers(self.on_screen(), s.removing, false, s.attention_shown.is_some(), false).attention
        };
        if !want {
            self.st.borrow_mut().attention_timer = None;
            return;
        }
        if self.st.borrow().attention_timer.is_some() {
            return;
        }
        let me = self.me();
        let t = Repeat::new(0.4, Attention::INTERVAL, move || {
            if let Some(c) = me.upgrade() {
                c.pulse_attention();
            }
        });
        let mut s = self.st.borrow_mut();
        s.attention_timer = Some(t);
        s.attention_since = sys::media_time();
    }

    /// One pulse: a soft glow of the attention colour across the pill and its edge, and the symbol rings (bell) or
    /// bounces (question mark). About a second, then still until the next one.
    pub fn pulse_attention(self: &Rc<Self>) {
        let kind = {
            let s = self.st.borrow();
            match (&s.attention_shown, s.expanded, s.placed, s.removing) {
                (Some(k), false, true, false) => k.clone(),
                _ => return,
            }
        };
        let color = if kind == "question" { blue() } else { orange() };
        self.st.borrow_mut().pulse_count += 1;
        self.glow(&color, 0.45, 0.9);
        let icon = self.st.borrow().icon.clone();
        if let Some(icon) = icon {
            if kind == "question" {
                let y = keyframes("transform.translation.y", &[0.0, -5.0, 0.0, -2.0, 0.0], Some(&[0.0, 0.3, 0.55, 0.78, 1.0]), 0.7);
                let sc = keyframes("transform.scale", &[1.0, 1.14, 0.97, 1.04, 1.0], Some(&[0.0, 0.3, 0.55, 0.78, 1.0]), 0.7);
                let grp = group(&[&y, &sc], 0.7);
                grp.setTimingFunction(Some(&ease_out()));
                timed(&grp, &icon);
                add(&icon, &grp, "attnRing");
            } else {
                let r = keyframes("transform.rotation.z", &[0.0, 0.42, -0.36, 0.24, -0.13, 0.05, 0.0], None, 0.8);
                r.setTimingFunction(Some(&ease_out()));
                timed(&r, &icon);
                add(&icon, &r, "attnRing");
            }
        }
        let me = self.me();
        if let Some(c) = me.upgrade() {
            LAB_PULSE.with(|f| {
                if let Some(f) = f.borrow().as_ref() {
                    f(&c);
                }
            });
        }
    }

    /// A soft glow of `color` from the symbol across the shape and its edge (1.3 s), then gone.
    fn glow(&self, color: &NSColor, fill: f64, edge: f64) {
        let g = { let existing = self.st.borrow().glow.clone(); existing }.unwrap_or_else(|| {
            let g = layer();
            continuous(&g);
            g.setBorderWidth(1.5);
            g.setOpacity(0.0);
            let grad = CAGradientLayer::new();
            no_implicit(&grad);
            // SAFETY: a framework constant.
            grad.setType(unsafe { objc2_quartz_core::kCAGradientLayerRadial });
            g.addSublayer(&grad);
            self.st.borrow_mut().glow = Some(g.clone());
            g
        });
        quiet(|| {
            let b = self.shape.bounds();
            g.setFrame(b);
            g.setCornerRadius(self.shape.cornerRadius());
            g.setBorderColor(Some(&color.colorWithAlphaComponent(edge).CGColor()));
            if let Some(subs) = unsafe { g.sublayers() } {
                if let Some(first) = subs.iter().next() {
                    // SAFETY: the first sublayer is the CAGradientLayer made above.
                    let grad: &CAGradientLayer = unsafe { &*(Retained::as_ptr(&first) as *const CAGradientLayer) };
                    grad.setFrame(b);
                    let c1 = color.colorWithAlphaComponent(fill).CGColor();
                    let c2 = color.colorWithAlphaComponent(0.0).CGColor();
                    let objs: Vec<&AnyObject> =
                        [&c1, &c2].iter().map(|c| unsafe { &*(&***c as *const objc2_core_graphics::CGColor as *const AnyObject) }).collect();
                    // SAFETY: CGColors.
                    unsafe { grad.setColors(Some(&NSArray::from_slice(&objs))) };
                    let cx = (self.header.position().x + ICON_SIZE / 2.0) / b.size.width.max(1.0);
                    grad.setStartPoint(pt(cx, 0.5));
                    grad.setEndPoint(pt(cx + 90.0 / b.size.width.max(1.0), 0.5 + 90.0 / b.size.height.max(1.0)));
                }
            }
            if g.superlayer().map(|s| Retained::as_ptr(&s)) != Some(Retained::as_ptr(&self.shape)) {
                self.shape.insertSublayer_atIndex(&g, 0);
            }
        });
        let a = keyframes("opacity", &[0.0, 1.0, 0.0], Some(&[0.0, 0.22, 1.0]), 1.3);
        a.setTimingFunction(Some(&ease_in_out()));
        timed(&a, &g);
        add(&g, &a, "attnGlow");
    }

    // MARK: Back for news while hidden (`model/flash.rs`)

    /// Onto the screen, if it is not, and fade in from where the fade is. An island not placed yet arrives as usual
    /// (`place` orders it in).
    pub fn flash_in(self: &Rc<Self>) {
        let (placed, removing, was) = {
            let mut s = self.st.borrow_mut();
            let was = s.flashed;
            s.flashed = true;
            (s.placed, s.removing, was)
        };
        if !placed || removing {
            return;
        }
        // On screen again: what moves while it is seen moves again.
        self.refresh_timers();
        // News while it shrinks away: the hide is over for it; it turns around from where it is on screen and comes
        // back whole (its window never went out).
        if self.st.borrow().leaving {
            {
                let mut s = self.st.borrow_mut();
                s.leaving = false;
                s.toggle_gen += 1;
            }
            self.toggle_motion(true, 0.0, reduce_motion());
            return;
        }
        if !was {
            // An open viewer's live view sits beside the root: its picture fades in with the rest, then it is live.
            self.freeze_viewer();
            quiet(|| self.root.setOpacity(0.0));
            if !offscreen() {
                self.panel.orderFrontRegardless();
                OverlaySpace::adopt(self.panel.windowNumber());
            }
            fade_from(&self.root, 0.0, 1.0, crate::model::flash::FADE_IN, 0.0);
            self.live_viewer_after(crate::model::flash::FADE_IN);
        } else {
            // Back from where its fade out is (its viewer went as its picture).
            fade(&self.root, 1.0, crate::model::flash::FADE_IN, 0.0);
            self.live_viewer_after(crate::model::flash::FADE_IN);
        }
    }

    /// One soft pulse in the colour of the news: its question or bell as usual, else green done, red failed, orange
    /// waiting, white for anything else.
    pub fn flash_pulse(self: &Rc<Self>) {
        if self.attention_shown().is_some() {
            // A question or bell that has just appeared pulses on its own timer (0.4 s); only later news pulses here.
            if sys::media_time() - self.st.borrow().attention_since >= 1.0 {
                self.pulse_attention();
            }
            return;
        }
        let island = self.island();
        let (color, fill, edge) = match island.look() {
            "failed" => (red(), 0.4, 0.85),
            "waiting" => (orange(), 0.4, 0.85),
            // A viewer or note reads as done without saying so: its news is a reply, white.
            _ if island.state() == "done" && !matches!(island.preset(), "viewer" | "note") => (green(), 0.4, 0.85),
            _ => (white(1.0), 0.22, 0.5),
        };
        self.glow(&color, fill, edge);
    }

    pub fn flash_out(&self) {
        // An open viewer goes as its picture, with the rest.
        self.freeze_viewer();
        fade(&self.root, 0.0, crate::model::flash::FADE_OUT, 0.0);
    }

    /// The fade out is over: off the screen again, ready to come back.
    pub fn flash_gone(&self) {
        self.flash_reset();
        if app().hidden() && !offscreen() {
            if self.panel.isKeyWindow() {
                self.panel.resignKeyWindow();
            }
            self.panel.orderOut(None);
        }
        self.refresh_timers();
    }

    /// No longer up for news: full opacity for the next time it is on screen.
    pub fn flash_reset(&self) {
        self.st.borrow_mut().flashed = false;
        quiet(|| {
            self.root.removeAnimationForKey(&NSString::from_str("opacity"));
            self.root.setOpacity(1.0);
        });
    }

    /// All islands hidden or shown (`App::set_hidden`, `model/toggle.rs`): going, the shape shrinks into its own centre
    /// on a spring and fades, then the window goes out; coming, the window is back at once and the shape grows out of
    /// its centre to its exact place. Turned around midway, it goes back from where it is on screen. Nothing else
    /// changes. An island not placed yet was never on screen and is left to its own path; one `up` for news is on
    /// screen already and stays as it is.
    pub fn set_screen_hidden(self: &Rc<Self>, hidden: bool, delay: f64, reduce: bool, up: bool) {
        let (placed, removing) = {
            let s = self.st.borrow();
            (s.placed, s.removing)
        };
        if !placed || (removing && !hidden) {
            return;
        }
        let g = {
            let mut s = self.st.borrow_mut();
            s.toggle_gen += 1;
            s.leaving = hidden && !removing;
            s.toggle_gen
        };
        if hidden {
            if self.panel.isKeyWindow() {
                self.panel.resignKeyWindow();
            }
            // A leaving island goes at once (its removal orders it out again, harmlessly); it never comes back.
            if removing {
                if !offscreen() {
                    self.panel.orderOut(None);
                }
                return;
            }
            self.toggle_motion(false, delay, reduce);
            let me = self.me();
            sys::after(delay + crate::model::toggle::hide_time(reduce), move || {
                let Some(c) = me.upgrade() else { return };
                if c.st.borrow().toggle_gen != g {
                    return;
                }
                c.st.borrow_mut().leaving = false;
                if !offscreen() {
                    if c.panel.isKeyWindow() {
                        c.panel.resignKeyWindow();
                    }
                    c.panel.orderOut(None);
                }
                // Out of sight: whole again for whatever brings it back (news, or the next show), and still.
                c.toggle_rest();
                c.refresh_timers();
            });
        } else {
            if up {
                self.toggle_rest();
                self.live_viewer_after(0.0);
            } else {
                self.toggle_motion(true, delay, reduce);
            }
            // Its start (nothing, unlit) committed before the window is back, so no frame shows it whole first.
            CATransaction::flush();
            if !offscreen() {
                self.panel.orderFrontRegardless();
                OverlaySpace::adopt(self.panel.windowNumber());
            }
            self.refresh_timers();
        }
    }

    /// Whether its window belongs on screen (`App::on_screen`): all shown, or up for news while hidden.
    fn on_screen(&self) -> bool {
        app().on_screen(&self.id)
    }

    /// The repeating timers follow whether it is on screen (`model::toggle::timers`): at rest nothing draws.
    pub fn refresh_timers(&self) {
        self.refresh_working();
        self.run_clock();
        self.arm_attention();
    }

    /// Lab: which repeating timers are armed now (`model::toggle::Timers`).
    pub fn timers_armed(&self) -> crate::model::toggle::Timers {
        let s = self.st.borrow();
        crate::model::toggle::Timers { work: s.work_timer.is_some(), clock: s.clock_timer.is_some(), attention: s.attention_timer.is_some() }
    }

    /// Whether it is shrinking away now (hidden, but its window not out yet).
    pub fn leaving(&self) -> bool {
        self.st.borrow().leaving
    }

    /// The shrink or the growth: the shape's scale about its centre on a spring, the whole island's opacity on a fade.
    /// Each starts from where it is on screen; one already moving turns around at once instead of waiting its turn.
    fn toggle_motion(&self, showing: bool, delay: f64, reduce: bool) {
        use crate::model::toggle::*;
        // An open viewer's live view sits beside the shape, not in it: a picture of it inside the shape shrinks and
        // fades with the rest, and the live view is back once the growth is over.
        self.freeze_viewer();
        let moving = self.shape.has_anim(TOGGLE_SCALE) || self.root.has_anim("opacity");
        let delay = if moving { 0.0 } else { delay };
        // An explicit start on the shared clock, so every island's turn in the stagger is measured from the same moment.
        let begin = self.root.convertTime_fromLayer(sys::media_time() + delay, None);
        let scale_now = if self.shape.has_anim(TOGGLE_SCALE) {
            shown(&self.shape).key_value("transform.scale").map(|v| unsafe { msg_send![&*v, doubleValue] }).unwrap_or(1.0)
        } else if showing && !reduce {
            GONE_SCALE
        } else {
            1.0
        };
        let alpha_now = if self.root.has_anim("opacity") {
            shown(&self.root).opacity() as f64
        } else if showing {
            0.0
        } else {
            1.0
        };
        let scale_to = if showing || reduce { 1.0 } else { GONE_SCALE };
        let alpha_to = if showing { 1.0 } else { 0.0 };
        quiet(|| {
            self.shape.set_key_value("transform.scale", any(&*num(scale_to)));
            if reduce || (scale_now - scale_to).abs() < 1e-4 {
                self.shape.remove_anim(TOGGLE_SCALE);
            } else {
                let spring = if showing { Spring::GROW } else { Spring { response: HIDE_RESPONSE, damping: 1.0 } };
                let a = spring.animation("transform.scale", any(&*num(scale_now)), any(&*num(scale_to)), 0.0);
                if !showing {
                    // Critically damped, it is within a few hundredths of nothing by then, and already faded out.
                    a.setDuration(hide_time(false));
                }
                a.setBeginTime(begin);
                fill_backwards(&a);
                add(&self.shape, &a, TOGGLE_SCALE);
                if showing {
                    self.room_for_overshoot(delay + a.duration());
                    self.live_viewer_after(delay + a.duration());
                }
            }
            self.root.setOpacity(alpha_to as f32);
            let a = basic("opacity", Some(any(&*num(alpha_now))), Some(any(&*num(alpha_to))), if reduce { PLAIN_FADE } else if showing { SHOW_FADE } else { HIDE_FADE });
            // Coming, it is lit almost at once; going, it stays lit while it shrinks and is gone as it reaches nothing.
            if showing && (reduce || (scale_now - scale_to).abs() < 1e-4) {
                self.live_viewer_after(delay + if reduce { PLAIN_FADE } else { SHOW_FADE });
            }
            let timing = if showing { ease_out() } else { ease_in() };
            a.setTimingFunction(Some(&timing));
            a.setBeginTime(begin);
            fill_backwards(&a);
            add(&self.root, &a, "opacity");
        });
    }

    /// An open viewer's live view out of the window for a hide or show (players keep playing: hidden, the islands
    /// are only off screen). A view whose picture is what is on screen (text, images, audio) is shown as that picture
    /// in the shape, so it shrinks and grows with it; any other (a PDF draws its first page, a video its poster, web
    /// and QuickLook views draw blank) fades out quickly and the shape goes with a plain body.
    fn freeze_viewer(&self) {
        if !(self.is_viewer() && self.expanded()) {
            return;
        }
        let Some(v) = self.file_viewer() else { return };
        let Some(r) = v.root().filter(|r| !r.isHidden() && r.alphaValue() > 0.0) else { return };
        let faithful = matches!(v.kind(), ViewerKind::Markdown | ViewerKind::Text(_) | ViewerKind::Image | ViewerKind::Audio | ViewerKind::Missing);
        if !faithful {
            quiet(|| self.body_host.setOpacity(0.0));
            let r2 = r.clone();
            let block = block2::RcBlock::new(move |ctx: std::ptr::NonNull<objc2_app_kit::NSAnimationContext>| {
                // SAFETY: the context AppKit passes for this group.
                unsafe { ctx.as_ref().setDuration(0.1) };
                let a: Retained<NSView> = unsafe { msg_send![&*r2, animator] };
                a.setAlphaValue(0.0);
            });
            objc2_app_kit::NSAnimationContext::runAnimationGroup(&block);
            return;
        }
        let body = FileViewer::body_rect(self.shape_rect().size());
        let pic = unsafe { self.body_host.sublayers() }.and_then(|subs| subs.firstObject());
        if let (Some(img), Some(l)) = (v.snapshot(body.size()), pic) {
            // SAFETY: a CGImage as contents.
            quiet(|| unsafe { l.setContents(Some(img.object())) });
        }
        r.setHidden(true);
        r.setAlphaValue(0.0);
        quiet(|| self.body_host.setOpacity(1.0));
        self.st.borrow_mut().frozen = true;
    }

    /// The live viewer back once a show's growth is over, if it is still open and still shown.
    fn live_viewer_after(&self, secs: f64) {
        // Open or not yet: one opened meanwhile (an update during the growth) is live after it too.
        if !self.is_viewer() {
            return;
        }
        let g = self.st.borrow().toggle_gen;
        let me = self.me();
        sys::after(secs + 0.06, move || {
            let Some(c) = me.upgrade() else { return };
            let s = c.st.borrow();
            let current = s.toggle_gen == g && !s.leaving && !s.removing && s.expanded;
            drop(s);
            if current && app().on_screen(&c.id) && c.is_viewer() {
                c.show_scroll();
            }
        });
    }

    /// Growing in overshoots its place by a few percent: the window, at rest exactly the shape, gets that much room
    /// on every side (the shape stays where it is on screen) so the overshoot is not cut square, and takes the
    /// shape's size again once the spring is over (`settle`). An island already moving (a morph) has its room.
    fn room_for_overshoot(&self, secs: f64) {
        let (sr, g) = {
            let s = self.st.borrow();
            (s.shape_rect, s.generation)
        };
        let wf = rect_from(self.panel.frame());
        if wf != sr || ["position", "bounds"].iter().any(|k| self.shape.has_anim(k)) {
            return;
        }
        let m = (0.05 * sr.w.max(sr.h)).ceil() + 2.0;
        let room = sr.inset(-m, -m);
        self.panel.setFrame_display(ns_rect(room), false);
        self.view.setFrame(ns_rect(Rect::new(0.0, 0.0, room.w, room.h)));
        let t = local(sr, room);
        self.shape.setPosition(pt(t.mid_x(), t.mid_y()));
        let me = self.me();
        sys::after(secs + 0.05, move || {
            if let Some(c) = me.upgrade() {
                c.settle(g);
            }
        });
    }

    /// Whole and still: full size, fully lit, no hide or show motion left.
    fn toggle_rest(&self) {
        quiet(|| {
            self.shape.remove_anim(crate::model::toggle::TOGGLE_SCALE);
            self.shape.set_key_value("transform.scale", any(&*num(1.0)));
            self.root.remove_anim("opacity");
            self.root.setOpacity(1.0);
        });
    }

    pub fn remove(self: &Rc<Self>, done: impl Fn() + 'static) {
        let r = self.visible_shape_rect();
        // A viewer opened from an island shrinks back into that island while it is on screen (Quick Look style).
        let back = self.st.borrow().born_island.clone()
            .and_then(|id| app().ctl(&id).filter(|c| app().on_screen(&c.id) && !c.leaving()).map(|c| c.visible_shape_rect()));
        let dot = back.unwrap_or_else(|| Rect::new(r.mid_x() - PILL_H / 2.0, r.max_y() - PILL_H, PILL_H, PILL_H));
        let spring = if back.is_some() { Spring::ZOOM } else { Spring::CALM };
        fade(&self.header, 0.0, 0.1, 0.0);
        fade(&self.body_host, 0.0, 0.1, 0.0);
        if let Some(t) = self.st.borrow().trailing_layer.clone() {
            fade(&t, 0.0, 0.1, 0.0);
        }
        self.morph(dot, spring, None, None);
        self.st.borrow_mut().removing = true;
        self.reset_viewer();
        // An open viewer's outside-click monitor goes with it (dropping it does not unregister it).
        if let Some(m) = self.st.borrow_mut().outside_monitor.take() {
            // SAFETY: a monitor we added.
            unsafe { NSEvent::removeMonitor(&m) };
        }
        {
            let mut s = self.st.borrow_mut();
            s.clock_timer = None;
            s.attention_timer = None;
            s.work_timer = None;
        }
        let panel = self.panel.clone();
        transaction(
            false,
            Some(Box::new(move || {
                panel.orderOut(None);
                done();
            })),
            || fade(&self.shape, 0.0, 0.22, 0.1),
        );
    }

    // MARK: Pointer

    /// A view point in body coordinates (scroll included); `None` outside the visible body.
    fn body_point(&self, p: CGPoint) -> Option<Point> {
        let f = self.shape.frame();
        let origin = (f.origin.x + PAD, f.origin.y + BODY_TOP);
        let v = (p.x - origin.0, p.y - origin.1);
        if self.scrolls() && (v.1 < -4.0 || v.1 > self.viewport_height() + 4.0) {
            return None;
        }
        Some(Point::new(v.0, v.1 + self.st.borrow().scroll_y))
    }

    fn control_at(&self, p: CGPoint) -> Option<(Control, Point)> {
        if !self.expanded() || self.body_host.opacity() <= 0.0 {
            return None;
        }
        let q = self.body_point(p)?;
        let s = self.st.borrow();
        let body = s.body.as_ref()?;
        if let Some((_, row, _)) = &s.confirming {
            if confirm::hit(row, q, sys::uptime()) != Hit::Outside {
                return None;
            }
        }
        body.regions.iter().find(|r| r.rect.contains(q)).map(|r| (r.control.clone(), q))
    }

    /// The pointer in screen coordinates at the event's time (the event's own global location; the window moves under
    /// the pointer during a drag).
    fn screen_point(&self, e: &NSEvent) -> Point {
        if let Some(cg) = e.CGEvent() {
            if let Some(h) = NSScreen::screens(mtm()).iter().next().map(|s| s.frame().size.height) {
                let l = objc2_core_graphics::CGEvent::location(Some(&cg));
                return Point::new(l.x, h - l.y);
            }
        }
        let p = self.panel.convertPointToScreen(e.locationInWindow());
        Point::new(p.x, p.y)
    }

    fn view_point(&self, e: &NSEvent) -> CGPoint {
        self.view.convertPoint_fromView(e.locationInWindow(), None)
    }

    pub fn mouse_down(self: &Rc<Self>, e: &NSEvent) {
        // Shrinking away (every island hidden): not there to press or carry.
        if self.leaving() {
            self.st.borrow_mut().down_screen = None;
            return;
        }
        let p = self.view_point(e);
        let sf = self.shape.frame();
        if self.expanded() {
            let close = self.st.borrow().close_layer.clone();
            if let Some(c) = close {
                let cf = rect_of(c.frame()).inset(-6.0, -6.0).offset(sf.origin.x, sf.origin.y);
                if c.opacity() > 0.0 && cf.contains(Point::new(p.x, p.y)) {
                    self.st.borrow_mut().down_screen = None;
                    self.close_island();
                    return;
                }
            }
        }
        if let Some(h) = self.shown_share() {
            if h.frame_in_view(sf).inset(-2.0, -2.0).contains(Point::new(p.x, p.y)) {
                self.st.borrow_mut().down_screen = None;
                h.press(Point::new(p.x, p.y));
                return;
            }
        }
        if self.is_viewer() && self.expanded() && p.y - sf.origin.y > PILL_H + 8.0 {
            self.st.borrow_mut().down_screen = None;
            return;
        }
        let sp = self.screen_point(e);
        {
            let mut s = self.st.borrow_mut();
            s.down_screen = Some(sp);
            s.samples = vec![(e.timestamp(), sp)];
            s.down_anchor = s.anchor;
            s.dragging = false;
            s.pressed = None;
            s.slider_drag = None;
        }
        if let Some((c, q)) = self.control_at(p) {
            if let Control::Slider { id, .. } = &c {
                self.st.borrow_mut().slider_drag = Some(id.clone());
                self.move_slider(id, q.x, false);
            } else {
                self.st.borrow_mut().pressed = Some(c.clone());
                self.show_press(Some(&c));
            }
        }
    }

    pub fn mouse_dragged(self: &Rc<Self>, e: &NSEvent) {
        if self.leaving() {
            return;
        }
        let share = self.st.borrow().share.clone().filter(|h| h.pressed());
        if let Some(h) = share {
            let p = self.view_point(e);
            h.dragged(&self.view, e, Point::new(p.x, p.y), h.frame_in_view(self.shape.frame()));
            return;
        }
        let Some(start) = self.st.borrow().down_screen else { return };
        let p = self.screen_point(e);
        let slider = self.st.borrow().slider_drag.clone();
        if let Some(id) = slider {
            let q = self.slider_point(e);
            self.move_slider(&id, q.x, false);
            return;
        }
        if !self.st.borrow().dragging && (p.x - start.x).hypot(p.y - start.y) < 3.0 {
            return;
        }
        self.show_press(None);
        {
            let mut s = self.st.borrow_mut();
            if !s.dragging {
                s.dragging = true;
                s.down_pill_anchor = s.anchor;
                s.down_anchor = Point::new(s.shape_rect.mid_x(), s.shape_rect.max_y());
            }
            s.pressed = None;
            s.samples.push((e.timestamp(), p));
            let n = s.samples.len();
            if n > 12 {
                s.samples.drain(0..n - 12);
            }
            s.anchor = Point::new(s.down_anchor.x + p.x - start.x, s.down_anchor.y + p.y - start.y);
            s.generation += 1;
        }
        self.shape.removeAllAnimations();
        let size = self.size();
        let a = self.anchor();
        let r = Rect::new((a.x - size.w / 2.0).round(), (a.y - size.h).round(), size.w, size.h);
        self.st.borrow_mut().shape_rect = r;
        quiet(|| {
            self.panel.setFrame_display(ns_rect(r), false);
            self.view.setFrame(ns_rect(Rect::new(0.0, 0.0, r.w, r.h)));
            self.shape.setBounds(cgr(Rect::new(0.0, 0.0, r.w, r.h)));
            self.shape.setPosition(pt(r.w / 2.0, r.h / 2.0));
        });
    }

    /// Points per second (y up) over the last 80 ms of the carry; a hand that stopped before letting go throws nothing.
    fn release_velocity(&self, t: f64, p: Point) -> Vector {
        let samples = std::mem::take(&mut self.st.borrow_mut().samples);
        let Some(last) = samples.last() else { return Vector::ZERO };
        if t - last.0 >= 0.06 {
            return Vector::ZERO;
        }
        let mut pts: Vec<(f64, Point)> = samples.iter().copied().filter(|s| t - s.0 <= 0.08).collect();
        pts.push((t, p));
        let (first, end) = (pts[0], pts[pts.len() - 1]);
        if end.0 - first.0 <= 0.008 {
            return Vector::ZERO;
        }
        let dt = end.0 - first.0;
        Vector::new((end.1.x - first.1.x) / dt, (end.1.y - first.1.y) / dt)
    }

    fn slider_point(&self, e: &NSEvent) -> Point {
        let p = self.view_point(e);
        let f = self.shape.frame();
        Point::new(p.x - (f.origin.x + PAD), p.y - (f.origin.y + BODY_TOP))
    }

    pub fn mouse_up(self: &Rc<Self>, e: &NSEvent) {
        let share = self.st.borrow().share.clone();
        if share.is_some_and(|h| h.released()) {
            return;
        }
        self.mouse_up_inner(e);
        {
            let mut s = self.st.borrow_mut();
            s.down_screen = None;
            s.dragging = false;
            s.pressed = None;
            s.slider_drag = None;
        }
        self.show_press(None);
    }

    fn mouse_up_inner(self: &Rc<Self>, e: &NSEvent) {
        if self.st.borrow().down_screen.is_none() {
            return;
        }
        let slider = self.st.borrow().slider_drag.clone();
        if let Some(id) = slider {
            let q = self.slider_point(e);
            self.move_slider(&id, q.x, true);
            return;
        }
        let dragging = self.st.borrow().dragging;
        if dragging && self.is_viewer() && self.expanded() {
            let sr = {
                let mut s = self.st.borrow_mut();
                s.dragging = false;
                s.anchor = s.down_pill_anchor;
                s.shape_rect
            };
            let scr = screen_rects().into_iter().find(|f| f.intersects(&sr)).unwrap_or(self.st.borrow().screen_frame);
            let pos = crate::jobj! {"x": sr.mid_x() - scr.mid_x(), "y": scr.max_y() - sr.max_y()};
            if let Some(o) = IslandFile::patch(&self.id, |o| {
                o.insert("openPosition".into(), Value::Object(pos.clone()));
            }) {
                self.update(Island::new(&self.id, o));
            }
            app().state_changed();
            return;
        }
        if dragging {
            self.st.borrow_mut().dragging = false;
            let v = self.release_velocity(e.timestamp(), self.screen_point(e));
            self.st.borrow_mut().pending_velocity = Some(v);
            let sr = self.shape_rect();
            if let Some(o) = app().drop_island(self, sr, v) {
                self.update(Island::new(&self.id, o));
            }
            app().layout_now(Spring::SNAP);
            CATransaction::flush();
            return;
        }
        let vp = self.view_point(e);
        let confirming = self.st.borrow().confirming.as_ref().map(|c| c.1.clone());
        if let (Some(row), Some(q)) = (confirming, self.body_point(vp)) {
            match confirm::hit(&row, q, sys::uptime()) {
                Hit::Confirm => {
                    self.finish_confirm();
                    return;
                }
                Hit::Cancel => {
                    self.cancel_confirm();
                    return;
                }
                Hit::Ignored => return,
                Hit::Outside => self.cancel_confirm(),
            }
        }
        let pressed = self.st.borrow().pressed.clone();
        if let Some(c) = pressed {
            if let Some((now, _)) = self.control_at(vp) {
                if c.same(&now) {
                    self.perform(c);
                    return;
                }
            }
        }
        if self.expanded() && vp.y - self.shape.frame().origin.y > PILL_H {
            return;
        }
        let keep = !self.expanded();
        if let Some(o) = IslandFile::patch(&self.id, |o| {
            o.insert("expanded".into(), Value::Bool(keep));
        }) {
            self.update(Island::new(&self.id, o));
        }
    }

    fn show_press(&self, c: Option<&Control>) {
        if let Some(l) = self.st.borrow_mut().press_layer.take() {
            l.removeFromSuperlayer();
        }
        let Some(c) = c else { return };
        let (r, bl) = {
            let s = self.st.borrow();
            let Some(body) = &s.body else { return };
            let Some(r) = body.regions.iter().find(|r| r.control.same(c)).map(|r| r.rect) else { return };
            (r, body.layer.clone())
        };
        let button = matches!(c, Control::Button { .. });
        let f = if button { r } else { r.inset(-6.0, 0.0) };
        let l = boxed(f.x, f.y, f.w, f.h, &*if button { gray(0.0, 0.18) } else { white(0.1) }, if button { r.h / 2.0 } else { 8.0 });
        bl.addSublayer(&l);
        self.st.borrow_mut().press_layer = Some(l);
    }

    /// Open the island's own target from its menu: the viewer grows out of the island itself.
    pub fn open_target(&self, target: &str) {
        let frame = self.st.borrow().placed.then(|| self.visible_shape_rect());
        self.open_target_from(target, frame, None)
    }

    /// What the island shows in `r` (screen points, y up) now, drawn at the screen's scale.
    pub fn snapshot(&self, r: Rect) -> Option<draw::Image> {
        let s = self.scale();
        let b = draw::Bitmap::new((r.w * s).ceil() as usize, (r.h * s).ceil() as usize)?;
        let f = rect_from(self.panel.frame());
        b.scale(s, s);
        b.translate(f.min_x() - r.min_x(), f.max_y() - r.min_y());
        b.scale(1.0, -1.0);
        shown(&self.root).renderInContext(b.cg());
        b.image()
    }

    /// An element's body rect on screen (`model::island::body_rect_on_screen` with this island's shape and window).
    pub fn body_rect_on_screen(&self, r: Rect) -> Rect {
        let f = self.shape.frame();
        let shape = Rect::new(f.origin.x, f.origin.y, f.size.width, f.size.height);
        crate::model::island::body_rect_on_screen(r, shape, rect_from(self.panel.frame()), PAD, BODY_TOP, self.st.borrow().scroll_y)
    }

    /// Open a file Heimdall's viewer can show in a viewer island that grows out of `frame` (the clicked element on
    /// screen, its picture `image` on the first frame), a web page or any other file in the default browser or app.
    /// Lab runs (FI_OPEN_LOG) write what would open instead (the viewer island is still put, in the lab's folder).
    pub fn open_target_from(&self, target: &str, frame: Option<Rect>, image: Option<&str>) {
        let Some((route, origin)) = crate::model::island::open_request(target, &self.id, frame, image) else { return };
        // A page bound before it exists (a running task's report): nothing to open yet.
        if OpenTarget::missing(target) {
            return;
        }
        IslandFile::respond(&self.id, "open", Some("open"), Some(Value::from(target)));
        let url = match route {
            OpenRoute::Viewer(path) => {
                // No picture to show first: the clicked element as it looks now.
                if let (Some(o), None) = (&origin, image) {
                    if let Some(img) = self.snapshot(o.rect) {
                        SNAPSHOTS.with(|s| s.borrow_mut().insert(path.clone(), img));
                    }
                }
                IslandFile::view_from(&path, None, None, vec![], origin.map(|o| o.json()));
                Self::log_open(&format!("{path}\tviewer\n"));
                return;
            }
            OpenRoute::Reveal(path) => {
                // Selected in Finder, never opened (a bundle would launch).
                Self::log_open(&format!("{path}\treveal\n"));
                if !(std::env::var("FI_OPEN_LOG").is_ok() && std::env::var("FLOATING_ISLANDS_DIR").is_ok()) {
                    let url = objc2_foundation::NSURL::fileURLWithPath(&ns(&path));
                    objc2_app_kit::NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
                }
                return;
            }
            OpenRoute::System(url) => url,
        };
        let s = OpenTarget::absolute_string(&url);
        let nsurl: Option<Retained<objc2_foundation::NSURL>> = match &url {
            crate::model::island::Target::File(p) => Some(objc2_foundation::NSURL::fileURLWithPath(&ns(p))),
            crate::model::island::Target::Url(u) => objc2_foundation::NSURL::URLWithString(&ns(u)),
        };
        let Some(nsurl) = nsurl else { return };
        let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
        if let (Ok(_), true) = (std::env::var("FI_OPEN_LOG"), std::env::var("FLOATING_ISLANDS_DIR").is_ok()) {
            let app_name = ws
                .URLForApplicationToOpenURL(&nsurl)
                .and_then(|u| u.lastPathComponent())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "none".into());
            Self::log_open(&format!("{s}\t{app_name}\n"));
            return;
        }
        ws.openURL(&nsurl);
    }

    /// One line in the lab's FI_OPEN_LOG (only with FLOATING_ISLANDS_DIR set).
    fn log_open(line: &str) {
        let (Ok(log), true) = (std::env::var("FI_OPEN_LOG"), std::env::var("FLOATING_ISLANDS_DIR").is_ok()) else { return };
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// A click on a control. An answer to the agent asks "Sure?" first.
    fn perform(self: &Rc<Self>, c: Control) {
        let answer = match &c {
            Control::Button { id, .. } => Some(id.clone()),
            Control::Approve { action, .. } | Control::Waiting { action, .. } => Some(action.clone()),
            Control::ListRow { id, .. } => Some(id.clone()),
            _ => None,
        };
        if answer.is_some_and(|a| confirm::needed(&a)) {
            self.ask_confirm(c);
        } else {
            self.commit(c);
        }
    }

    fn ask_confirm(self: &Rc<Self>, c: Control) {
        self.cancel_confirm();
        let Some(row) = self.confirm_row(&c) else {
            self.commit(c);
            return;
        };
        if let Some(bl) = self.body_layer() {
            bl.addSublayer(&row.layer);
        }
        let me = self.me();
        let w = sys::after_cancellable(confirm::WINDOW, move || {
            if let Some(c) = me.upgrade() {
                c.cancel_confirm();
            }
        });
        let r = self.st.borrow().body.as_ref().and_then(|b| b.regions.iter().find(|r| r.control.same(&c)).map(|r| r.rect));
        self.st.borrow_mut().confirming = Some((c, row, w));
        if let Some(r) = r {
            self.scroll_to_show(r);
        }
    }

    /// The clicked button's row: every button beside it (for a proposal, its own actions).
    fn confirm_row(&self, c: &Control) -> Option<confirm::Row> {
        let s = self.st.borrow();
        let body = s.body.as_ref()?;
        let clicked = body.regions.iter().find(|r| r.control.same(c))?.rect;
        let row = body
            .regions
            .iter()
            .filter(|r| match (c, &r.control) {
                (Control::Button { .. }, Control::Button { .. }) => (r.rect.min_y() - clicked.min_y()).abs() < 1.0,
                (Control::Approve { key: k, .. }, Control::Approve { key: k2, .. }) | (Control::Waiting { id: k, .. }, Control::Waiting { id: k2, .. }) => k == k2,
                _ => false,
            })
            .map(|r| r.rect)
            .fold(clicked, |a, b| a.union(&b));
        // Goals' quick answers are small chips: "Sure? Cancel Confirm" takes the ask's whole text column.
        let mut row = row;
        if matches!(c, Control::Approve { .. }) && s.island.list().as_deref() == Some("goals") {
            let right = self.expanded_width() - 2.0 * PAD - 10.0;
            row.w = row.w.max(right - row.x);
        }
        let bg = if matches!(c, Control::Approve { .. }) { gray(0.08, 1.0) } else { black() };
        drop(s);
        Some(confirm::row(row, clicked, &bg, self.scale()))
    }

    fn finish_confirm(self: &Rc<Self>) {
        let Some((c, row, w)) = self.st.borrow_mut().confirming.take() else { return };
        w.cancel();
        confirm::dismiss(&row);
        self.commit(c);
    }

    fn cancel_confirm(&self) {
        let Some((_, row, w)) = self.st.borrow_mut().confirming.take() else { return };
        w.cancel();
        confirm::dismiss(&row);
    }

    /// After a rebuild the row comes back over the same button, or goes with it.
    fn reattach_confirm(&self) {
        let Some((c, old, w)) = self.st.borrow_mut().confirming.take() else { return };
        let Some(row) = self.confirm_row(&c) else {
            w.cancel();
            return;
        };
        quiet(|| row.layer.removeAllAnimations());
        if let Some(bl) = self.body_layer() {
            bl.addSublayer(&row.layer);
        }
        let row = confirm::Row { started: old.started, ..row };
        self.st.borrow_mut().confirming = Some((c, row, w));
    }

    pub fn is_confirming(&self) -> bool {
        self.st.borrow().confirming.is_some()
    }
    pub fn lab_confirm(self: &Rc<Self>, yes: bool) {
        if yes {
            self.finish_confirm();
        } else {
            self.cancel_confirm();
        }
    }
    /// Lab: where the Sure? row's Cancel or Confirm is (window coordinates).
    /// Lab: show the frozen opening motion `t` seconds in (`FI_LAB_SCRUB`). False when it was not frozen.
    pub fn lab_scrub(&self, t: f64) -> bool {
        let Some(t0) = self.st.borrow().lab_t0 else { return false };
        self.root.setTimeOffset(t0 + t);
        CATransaction::flush();
        true
    }

    /// Lab: let a frozen opening motion run on (its animations are over by now).
    pub fn lab_unfreeze(&self) {
        if self.st.borrow_mut().lab_t0.take().is_some() {
            // Offscreen the render server plays nothing: the motion's end state is the layers' own values.
            fn clear(l: &CALayer) {
                l.removeAllAnimations();
                // SAFETY: reading the sublayer list on the main thread.
                if let Some(subs) = unsafe { l.sublayers() } {
                    for s in subs.iter() { clear(&s) }
                }
            }
            clear(&self.root);
            self.root.setSpeed(1.0);
            self.root.setTimeOffset(0.0);
            if let Some(g) = self.st.borrow_mut().ghost.take() { g.removeFromSuperlayer() }
            CATransaction::flush();
        }
    }

    /// Lab: click the first picture on the open island, as a pointer click would (its frame, its image). False when
    /// there is none.
    pub fn lab_click_picture(self: &Rc<Self>) -> bool { self.lab_click_open(true) }

    /// Lab: click the island's open chip (a row with no picture).
    pub fn lab_click_chip(self: &Rc<Self>) -> bool { self.lab_click_open(false) }

    fn lab_click_open(self: &Rc<Self>, picture: bool) -> bool {
        let c = {
            let s = self.st.borrow();
            s.body.as_ref().and_then(|b| b.regions.iter().find(|r| matches!(&r.control, Control::Open { image, .. } if image.is_some() == picture)).map(|r| r.control.clone()))
        };
        match c {
            Some(c) => { self.commit(c); true }
            None => false,
        }
    }

    pub fn lab_confirm_centre(&self, cancel: bool) -> Option<CGPoint> {
        let s = self.st.borrow();
        let (_, row, _) = s.confirming.as_ref()?;
        let r = if cancel { row.cancel } else { row.confirm };
        let f = self.shape.frame();
        let v = pt(f.origin.x + PAD + r.mid_x(), f.origin.y + BODY_TOP - s.scroll_y + r.mid_y());
        Some(self.view.convertPoint_toView(v, None))
    }

    fn commit(self: &Rc<Self>, c: Control) {
        let id = self.id.clone();
        let mut new: Option<Json> = None;
        match c {
            Control::Button { id: action, label } => {
                IslandFile::respond(&id, "button", Some(&action), Some(Value::from(label)));
                new = IslandFile::patch(&id, |o| {
                    o.insert("chosen".into(), Value::from(action.clone()));
                });
                if self.island().look() == "waiting" {
                    let id = id.clone();
                    sys::after(0.6, move || {
                        if IslandFile::read(&Paths::file(&id)).and_then(|o| j::str(o.get("chosen")).map(str::to_owned)).as_deref() == Some(action.as_str()) {
                            let _ = std::fs::remove_file(Paths::file(&id));
                        }
                    });
                }
            }
            Control::ListRow { id: item, title } => {
                IslandFile::respond(&id, "list", Some(&item), Some(Value::from(title)));
                new = IslandFile::patch(&id, |o| map_parts(o, "list", None, |p| {
                    p.insert("selected".into(), Value::from(item.clone()));
                }));
            }
            Control::Toggle { id: tid, on } => {
                IslandFile::respond(&id, "toggle", Some(&tid), Some(Value::Bool(!on)));
                new = IslandFile::patch(&id, |o| map_parts(o, "toggle", Some(&tid), |p| {
                    p.insert("on".into(), Value::Bool(!on));
                }));
            }
            Control::Open { target, from, image } => {
                let frame = from.map(|r| self.body_rect_on_screen(r));
                self.open_target_from(&target, frame, image.as_deref())
            }
            Control::Tick { key, .. } => {
                let mut result: Option<(String, bool)> = None;
                new = IslandFile::patch(&id, |o| {
                    let Some(it) = Plan::items(o).into_iter().find(|i| i.key == key) else { return };
                    let Some(Value::Array(items)) = o.get_mut("items") else { return };
                    let mut obj = items[it.index].as_object().cloned().unwrap_or_else(|| crate::jobj! {"title": it.title.clone()});
                    let d = !it.done;
                    if d {
                        obj.insert("done".into(), Value::Bool(true));
                    } else {
                        obj.remove("done");
                    }
                    items[it.index] = Value::Object(obj);
                    result = Some((it.id.clone().unwrap_or(it.title.clone()), d));
                });
                if let Some((item, done)) = result {
                    IslandFile::respond_object(&Responses::tick(&id, &item, done));
                }
            }
            Control::Disclose { key } => {
                {
                    let mut s = self.st.borrow_mut();
                    if !s.open_items.remove(&key) {
                        s.open_items.insert(key.clone());
                    }
                }
                self.rebuild_plan();
                if self.st.borrow().open_items.contains(&key) {
                    let frames = self.st.borrow().body.as_ref().and_then(|b| {
                        let r = rect_of(b.rows.get(&format!("row:{key}"))?.0.frame());
                        Some(b.rows.get(&format!("prop:{key}")).map(|p| r.union(&rect_of(p.0.frame()))).unwrap_or(r))
                    });
                    if let Some(r) = frames {
                        self.scroll_to_show(r);
                    }
                }
            }
            Control::Approve { key, action } => {
                let mut item_id: Option<String> = None;
                let mut label: Option<String> = None;
                new = IslandFile::patch(&id, |o| {
                    let Some(it) = Plan::items(o).into_iter().find(|i| i.key == key) else { return };
                    let Some(Value::Array(items)) = o.get_mut("items") else { return };
                    let Some(obj) = items[it.index].as_object_mut() else { return };
                    obj.insert("chosen".into(), Value::from(action.clone()));
                    item_id = Some(it.id.clone().unwrap_or(it.title.clone()));
                    label = it.proposal.as_ref().and_then(|p| p.actions.iter().find(|a| a.1 == action).map(|a| a.0.clone()));
                });
                if let Some(item) = item_id {
                    let mut r = Responses::approval(&id, &item, &action);
                    if let Some(l) = label {
                        r.insert("label".into(), Value::from(l));
                    }
                    IslandFile::respond_object(&r);
                }
                // Goals: the answered ask stays to show the choice for as long, then leaves (it no longer needs the user).
                let answered = format!("answered:{key}");
                self.st.borrow_mut().open_items.insert(answered.clone());
                let me = self.me();
                sys::after(1.4, move || {
                    let Some(c) = me.upgrade() else { return };
                    let a = c.st.borrow_mut().open_items.remove(&answered);
                    if c.st.borrow_mut().open_items.remove(&key) || (a && c.island().list().as_deref() == Some("goals")) {
                        c.rebuild_plan();
                    }
                });
            }
            Control::Details => {
                // As the island's own `expanded`: under the folder lock, temp file + rename.
                new = IslandFile::patch(&id, |o| {
                    let open = o.get("details").and_then(Value::as_bool) == Some(true);
                    o.insert("details".into(), Value::Bool(!open));
                });
            }
            Control::GoalToggle { key } => {
                // Written as the app writes the island's own `expanded`: under the folder lock, temp file + rename.
                new = IslandFile::patch(&id, |o| {
                    crate::model::Goals::toggle_expanded(o, &key);
                });
            }
            Control::Yes { list, id: pid } => {
                // One word or one click (`docs/one-system.md`): no "Sure?"; Heimdall fills the ghost in where it sits.
                IslandFile::respond_object(&crate::jobj! {"island": list, "kind": "approval", "action": "yes", "value": pid.clone()});
                let mark = format!("yes:{pid}");
                self.st.borrow_mut().open_items.insert(mark.clone());
                self.rebuild_plan();
                // Heimdall fills it in within a moment; one it could not take (six goals already, its goal gone) stays
                // a proposal, so the chip shows unanswered again (review 2 Nov).
                let me = self.me();
                sys::after(4.0, move || {
                    let Some(c) = me.upgrade() else { return };
                    if c.st.borrow_mut().open_items.remove(&mark) { c.rebuild_plan(); }
                });
            }
            Control::Waiting { id: wid, action, label } => {
                IslandFile::respond_object(&crate::jobj! {"island": "scheduled", "kind": "waiting", "action": action.clone(), "value": wid.clone(), "label": label});
                self.st.borrow_mut().open_items.insert(format!("chose:{wid}:{action}"));
                self.rebuild_plan();
            }
            Control::Proof { key, open } => {
                let k = format!("proof:{key}");
                let opened = {
                    let mut s = self.st.borrow_mut();
                    if s.open_items.remove(&k) { false } else { s.open_items.insert(k); true }
                };
                self.rebuild_plan();
                // A file proof also opens, the way the open button does (an `open` response, the viewer).
                if let (true, Some(t)) = (opened, open) {
                    self.open_target(&t);
                }
            }
            Control::Slider { .. } => {}
        }
        if let Some(n) = new {
            self.update(Island::new(&self.id, n));
        }
    }

    fn move_slider(self: &Rc<Self>, sid: &str, x: f64, commit: bool) {
        let island = self.island();
        let Some(part) = island.parts().into_iter().find(|p| j::str(p.get("type")) == Some("slider") && j::str(p.get("id")).unwrap_or("slider") == sid) else {
            return;
        };
        let (lo, hi) = super::parts::slider_bounds(j::num(part.get("min")), j::num(part.get("max")));
        let step = j::num(part.get("step")).filter(|s| s.is_finite()).unwrap_or(0.0);
        let parts = {
            let s = self.st.borrow();
            s.body.as_ref().and_then(|b| b.sliders.get(sid)).map(|sp| (sp.track, sp.fill.clone(), sp.knob.clone(), sp.label.clone(), sp.unit.clone()))
        };
        let Some((track, fill, knob, label, unit)) = parts else { return };
        let mut v = super::parts::slider_value(lo, hi, x / track.w);
        if step > 0.0 {
            v = ((v / step).round() * step).clamp(lo, hi);
        }
        let frac = super::parts::slider_fraction(lo, hi, v);
        quiet(|| {
            let mut b = fill.bounds();
            b.size.width = (track.w * frac).max(4.0);
            fill.setBounds(b);
            let mut p = knob.position();
            p.x = track.w * frac - 9.0;
            knob.setPosition(p);
            if let Some(l) = &label {
                let text = crate::model::format_number(v) + &*if unit.is_empty() { String::new() } else { format!(" {unit}") };
                super::parts::retext_plain(l, &text, &small_mono(), &white(1.0));
            }
        });
        if !commit {
            return;
        }
        IslandFile::respond(&self.id, "slider", Some(sid), Some(j::number(v)));
        if let Some(o) = IslandFile::patch(&self.id, |o| map_parts(o, "slider", Some(sid), |p| {
            p.insert("value".into(), j::number(v));
        })) {
            self.st.borrow_mut().island = Island::new(&self.id, o);
        }
    }

    // MARK: Lab hooks

    /// The close button's centre in window coordinates (`None` while it is hidden).
    pub fn close_centre(&self) -> Option<CGPoint> {
        if !self.expanded() {
            return None;
        }
        let c = self.st.borrow().close_layer.clone()?;
        if c.opacity() <= 0.0 {
            return None;
        }
        let f = self.shape.frame();
        Some(self.view.convertPoint_toView(pt(f.origin.x + c.position().x, f.origin.y + c.position().y), None))
    }

    /// The strings the open body shows.
    pub fn body_texts(&self) -> Vec<String> {
        fn walk(l: &CALayer, out: &mut Vec<String>) {
            if is_text_layer(l) {
                // SAFETY: checked class.
                let t: &CATextLayer = unsafe { &*(l as *const CALayer as *const CATextLayer) };
                if let Some(s) = text_of(t) {
                    out.push(s);
                    return;
                }
            }
            if let Some(subs) = unsafe { l.sublayers() } {
                for s in subs.iter() {
                    walk(&s, out);
                }
            }
        }
        let mut out = vec![];
        walk(&self.body_host, &mut out);
        out
    }

    /// Layers with an animation still running or looping (empty at rest).
    pub fn animated_layers(&self) -> Vec<String> {
        fn walk(l: &CALayer, out: &mut Vec<String>) {
            let now = l.convertTime_fromLayer(sys::media_time(), None);
            if let Some(keys) = l.animationKeys() {
                for k in keys.iter() {
                    // SAFETY: reading the layer's own animation.
                    let Some(a) = (unsafe { l.animationForKey(&k) }) else { continue };
                    let (begin, dur, rep, rep_dur, auto) = media(&a);
                    let live = rep.is_infinite() || rep_dur.is_infinite() || {
                        let end = begin + dur * (rep.max(1.0) as f64) * if auto { 2.0 } else { 1.0 };
                        end > now
                    };
                    if live {
                        let cls = l.class().name().to_string_lossy().into_owned();
                        out.push(format!("{cls}.{k}"));
                    }
                }
            }
            if let Some(subs) = unsafe { l.sublayers() } {
                for s in subs.iter() {
                    walk(&s, out);
                }
            }
        }
        let mut out = vec![];
        walk(&self.root, &mut out);
        out
    }

    /// Lab: what the open body looks like now (render debugging).
    pub fn debug_state(&self) -> String {
        let bl = self.body_layer();
        let subs = bl.as_ref().and_then(|b| unsafe { b.sublayers() }).map(|s| s.iter().map(|l| format!("{:.2}/{:.2}", l.opacity(), shown(&l).opacity())).collect::<Vec<_>>().join(" ")).unwrap_or_default();
        let g = shown(&self.body_group);
        format!(
            "host op {} (shown {}), host sublayers {}, group scale {:?}, body in host {}, shape {:?}, rows: {subs}",
            self.body_host.opacity(),
            shown(&self.body_host).opacity(),
            unsafe { self.body_host.sublayers() }.map(|s| s.count()).unwrap_or(0),
            g.key_value("transform.scale").map(|v| { let d: f64 = unsafe { msg_send![&*v, doubleValue] }; d }),
            bl.as_ref().and_then(|b| b.superlayer()).is_some(),
            shown(&self.shape).bounds().size,
        )
    }

    pub fn lab_perform(self: &Rc<Self>, c: Control) {
        self.commit(c);
    }
    pub fn lab_click(self: &Rc<Self>, c: Control) {
        self.perform(c);
    }

    /// A plan row's place in the body by key (`row:<key>`, `hdr:<group>`, `prop:<key>`).
    pub fn row_frame(&self, key: &str) -> Option<Rect> {
        self.st.borrow().body.as_ref()?.rows.get(key).map(|r| rect_of(r.0.frame()))
    }

    /// Keys of the open body's plan rows, top to bottom.
    pub fn row_order(&self) -> Vec<String> {
        let s = self.st.borrow();
        let Some(b) = &s.body else { return vec![] };
        let mut rows: Vec<(f64, String)> =
            b.rows.iter().filter(|(k, _)| k.starts_with("row:")).map(|(k, r)| (r.0.frame().origin.y, k[4..].to_owned())).collect();
        rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        rows.into_iter().map(|r| r.1).collect()
    }

    /// A control's centre in window coordinates.
    pub fn control_centre(&self, m: impl Fn(&Control) -> bool) -> Option<CGPoint> {
        let s = self.st.borrow();
        let r = s.body.as_ref()?.regions.iter().find(|r| m(&r.control))?.rect;
        let f = self.shape.frame();
        let v = pt(f.origin.x + PAD + r.mid_x(), f.origin.y + BODY_TOP - s.scroll_y + r.mid_y());
        drop(s);
        Some(self.view.convertPoint_toView(v, None))
    }

    // MARK: Viewer: a file shown large in the middle of the screen, closed by Esc, a click outside, its X or the CLI.

    fn load_viewer(self: &Rc<Self>) -> Rc<FileViewer> {
        if let Some(v) = self.st.borrow().file_viewer.clone() {
            return v;
        }
        let island = self.island();
        let v = FileViewer::new(&island.path().unwrap_or_default(), island.viewer_actions());
        let id = self.id.clone();
        v.set_on_action(Box::new(move |a| {
            IslandFile::respond(&id, "button", Some(&a.action), None);
        }));
        let rid = self.id.clone();
        v.set_on_report(Rc::new(move |action, value, label| {
            let mut extra = Json::new();
            extra.insert("value".into(), value);
            if let Some(l) = label {
                extra.insert("label".into(), Value::from(l));
            }
            let mut o = crate::jobj! {"island": rid.clone(), "kind": "report", "action": action};
            o.extend(extra);
            IslandFile::respond_object(&o);
        }));
        let me = self.me();
        v.set_on_resize(Box::new(move || {
            let Some(c) = me.upgrade() else { return };
            if !c.is_viewer() || !c.expanded() || !c.st.borrow().placed {
                return;
            }
            let r = c.rect_for(c.size());
            c.morph(r, Spring::GROW, None, None);
        }));
        self.st.borrow_mut().file_viewer = Some(v.clone());
        v
    }

    pub fn file_viewer(&self) -> Option<Rc<FileViewer>> {
        self.st.borrow().file_viewer.clone()
    }

    pub fn viewer_size(&self) -> Size {
        let me = self.me().upgrade().expect("live controller");
        let sf = self.st.borrow().screen_frame;
        me.load_viewer().island_size(if sf.is_empty() { Size::new(1728.0, 1117.0) } else { sf.size() })
    }

    /// The file changed under an open viewer: rebuild it from the file, keeping the scroll position.
    pub fn reload_viewer(self: &Rc<Self>) {
        let Some(v) = self.file_viewer() else {
            debug(&format!("reload {}: no viewer", self.id));
            return;
        };
        if !self.is_viewer() || !v.is_stale() {
            debug(&format!("reload {}: not stale", self.id));
            return;
        }
        debug(&format!("reload {}: stale, offset {:?}", self.id, v.scroll_offset()));
        // A reload still waiting to restore its place (an async load) keeps the place it was given.
        let offset = v.restore_offset().or_else(|| v.scroll_offset());
        self.reset_viewer();
        if !self.expanded() || !self.st.borrow().placed {
            return;
        }
        self.load_viewer().set_restore_offset(offset);
        self.viewer_opened(true);
        let r = self.rect_for(self.size());
        self.morph(r, Spring::GROW, None, None);
        // Mid hide or show the new picture goes with the shape; the live view follows the growth (`live_viewer_after`).
        if !(self.leaving() || self.shape.has_anim(crate::model::toggle::TOGGLE_SCALE)) {
            self.show_scroll();
        }
        debug(&format!("reloaded {}: {:?} hidden {:?}", self.id, self.file_viewer().and_then(|v| v.text_prefix(12)), self.file_viewer().and_then(|v| v.root()).map(|r| r.isHidden())));
    }

    fn viewer_opened(self: &Rc<Self>, open: bool) {
        if open {
            // Opened again after the file changed: what it shows is read now.
            if self.file_viewer().is_some_and(|v| v.is_stale()) {
                self.reset_viewer();
            }
            if let Some(p) = self.island().path() {
                let path = std::path::PathBuf::from(crate::model::island::viewer_path(&p));
                let same = self.st.borrow().file_watch.as_ref().is_some_and(|w| w.path == path);
                if !same {
                    if let Some(w) = self.st.borrow_mut().file_watch.take() {
                        w.cancel();
                    }
                    let me = self.me();
                    let w = super::filewatch::FileWatch::new(
                        &path,
                        Box::new(move || {
                            if let Some(c) = me.upgrade() {
                                c.reload_viewer();
                            }
                        }),
                    );
                    self.st.borrow_mut().file_watch = Some(w);
                }
            }
            let sz = self.viewer_size();
            let body = FileViewer::body_rect(sz);
            if let Some(subs) = unsafe { self.body_host.sublayers() } {
                for l in subs.iter() {
                    l.removeFromSuperlayer();
                }
            }
            let l = layer();
            l.setAnchorPoint(pt(0.0, 0.0));
            l.setFrame(cgr(Rect::new(0.0, 0.0, body.w, body.h)));
            let v = self.load_viewer();
            if let Some(img) = v.snapshot(body.size()) {
                // SAFETY: a CGImage as contents.
                unsafe { l.setContents(Some(img.object())) };
            }
            // SAFETY: a framework constant.
            l.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityTopLeft });
            l.setContentsScale(self.scale());
            self.body_host.addSublayer(&l);
            self.body_host.setBounds(l.bounds());
            self.body_host.setPosition(pt(PAD, BODY_TOP));
            if self.st.borrow().outside_monitor.is_none() && !offscreen() {
                let me = self.me();
                let block = block2::RcBlock::new(move |_e: std::ptr::NonNull<NSEvent>| {
                    // Hidden, a click anywhere is not a click outside an open viewer: it stays open for the show.
                    // Back on screen for news, it is a viewer on screen like any other.
                    let Some(c) = me.upgrade() else { return };
                    if !app().on_screen(&c.id) {
                        return;
                    }
                    // Heimdall's drawing mode: a press on its overlay is ink over the viewer, not a click outside.
                    if !super::outside_click::closes_viewer() {
                        return;
                    }
                    Ctl::close_viewer(&c);
                });
                let mask = objc2_app_kit::NSEventMask::LeftMouseDown | objc2_app_kit::NSEventMask::RightMouseDown;
                let m = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &block);
                self.st.borrow_mut().outside_monitor = m;
            }
            self.panel.ivars().accepts_key.set(true);
        } else {
            self.hide_scroll();
            if let Some(m) = self.st.borrow_mut().outside_monitor.take() {
                // SAFETY: a monitor we added.
                unsafe { NSEvent::removeMonitor(&m) };
            }
            if let Some(w) = self.st.borrow_mut().file_watch.take() {
                w.cancel();
            }
            self.panel.ivars().accepts_key.set(false);
            if self.panel.isKeyWindow() {
                self.panel.resignKeyWindow();
            }
        }
    }

    /// The live viewer (scrolling, selection, links, players), shown once the shape has opened.
    fn show_scroll(self: &Rc<Self>) {
        let v = self.load_viewer();
        let body = FileViewer::body_rect(self.shape_rect().size());
        let r = v.make_view(body.size());
        let in_container = unsafe { r.superview() }.map(|s| Retained::as_ptr(&s) as *const ()) == Some(Retained::as_ptr(&self.container) as *const ());
        if !in_container {
            self.container.addSubview(&r);
        }
        r.setFrame(ns_rect(body));
        v.layout(body.size());
        self.body_host.setOpacity(0.0);
        if !(r.isHidden() || r.alphaValue() < 1.0) {
            return;
        }
        r.setHidden(false);
        // Back from a hide or show: its picture was just the same.
        if std::mem::take(&mut self.st.borrow_mut().frozen) {
            r.setAlphaValue(1.0);
            return;
        }
        r.setAlphaValue(0.0);
        let r2 = r.clone();
        let block = block2::RcBlock::new(move |ctx: std::ptr::NonNull<objc2_app_kit::NSAnimationContext>| {
            // SAFETY: the context AppKit passes for this group.
            unsafe { ctx.as_ref().setDuration(0.2) };
            let a: Retained<NSView> = unsafe { msg_send![&*r2, animator] };
            a.setAlphaValue(1.0);
        });
        objc2_app_kit::NSAnimationContext::runAnimationGroup(&block);
    }

    fn hide_scroll(&self) {
        self.st.borrow_mut().frozen = false;
        if let Some(v) = self.file_viewer() {
            v.pause();
            if let Some(r) = v.root() {
                r.setHidden(true);
                r.setAlphaValue(0.0);
            }
        }
        self.body_host.setOpacity(1.0);
    }

    /// The file or its actions changed, or the island left: drop the viewer (players stop).
    pub fn reset_viewer(&self) {
        self.st.borrow_mut().frozen = false;
        let v = self.st.borrow_mut().file_viewer.take();
        if let Some(v) = v {
            v.teardown();
        }
        // Open, viewer_opened arms the watch again for the current path; closed or leaving, nothing watches.
        let keep = { let s = self.st.borrow(); !s.removing && s.expanded && s.island.look() == "viewer" };
        if !keep {
            if let Some(w) = self.st.borrow_mut().file_watch.take() {
                w.cancel();
            }
        }
    }

    pub fn close_viewer(self: &Rc<Self>) {
        if !self.is_viewer() || !self.expanded() {
            return;
        }
        if let Some(o) = IslandFile::patch(&self.id, |o| {
            o.insert("expanded".into(), Value::Bool(false));
        }) {
            self.update(Island::new(&self.id, o));
        }
    }

    pub fn menu_toggle(self: &Rc<Self>) {
        let keep = !self.expanded();
        if let Some(o) = IslandFile::patch(&self.id, |o| {
            o.insert("expanded".into(), Value::Bool(keep));
        }) {
            self.update(Island::new(&self.id, o));
        }
    }

    pub fn menu_open_target(self: &Rc<Self>) {
        if let Some(t) = self.island().open() {
            self.open_target(&t);
        }
    }

    /// Close for good: the island goes and the agent hears about it. A plan list (Goals, Scheduled, Settled, a
    /// left-over To do) is hidden instead, its file kept (bug 2026-10-31T15-57-16: the user closed To do by accident and the
    /// whole list was deleted); it comes back when `hidden` is cleared (the menu's Show, `islands show`).
    pub fn close_island(self: &Rc<Self>) {
        IslandFile::respond(&self.id, "closed", Some("close"), None);
        close_file(&self.id);
    }

    fn layout_close(&self, animated: bool) {
        let expanded = self.expanded();
        if !expanded && self.st.borrow().close_layer.is_none() {
            return;
        }
        let c = { let existing = self.st.borrow().close_layer.clone(); existing }.unwrap_or_else(|| {
            let c = sym("xmark.circle.fill", 22.0, &[white(0.9), white(0.18)], self.scale());
            c.setOpacity(0.0);
            self.shape.addSublayer(&c);
            self.st.borrow_mut().close_layer = Some(c.clone());
            c
        });
        let target = pt(self.size().w - PAD - 4.0, PILL_H / 2.0);
        self.shape.addSublayer(&c);
        quiet(|| c.setPosition(target));
        if animated {
            fade(&c, if expanded { 1.0 } else { 0.0 }, if expanded { 0.2 } else { 0.08 }, if expanded { 0.15 } else { 0.0 });
        } else {
            c.setOpacity(if expanded { 1.0 } else { 0.0 });
        }
        self.layout_share(animated, target);
    }

    /// An open viewer of a file that is there shows its drag handle left of the close button (`share.rs`); closed,
    /// gone or another kind of island, there is none.
    fn layout_share(&self, animated: bool, close: CGPoint) {
        let path = self.is_viewer().then(|| self.island().path()).flatten().map(|p| std::path::PathBuf::from(crate::model::island::viewer_path(&p)));
        let shown = self.expanded() && path.as_ref().is_some_and(|p| p.exists());
        let old = self.st.borrow().share.clone();
        let Some(path) = path.filter(|_| shown) else {
            if let Some(h) = old {
                h.untrack(&self.view);
                if animated {
                    fade(&h.layer, 0.0, 0.08, 0.0);
                } else {
                    h.layer.setOpacity(0.0);
                }
            }
            return;
        };
        let h = match old {
            Some(h) if h.path() == path.as_path() => h,
            other => {
                if let Some(o) = other {
                    o.teardown(&self.view);
                }
                let h = Rc::new(super::share::Handle::new(&path, self.scale()));
                h.layer.setOpacity(0.0);
                self.st.borrow_mut().share = Some(h.clone());
                h
            }
        };
        self.shape.addSublayer(&h.layer);
        let at = pt(close.x - super::share::ROOM, close.y);
        quiet(|| h.layer.setPosition(at));
        if animated {
            fade(&h.layer, 1.0, 0.2, 0.15);
        } else {
            h.layer.setOpacity(1.0);
        }
        let f = Rect::new(at.x - super::share::SIZE / 2.0, at.y - super::share::SIZE / 2.0, super::share::SIZE, super::share::SIZE);
        if !offscreen() {
            h.track(&self.view, f);
        }
    }

    /// The drag handle while it shows.
    pub fn shown_share(&self) -> Option<Rc<super::share::Handle>> {
        if !(self.is_viewer() && self.expanded()) {
            return None;
        }
        self.st.borrow().share.clone().filter(|h| h.layer.opacity() > 0.0)
    }

    /// The pointer came over the drag handle or left it (its tracking area).
    pub fn share_hover(&self, on: bool) {
        let h = self.st.borrow().share.clone();
        if let Some(h) = h {
            h.set_hover(on && self.shown_share().is_some());
        }
    }
}

/// What closing an island does to its file: a plan list is hidden and minimised, its file and items kept; any
/// other island's file is removed.
pub fn close_file(id: &str) {
    let path = Paths::file(id);
    IslandFile::locked(|| match IslandFile::read(&path).and_then(|o| PlanList::closed(id, &o)) {
        Some(o) => {
            let _ = IslandFile::write_island(id, &o);
        }
        None => {
            let _ = std::fs::remove_file(&path);
        }
    });
}

/// The labels an open island shows by the clock now, and what it reads of the other islands (`world::Ctx`).
fn due_sig(i: &Island, cal: &Cal) -> String {
    let now = crate::model::time::now();
    let list = i.list().unwrap_or_default();
    let ctx = super::world::Ctx::current().sig(i);
    match list.as_str() {
        "settled" => return format!("{}|{ctx}", super::settled::label_sig(&i.raw, now, cal)),
        "scheduled" => return format!("{}|{ctx}", super::scheduled::label_sig(&i.raw, now, cal)),
        "goals" => return format!("{:?}|{ctx}", crate::model::onesys::front(&i.raw, now).map(|(p, n)| (p.id, n))),
        "" if crate::model::onesys::sent(i).is_some() => {
            let quiet = crate::model::onesys::silent_since(i, now).map(|u| crate::model::onesys::minutes(now - u));
            return format!("{quiet:?}|{ctx}");
        }
        _ => {}
    }
    i.plan_items().iter().filter_map(|p| super::parts::when_label(p, &list, now, cal)).map(|(t, s)| format!("{t}{}", if s { "!" } else { "" })).collect::<Vec<_>>().join("|")
}

pub fn is_plan(i: &Island) -> bool {
    i.list().is_some() || i.grouped_items().is_some()
}

/// A Day Timeline: its rows open on a click like a plan's.
pub fn has_timeline(i: &Island) -> bool {
    i.parts().iter().any(|p| j::str(p.get("type")) == Some("timeline"))
}

fn map_parts(o: &mut Json, ty: &str, id: Option<&str>, f: impl Fn(&mut Json)) {
    let Some(parts) = j::objs(o.get("parts")).map(|p| p.into_iter().cloned().collect::<Vec<Json>>()) else {
        o.insert("parts".into(), Value::Array(vec![]));
        return;
    };
    let out: Vec<Value> = parts
        .into_iter()
        .map(|mut p| {
            if j::str(p.get("type")) == Some(ty) && id.is_none_or(|id| j::str(p.get("id")).unwrap_or(ty) == id) {
                f(&mut p);
            }
            Value::Object(p)
        })
        .collect();
    o.insert("parts".into(), Value::Array(out));
}

fn crossfade(old: &CALayer, new: &CALayer, parent: &CALayer) {
    parent.addSublayer(new);
    fade(new, new.opacity(), 0.2, 0.04);
    let a = basic("opacity", Some(any(&*num(shown(old).opacity() as f64))), Some(any(&*num(0.0))), 0.14);
    let o2 = old.retain();
    transaction(false, Some(Box::new(move || o2.removeFromSuperlayer())), || {
        old.setOpacity(0.0);
        add(old, &a, "out");
    });
}

fn arrive(l: &CALayer, look: &str) {
    if look == "failed" {
        let k = keyframes("transform.rotation.z", &[0.0, 0.22, -0.18, 0.12, -0.06, 0.0], None, 0.5);
        add(l, &k, "wiggle");
    } else {
        let b = Spring { response: 0.3, damping: 0.5 }.animation("transform.scale", any(&*num(0.3)), any(&*num(1.0)), 0.0);
        b.setBeginTime(sys::media_time() + 0.08);
        fill_backwards(&b);
        add(l, &b, "bounce");
    }
}

/// Move a layer while it is invisible: out quickly, jump, back in once the shape has room.
fn hop(l: &CALayer, p: CGPoint, target: f32, expanded: bool) {
    let sl = shown(l);
    let from = sl.position();
    let o0 = sl.opacity();
    l.setPosition(p);
    l.setOpacity(target);
    let same = from.x == p.x && from.y == p.y;
    if same && o0 == target {
        return;
    }
    let pos = keyframe_values("position", &[any(&*point_val(from)).retain(), any(&*point_val(p)).retain()], &[0.0, 0.3, 1.0], 1.0);
    // SAFETY: a framework constant.
    pos.setCalculationMode(unsafe { objc2_quartz_core::kCAAnimationDiscrete });
    let times: Vec<f64> = if same { vec![0.0, 1.0] } else if expanded { vec![0.0, 0.14, 0.36, 1.0] } else { vec![0.0, 0.12, 0.24, 0.7] };
    let ovals: Vec<f64> = if same { vec![o0 as f64, target as f64] } else { vec![o0 as f64, 0.0, 0.0, target as f64] };
    let op = keyframes("opacity", &ovals, Some(&times), 1.0);
    let svals: Vec<f64> = if same { vec![1.0, 1.0] } else { vec![1.0, 0.94, 0.94, 1.0] };
    let sc = keyframes("transform.scale", &svals, Some(&times), 1.0);
    let dur = if expanded { 0.3 } else { 0.26 };
    let g = group(&[&pos, &op, &sc], dur);
    g.setTimingFunction(Some(&ease_out()));
    add(l, &g, "hop");
}

/// A soft light behind a row that fades out (about 1.4 s), then goes.
fn highlight(row: &CALayer, strength: f64) {
    let h = layer();
    h.setFrame(cgr(rect_of(row.bounds()).inset(-6.0, 0.0)));
    h.setCornerRadius(8.0);
    continuous(&h);
    set_bg(&h, &white(strength));
    h.setOpacity(0.0);
    row.insertSublayer_atIndex(&h, 0);
    let a = keyframes("opacity", &[0.0, 1.0, 1.0, 0.0], Some(&[0.0, 0.12, 0.4, 1.0]), 1.4);
    a.setTimingFunction(Some(&ease_out()));
    let h2 = h.clone();
    transaction(false, Some(Box::new(move || h2.removeFromSuperlayer())), || {
        timed(&a, &h);
        add(&h, &a, "highlight");
    });
}

/// The notch in screen coordinates: the gap between the screen's two unobscured top areas; on a display without one,
/// a small rect at the top centre.
pub fn notch_rect(screen: Option<&NSScreen>) -> Rect {
    let Some(screen) = screen else { return Rect::ZERO };
    let f = rect_from(screen.frame());
    let (l, r) = (rect_from(screen.auxiliaryTopLeftArea()), rect_from(screen.auxiliaryTopRightArea()));
    if l.w > 0.0 && r.w > 0.0 && r.min_x() > l.max_x() {
        let h = screen.safeAreaInsets().top.max(l.h);
        return Rect::new(f.min_x() + l.max_x(), f.max_y() - h, r.min_x() - l.max_x(), h);
    }
    Rect::new(f.mid_x() - 60.0, f.max_y() - 24.0, 120.0, 24.0)
}

#[allow(dead_code)]
fn _unused(_: &App, _: NSWindowOrderingMode, _: &NSString, _: CGRect, _: CGSize, _: &Presets) {}

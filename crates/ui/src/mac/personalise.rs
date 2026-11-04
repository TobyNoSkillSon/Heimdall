//! Personalisation ▸ Name & Icon ("Heimdall is just the default for me. Some people may want a different
//! name, a different icon … you should be able to choose your own icon in the menu bar"). The row, in the menu's
//! Personalisation section with the voice, carries the current mark as its image and opens one submenu: a grid of marks
//! (the helmet, the bundled SF Symbols, and the custom mark when one is set), the current one on the accent colour;
//! then two field rows, each a grey label and an editable field like Add Model's search: Name, Your Name. The name is
//! also the wake word ("Heimdall, hello" or "Ivy, hello"), so there is no wake-words field.
//!
//! A mark click keeps the menu open, moves the selection in place and sends `IdentityAction::SetIcon`. A field commits
//! on Return, on leaving it (Tab) and when the submenu closes: a value that validates (`Identity::apply` on a copy) is
//! applied in the menu at once and sent; one that does not shows the reason in red under the field and sends nothing.
//!
//! The rows' actions go to this module's own target, so the shared controller stays untouched.

use std::cell::{Cell, RefCell};
use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibilityRadioButtonRole, NSBezierPath, NSColor, NSEvent, NSMenu, NSMenuDelegate, NSMenuItem, NSTextField,
    NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{MainThreadMarker, NSCopying, NSNumber, NSRect, NSSize, NSString};

use super::util::*;
use super::{with_state, State};
use crate::identity::{Icon, Identity, IdentityAction, BUNDLED};
use crate::UiAction;

/// The row's title in the main menu's Personalisation section.
pub const TITLE: &str = "Name & Icon";

// The grid: seven 32-pt cells a row, 4 pt apart, 14 pt in from both edges (Add Model's search field's inset).
const COLUMNS: usize = 7;
const CELL: f64 = 32.0;
const GAP: f64 = 4.0;
const INSET: f64 = 14.0;
const GRID_PAD: f64 = 6.0;
/// The mark inside a cell, in points tall (the menu bar's is 16).
const CELL_MARK: f64 = 18.0;
const CELL_RADIUS: f64 = 8.0;
/// The submenu's width: the grid's.
pub(crate) const WIDTH: f64 = 2.0 * INSET + COLUMNS as f64 * CELL + (COLUMNS - 1) as f64 * GAP;
/// A field row: the search field's 34-pt row, its 24-pt field 5 pt down.
const FIELD_ROW: f64 = 34.0;
const FIELD_HEIGHT: f64 = 24.0;
const FIELD_TOP: f64 = 5.0;
/// From the label column to the field.
const LABEL_GAP: f64 = 10.0;
const ERROR_SIZE: f64 = 11.0;

/// The field rows, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Name,
    UserName,
}

impl Field {
    pub(crate) const ALL: [Field; 2] = [Field::Name, Field::UserName];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Field::Name => "Name",
            Field::UserName => "Your Name",
        }
    }

    fn index(self) -> usize {
        Field::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }
}

/// The marks the grid offers: the helmet, the bundled symbols, then `current` when it is none of those (a custom
/// symbol or image), so the mark in use is always there to see.
pub(crate) fn grid_icons(current: &Icon) -> Vec<Icon> {
    let mut icons = vec![Icon::Helmet];
    icons.extend(BUNDLED.iter().map(|n| Icon::Symbol { name: (*n).to_string() }));
    if !icons.contains(current) {
        icons.push(current.clone());
    }
    icons
}

/// A mark's name for VoiceOver: "Helmet", "Moon Stars", an image's file name.
pub(crate) fn mark_title(icon: &Icon) -> String {
    match icon {
        Icon::Helmet => "Helmet".into(),
        Icon::Symbol { name } => name
            .split('.')
            .filter(|w| !w.is_empty())
            .map(|w| {
                let mut c = w.chars();
                c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
            })
            .collect::<Vec<_>>()
            .join(" "),
        Icon::Image { path } => path.file_stem().map_or_else(|| "Image".into(), |s| s.to_string_lossy().into_owned()),
    }
}

/// What a field shows for `identity`: the name or the user's name.
pub(crate) fn field_value(identity: &Identity, field: Field) -> String {
    match field {
        Field::Name => identity.name.clone(),
        Field::UserName => identity.user_name.clone(),
    }
}

/// What committing `text` in `field` does: `Ok(None)` when nothing changes, `Ok(Some(action))` to apply and send,
/// `Err(reason)` when the result would not validate (nothing is sent).
pub(crate) fn commit(identity: &Identity, field: Field, text: &str) -> Result<Option<IdentityAction>, String> {
    let action = match field {
        Field::Name => IdentityAction::SetName(text.trim().to_string()),
        Field::UserName => IdentityAction::SetUserName(text.trim().to_string()),
    };
    let mut next = identity.clone();
    match next.apply(&action) {
        Ok(true) => Ok(Some(action)),
        Ok(false) => Ok(None),
        Err(reason) => Err(reason),
    }
}

/// The submenu's live parts while the menu is built.
#[derive(Default)]
pub(crate) struct Rows {
    item: Option<Retained<NSMenuItem>>,
    marks: Vec<(Icon, Retained<MarkCell>)>,
    fields: Vec<FieldRow>,
}

pub(crate) struct FieldRow {
    field: Field,
    host: Retained<FlippedView>,
    label: Retained<NSTextField>,
    input: Retained<NSTextField>,
    error: Retained<NSTextField>,
}

// ---- the mark cells ----

pub struct MarkCellIvars {
    index: Cell<usize>,
    icon: RefCell<Icon>,
    selected: Cell<bool>,
    hover: Cell<bool>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
}

define_class!(
    /// One mark in the grid: the mark in the label colour; on hover a quiet rounded fill; the current one on the accent
    /// colour with a white mark (the tiles' On). A press selects it; the menu keeps tracking.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallMarkCell"]
    #[ivars = MarkCellIvars]
    pub struct MarkCell;

    impl MarkCell {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        /// Acts on press: an open menu can swallow the first mouse-up after it opens.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            mark_clicked(self.ivars().index.get());
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {}

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().hover.set(true);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hover.set(false);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            let slot = &self.ivars().tracking;
            if let Some(old) = slot.borrow_mut().take() {
                self.removeTrackingArea(&old);
            }
            let options = NSTrackingAreaOptions::MouseEnteredAndExited | NSTrackingAreaOptions::ActiveAlways | NSTrackingAreaOptions::InVisibleRect;
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), self.bounds(), options, Some(AsRef::<AnyObject>::as_ref(self)), None)
            };
            self.addTrackingArea(&area);
            *slot.borrow_mut() = Some(area);
            unsafe { msg_send![super(self), updateTrackingAreas] }
        }

        #[unsafe(method(isAccessibilityElement))]
        fn ax_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn ax_role(&self) -> Retained<NSString> {
            unsafe { NSAccessibilityRadioButtonRole }.copy()
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn ax_label(&self) -> Retained<NSString> {
            ns(&mark_title(&self.ivars().icon.borrow()))
        }

        #[unsafe(method_id(accessibilityValue))]
        fn ax_value(&self) -> Retained<AnyObject> {
            Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::numberWithBool(self.ivars().selected.get()))))
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn ax_press(&self) -> Bool {
            mark_clicked(self.ivars().index.get());
            Bool::YES
        }
    }
);

impl MarkCell {
    fn new(index: usize, icon: Icon, selected: bool, frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MarkCellIvars {
            index: Cell::new(index),
            icon: RefCell::new(icon),
            selected: Cell::new(selected),
            hover: Cell::new(false),
            tracking: RefCell::new(None),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub(crate) fn is_selected(&self) -> bool {
        self.ivars().selected.get()
    }

    fn set_selected(&self, on: bool) {
        if self.ivars().selected.replace(on) != on {
            self.setNeedsDisplay(true);
        }
    }

    fn draw(&self) {
        let b = self.bounds();
        let (selected, hover) = (self.ivars().selected.get(), self.ivars().hover.get());
        let fill = if selected {
            Some(NSColor::controlAccentColor())
        } else if hover {
            Some(NSColor::quaternaryLabelColor())
        } else {
            None
        };
        if let Some(fill) = fill {
            fill.setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(b, CELL_RADIUS, CELL_RADIUS).fill();
        }
        let ink = if selected { NSColor::whiteColor() } else { label() };
        let r = rect(b.origin.x, ((b.size.height - CELL_MARK) / 2.0).round(), b.size.width, CELL_MARK);
        super::helmet::draw_icon(&self.ivars().icon.borrow(), r, false, &ink);
    }
}

define_class!(
    /// The grid's host: a press between cells does nothing (the menu stays open).
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallMarkGrid"]
    pub struct MarkGrid;

    impl MarkGrid {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {}

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {}
    }
);

impl MarkGrid {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), initWithFrame: frame] }
    }
}

// ---- the target ----

define_class!(
    /// Target of the Personalise fields, and the submenu's delegate (a field still being edited commits on close).
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HeimdallPersonaliseTarget"]
    pub(crate) struct Target;

    unsafe impl NSObjectProtocol for Target {}

    unsafe impl NSMenuDelegate for Target {
        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, _menu: &NSMenu) {
            with_state(|s| {
                let texts: Vec<(Field, String)> = s.personalise_rows.fields.iter().map(|r| (r.field, r.input.stringValue().to_string())).collect();
                for (field, text) in texts {
                    commit_field(s, field, &text);
                }
            });
        }
    }

    impl Target {
        #[unsafe(method(personaliseField:))]
        fn personalise_field(&self, sender: &NSTextField) {
            let Some(field) = Field::ALL.get(sender.tag() as usize).copied() else { return };
            let text = sender.stringValue().to_string();
            with_state(move |s| commit_field(s, field, &text));
        }
    }
);

thread_local! {
    /// Menu items and fields hold their target weakly: this one lives as long as the UI.
    static TARGET: std::cell::OnceCell<Retained<Target>> = const { std::cell::OnceCell::new() };
}

fn target(mtm: MainThreadMarker) -> Retained<Target> {
    TARGET.with(|t| {
        t.get_or_init(|| {
            let this = Target::alloc(mtm).set_ivars(());
            unsafe { msg_send![super(this), init] }
        })
        .clone()
    })
}

// ---- building ----

/// The Personalise row's image: the current mark at a menu symbol's size.
fn row_image(icon: &Icon) -> Retained<objc2_app_kit::NSImage> {
    super::helmet::icon_menu_image(icon, 16.0, 15.0)
}

/// Adds the Personalise row (the current mark, the submenu) to `menu` and returns its live parts for `sync`.
pub(crate) fn add_row(menu: &NSMenu, mtm: MainThreadMarker) -> Rows {
    let (sub, mut rows) = submenu(&crate::identity::current(), mtm);
    let item = menu_item(TITLE, None, None, "", None, mtm);
    item.setImage(Some(&row_image(&crate::identity::current().icon)));
    item.setSubmenu(Some(&sub));
    menu.addItem(&item);
    rows.item = Some(item);
    rows
}

/// The submenu: the mark grid, a separator, the two field rows.
pub(crate) fn submenu(identity: &Identity, mtm: MainThreadMarker) -> (Retained<NSMenu>, Rows) {
    let sub = NSMenu::new(mtm);
    sub.setAutoenablesItems(false);
    let target = target(mtm);
    sub.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*target)));
    let mut rows = Rows::default();

    let icons = grid_icons(&identity.icon);
    let lines = icons.len().div_ceil(COLUMNS);
    let height = 2.0 * GRID_PAD + lines as f64 * CELL + (lines.saturating_sub(1)) as f64 * GAP;
    let grid = MarkGrid::new(rect(0.0, 0.0, WIDTH, height), mtm);
    for (i, icon) in icons.into_iter().enumerate() {
        let (col, line) = (i % COLUMNS, i / COLUMNS);
        let frame = rect(INSET + col as f64 * (CELL + GAP), GRID_PAD + line as f64 * (CELL + GAP), CELL, CELL);
        let cell = MarkCell::new(i, icon.clone(), icon == identity.icon, frame, mtm);
        grid.addSubview(&cell);
        rows.marks.push((icon, cell));
    }
    let item = menu_item("", None, None, "", None, mtm);
    item.setView(Some(&grid));
    sub.addItem(&item);
    sub.addItem(&NSMenuItem::separatorItem(mtm));

    let title_font = menu_font();
    let label_width = Field::ALL.iter().map(|f| text_width(f.title(), &title_font)).fold(0.0, f64::max).ceil();
    let field_x = INSET + 2.0 + label_width + LABEL_GAP;
    let mut previous: Option<Retained<NSTextField>> = None;
    for field in Field::ALL {
        let host = FlippedView::new(rect(0.0, 0.0, WIDTH, FIELD_ROW), mtm);
        let label = text_label(field.title(), &title_font, &secondary(), mtm);
        let size = label.intrinsicContentSize();
        label.setFrame(rect(INSET, FIELD_TOP + ((FIELD_HEIGHT - size.height) / 2.0).round(), label_width + 4.0, size.height.ceil()));
        host.addSubview(&label);
        let input = NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(field_x, FIELD_TOP, WIDTH - INSET - field_x, FIELD_HEIGHT));
        input.setBezeled(true);
        input.setBezelStyle(objc2_app_kit::NSTextFieldBezelStyle::RoundedBezel);
        input.setEditable(true);
        input.setSelectable(true);
        input.setControlSize(objc2_app_kit::NSControlSize::Regular);
        input.setFont(Some(&font(13.0)));
        input.setStringValue(&ns(&field_value(identity, field)));
        input.setTag(field.index() as isize);
        unsafe {
            input.setTarget(Some(&target));
            input.setAction(Some(sel!(personaliseField:)));
        }
        if let Some(cell) = input.cell() {
            cell.setSendsActionOnEndEditing(true);
            cell.setScrollable(true);
            cell.setWraps(false);
        }
        if let Some(prev) = &previous {
            // SAFETY: both fields live in this submenu for as long as it does.
            unsafe { prev.setNextKeyView(Some(&input)) };
        }
        host.addSubview(&input);
        let error = text_label("", &font(ERROR_SIZE), &NSColor::systemRedColor(), mtm);
        error.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByWordWrapping);
        error.setMaximumNumberOfLines(0);
        error.setHidden(true);
        host.addSubview(&error);
        let item = menu_item("", None, None, "", None, mtm);
        item.setView(Some(&host));
        sub.addItem(&item);
        previous = Some(input.clone());
        rows.fields.push(FieldRow { field, host, label, input, error });
    }
    // A little air under the last field, as the grid has above its first line.
    if let Some(last) = rows.fields.last() {
        last.host.setFrameSize(NSSize::new(WIDTH, FIELD_ROW + 2.0));
    }
    (sub, rows)
}

impl FieldRow {
    /// The reason under the field in red (the row grows to hold it), or none.
    fn show_error(&self, reason: Option<&str>) {
        let x = self.input.frame().origin.x;
        let width = WIDTH - INSET - x;
        let last = self.field == Field::UserName;
        let rest = FIELD_ROW + if last { 2.0 } else { 0.0 };
        match reason {
            Some(reason) => {
                self.error.setStringValue(&ns(reason));
                self.error.setPreferredMaxLayoutWidth(width);
                let h = self.error.cell().map_or(14.0, |c| c.cellSizeForBounds(rect(0.0, 0.0, width, 1000.0)).height.ceil());
                // The field's own text column: the bezel's 2-pt inset.
                self.error.setFrame(rect(x + 2.0, FIELD_TOP + FIELD_HEIGHT + 3.0, width - 2.0, h));
                self.error.setHidden(false);
                self.host.setFrameSize(NSSize::new(self.host.frame().size.width, FIELD_TOP + FIELD_HEIGHT + 3.0 + h + 6.0));
            }
            None => {
                if !self.error.isHidden() {
                    self.error.setHidden(true);
                    self.host.setFrameSize(NSSize::new(self.host.frame().size.width, rest));
                }
            }
        }
    }

    /// Whether the field is being typed in (its text is the user's until he commits).
    fn editing(&self) -> bool {
        self.input.currentEditor().is_some()
    }
}

// ---- actions ----

/// A mark was chosen: the selection moves in place, the menu bar and the row follow at once, the app is told.
pub(crate) fn mark_clicked(index: usize) {
    with_state(move |s| {
        let Some(icon) = s.personalise_rows.marks.get(index).map(|(i, _)| i.clone()) else { return };
        apply(s, IdentityAction::SetIcon(icon));
    });
}

/// A field's text committed: applied and sent when it validates and changes something; the reason shown when not.
fn commit_field(s: &mut State, field: Field, text: &str) {
    let result = commit(&crate::identity::current(), field, text);
    if let Some(row) = s.personalise_rows.fields.iter().find(|r| r.field == field) {
        row.show_error(result.as_ref().err().map(String::as_str));
    }
    if let Ok(Some(action)) = result {
        apply(s, action);
    }
}

/// Applies `action` in the menu (optimistic, as Speaking's voices do) and sends it; the app answers with the saved
/// identity, which is then already the menu's.
pub(crate) fn apply(s: &mut State, action: IdentityAction) {
    let mut next = crate::identity::current();
    match next.apply(&action) {
        Ok(true) => {}
        Ok(false) | Err(_) => return,
    }
    crate::identity::set(next);
    s.update_status_button();
    s.refresh_menu();
    s.emit(UiAction::Identity(action));
}

/// `sync_tracking`'s part: the selection, the row's image and the fields (not one being typed in) follow the identity.
pub(crate) fn sync(s: &mut State) {
    let identity = crate::identity::current();
    let rows = &s.personalise_rows;
    for (icon, cell) in &rows.marks {
        cell.set_selected(*icon == identity.icon);
    }
    if let Some(item) = &rows.item {
        item.setImage(Some(&row_image(&identity.icon)));
    }
    for row in &rows.fields {
        if !row.editing() && row.error.isHidden() {
            row.input.setStringValue(&ns(&field_value(&identity, row.field)));
        }
    }
}

// ---- checks and renders ----

/// Offscreen checks: the grid's cells with their marks, and the field inputs.
pub(crate) fn cells(rows: &Rows) -> Vec<(Icon, Retained<MarkCell>)> {
    rows.marks.clone()
}
pub(crate) fn input(rows: &Rows, field: Field) -> Option<Retained<NSTextField>> {
    rows.fields.iter().find(|r| r.field == field).map(|r| r.input.clone())
}
pub(crate) fn error_text(rows: &Rows, field: Field) -> Option<String> {
    rows.fields.iter().find(|r| r.field == field).filter(|r| !r.error.isHidden()).map(|r| r.error.stringValue().to_string())
}
pub(crate) fn item(rows: &Rows) -> Option<Retained<NSMenuItem>> {
    rows.item.clone()
}
/// Sends a field's action as Return does.
pub(crate) fn press_return(field: &NSTextField) {
    let t = target(MainThreadMarker::from(field));
    let _: () = unsafe { msg_send![&*t, personaliseField: field] };
}

/// Offscreen renders only: the grey labels in the grey sampled from the real menu (see `offscreen::capture`).
pub(crate) fn render_grey(rows: &Rows, dark: bool) {
    let grey = if dark { 124.0 / 255.0 } else { 128.0 / 255.0 };
    for row in &rows.fields {
        row.label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(grey, grey, grey, 1.0)));
    }
}

/// The submenu as `submenu` builds it for `identity`, with `error` committed into the Name field first, drawn into
/// `path` (`offscreen::stack`).
pub(crate) fn render_submenu(path: &Path, identity: &Identity, error: Option<&str>, dark: bool, mtm: MainThreadMarker) -> bool {
    let (sub, rows) = submenu(identity, mtm);
    if let Some(text) = error {
        if let Some(row) = rows.fields.iter().find(|r| r.field == Field::Name) {
            row.input.setStringValue(&ns(text));
            row.show_error(commit(identity, Field::Name, text).err().as_deref());
        }
    }
    render_grey(&rows, dark);
    super::offscreen::stack(path, &sub, dark, mtm)
}

/// Offscreen renders for `heimdall-ui-demo --personalise <dir>`, dark first: the whole menu for Heimdall and for Ivy
/// with the leaf; the submenu for both, with an invalid name, and with a custom SVG mark; the menu-bar mark on, off
/// and needing attention for the helmet, the leaf and the SVG (1× and 2×); the notch island speaking with the leaf;
/// then the in-process checks. The identity is the default again afterwards. Returns what failed.
pub(crate) fn render(dir: &Path, mtm: MainThreadMarker) -> Vec<String> {
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let mut wrote = |path: &Path, ok: bool| {
        eprintln!("render {} {}", path.display(), if ok { "ok" } else { "FAILED" });
        if !ok {
            failures.push(path.display().to_string());
        }
    };
    let svg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lab/ui-renders/personalise/test-mark.svg");
    let svg = svg.canonicalize().unwrap_or(svg);
    let heimdall = Identity::default();
    let ivy = Identity { name: "Ivy".into(), user_name: "Evelyn".into(), icon: Icon::Symbol { name: "leaf".into() } };
    let star = Identity { icon: Icon::Image { path: svg.clone() }, ..ivy.clone() };
    for (name, identity) in [("helmet", &heimdall), ("leaf", &ivy), ("svg", &star)] {
        crate::identity::set(identity.clone());
        for (state, look) in [("on", super::helmet::BarState::On), ("off", super::helmet::BarState::Off), ("attention", super::helmet::BarState::Attention)] {
            for (appearance, dark) in [("dark", true), ("light", false)] {
                for scale in [1.0, 2.0] {
                    if !dark && scale == 1.0 { continue; }
                    let path = dir.join(format!("menubar-{name}-{state}-{appearance}@{scale}x.png"));
                    wrote(&path, super::helmet::write_menu_bar(&path, look, dark, scale));
                }
            }
        }
    }
    for (name, identity) in [("heimdall", &heimdall), ("ivy-leaf", &ivy)] {
        for (appearance, dark) in [("dark", true), ("light", false)] {
            crate::identity::set(identity.clone());
            let path = dir.join(format!("menu-{name}-{appearance}.png"));
            wrote(&path, super::offscreen::full_menu(&path, dark, mtm, |_| {}));
            let path = dir.join(format!("submenu-{name}-{appearance}.png"));
            wrote(&path, render_submenu(&path, identity, None, dark, mtm));
        }
    }
    crate::identity::set(ivy.clone());
    let path = dir.join("submenu-ivy-invalid-name-dark.png");
    wrote(&path, render_submenu(&path, &ivy, Some("Ivy!"), true, mtm));
    crate::identity::set(star.clone());
    let path = dir.join("submenu-ivy-svg-dark.png");
    wrote(&path, render_submenu(&path, &star, None, true, mtm));
    crate::identity::set(heimdall.clone());
    let path = dir.join("menu-speaking-model-dark.png");
    wrote(&path, super::offscreen::full_menu(&path, true, mtm, |s| {
        s.speech_voice = crate::speech_voice::demo_speech_voice(false, crate::speech_voice::LocalVoice::Phonon, true);
    }));
    // The notch island speaking, its left ear the leaf (the gallery's "speaking_words" card on a contact sheet).
    crate::identity::set(ivy.clone());
    let island = dir.join("island");
    match super::gallery::sheet(&island, "--card", "speaking_words") {
        Ok(path) => wrote(&path, true),
        Err(e) => wrote(&island.join(format!("speaking_words ({e})")), false),
    }
    crate::identity::set(heimdall);
    failures.extend(super::offscreen::personalise_checks(mtm));
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ivy() -> Identity {
        Identity { name: "Ivy".into(), user_name: "Evelyn".into(), ..Default::default() }
    }

    #[test]
    fn the_grid_starts_with_the_helmet_and_adds_a_custom_mark_once() {
        let g = grid_icons(&Icon::Helmet);
        assert_eq!(g.len(), 1 + BUNDLED.len());
        assert_eq!(g[0], Icon::Helmet);
        assert_eq!(grid_icons(&Icon::Symbol { name: "leaf".into() }).len(), g.len(), "a bundled mark is not added twice");
        let custom = Icon::Image { path: "/tmp/mark.svg".into() };
        let g = grid_icons(&custom);
        assert_eq!(g.last(), Some(&custom));
        assert!(g.len() <= 2 * COLUMNS, "two lines at most");
        assert_eq!(mark_title(&Icon::Symbol { name: "moon.stars".into() }), "Moon Stars");
        assert_eq!(mark_title(&custom), "mark");
    }

    /// No Wake Words field; the name is the wake word.
    #[test]
    fn the_fields_are_the_name_and_the_users_name_only() {
        let i = ivy();
        assert_eq!(Field::ALL.map(Field::title), ["Name", "Your Name"]);
        assert_eq!(field_value(&i, Field::Name), "Ivy");
        assert_eq!(field_value(&i, Field::UserName), "Evelyn");
    }

    #[test]
    fn commits_send_only_valid_changes() {
        let d = Identity::default();
        assert_eq!(commit(&d, Field::Name, " Ivy "), Ok(Some(IdentityAction::SetName("Ivy".into()))));
        assert_eq!(commit(&d, Field::Name, "Heimdall"), Ok(None), "no change, nothing sent");
        assert!(commit(&d, Field::Name, "Ivy!").is_err());
        assert!(commit(&d, Field::Name, "").is_err());
        assert_eq!(commit(&d, Field::UserName, "Evelyn"), Ok(Some(IdentityAction::SetUserName("Evelyn".into()))));
    }
}

//! Connect <provider>: the sign-in page opens in the browser and this small panel shows the code to approve there,
//! until the app reports the sign-in done (it closes) or failed (it says why). "Use API Key…" switches the same
//! panel to a paste field, the fallback: the clipboard is read only then, and only a key-shaped value pre-fills it.
//! A provider without a sign-in (Anthropic) opens straight in the paste field, with its API-keys page in the browser;
//! coming back to the panel fills an empty field from the clipboard.
//!
//! The look (22 Oct, the user's review of the installed menu): no window title bar text; the provider's mark beside a
//! bold "Connect …" heads the panel, labelled fields below it, Cancel and Connect at the bottom right. Connect is the
//! default button (Return) and stays disabled until the required fields hold something (`connect_enabled`); Cancel
//! is Escape. While a key is checked a small spinner and "Checking…" sit at the bottom left; a refusal shows in
//! orange under the fields (the panel grows for it, its top edge where it was). The OpenAI-compatible form alone
//! carries two lines on what a base URL and an API key are, with examples.

use std::cell::Cell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{sel, AllocAnyThread as _, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBox, NSBoxType, NSButton, NSColor, NSControlSize, NSFont, NSFontWeightRegular,
    NSFontWeightSemibold, NSImage, NSImageScaling, NSImageView, NSPanel, NSProgressIndicator,
    NSProgressIndicatorStyle, NSSecureTextField, NSTextAlignment, NSTextField, NSTitlePosition, NSView,
    NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSSize};

use super::util::*;

pub const WIDTH: f64 = 420.0;
/// The margin on every side.
const PAD: f64 = 20.0;
/// The provider's mark beside the title.
const MARK: f64 = 32.0;
/// Text fields and buttons: AppKit's large control size, as NSAlert draws its buttons on macOS 26 and later.
const FIELD: f64 = 28.0;
const BUTTON: f64 = 28.0;
const BUTTON_MIN: f64 = 92.0;
/// One status line.
const LINE: f64 = 16.0;
/// The sign-in code's rounded well.
const CHIP: f64 = 64.0;
/// Between a field label's right edge and its field.
const LABEL_GAP: f64 = 10.0;
/// The Open Console arrow beside the key field.
const CONSOLE: f64 = 24.0;

/// Connect is pressable only with what the provider needs: a key for a key provider; the URL for a provider on
/// the user's own URL (`custom` is `Some(key_required)`), and its key too unless it is optional; never while checking.
pub fn connect_enabled(custom: Option<bool>, url: &str, key: &str, busy: bool) -> bool {
    if busy {
        return false;
    }
    let has_key = !key.trim().is_empty();
    match custom {
        Some(key_required) => !url.trim().is_empty() && (has_key || !key_required),
        None => has_key,
    }
}

/// How many status lines a text needs at `width`: up to three, with a tenth spare for word wrapping (the label
/// truncates a fourth; the whole text is in its tooltip).
fn status_lines(text_width: f64, width: f64) -> usize {
    if text_width <= width - 4.0 { 1 } else { ((text_width * 1.1 / (width - 4.0)).ceil() as usize).clamp(2, 3) }
}

/// A key field stays one line however long the key: it scrolls sideways under the caret and never wraps its dots
/// onto further lines (18 Oct: a pasted 106-character Anthropic key wrapped out of the field).
fn single_line(field: &NSTextField) {
    field.setUsesSingleLineMode(true);
    field.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByClipping);
    if let Some(cell) = field.cell() {
        cell.setUsesSingleLineMode(true);
        cell.setWraps(false);
        cell.setScrollable(true);
        cell.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByClipping);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    SignIn,
    Key,
}

pub struct KeyPanel {
    pub panel: Retained<NSPanel>,
    pub provider: String,
    pub mode: Mode,
    /// The browser link of the pending sign-in (Open Browser opens it again).
    pub url: Option<String>,
    pub code: Retained<NSTextField>,
    pub field: Retained<NSSecureTextField>,
    /// [providers] The base URL field of a provider on the user's own URL; `custom` is `Some(key_required)` for one.
    pub url_field: Retained<NSTextField>,
    pub custom: Option<bool>,
    /// Progress (bottom left, beside the spinner, in key mode; under the code in sign-in mode) or a refusal in
    /// orange (under the fields).
    pub status: Retained<NSTextField>,
    pub connect: Retained<NSButton>,
    pub console: Retained<NSButton>,
    pub use_key: Retained<NSButton>,
    pub reopen: Retained<NSButton>,
    pub cancel: Retained<NSButton>,
    /// Refills an empty key field from the clipboard when the panel becomes key again (`State::watch_key_panel`).
    pub observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    title: Retained<NSTextField>,
    hint: Option<Retained<NSTextField>>,
    url_label: Retained<NSTextField>,
    key_label: Retained<NSTextField>,
    chip: Retained<NSBox>,
    spinner: Retained<NSProgressIndicator>,
    /// The status as last set: (lines, problem); no lines when there is none.
    shown: Cell<(usize, bool)>,
    busy: Rc<Cell<bool>>,
    /// Typing in either field re-evaluates Connect (`connect_enabled`).
    typing: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    /// [providers] A browser sign-in with no code to approve and no key to paste instead (ChatGPT): the panel only
    /// says to continue in the browser (`browser_only`).
    browser: bool,
}

impl Drop for KeyPanel {
    fn drop(&mut self) {
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        for observer in self.observer.take().into_iter().chain(self.typing.drain(..)) {
            unsafe { center.removeObserver(AsRef::<AnyObject>::as_ref(&*observer)) };
        }
    }
}

/// The provider's mark: Anthropic's and OpenCode's from their source vectors (`assets/providers/SOURCES.md`) at the
/// header's size, another bundled mark (`menu::provider_image`), else the SF Symbol the menu shows for it.
fn mark_view(provider: &str, mtm: MainThreadMarker) -> Retained<NSImageView> {
    let image = vector_mark(provider)
        .or_else(|| super::menu::provider_image(provider))
        .or_else(|| {
            let name = match provider {
                "openai-compatible" | "anthropic-compatible" => "server.rack",
                "own-voice" => "laptopcomputer",
                "elevenlabs" => "waveform",
                _ => "network",
            };
            symbol_sized(name, 24.0, unsafe { NSFontWeightRegular })
        })
        .unwrap_or_else(|| NSImage::initWithSize(NSImage::alloc(), NSSize::new(MARK, MARK)));
    image.setTemplate(true);
    let view = NSImageView::imageViewWithImage(&image, mtm);
    view.setImageScaling(NSImageScaling::ScaleProportionallyDown);
    view.setContentTintColor(Some(&label()));
    view
}

/// SVG path data with absolute M, L, H, V and Z only (the marks' sources use nothing else) as a bezier path, mapped
/// by `(x, y) -> (x * s + dx, y * s + dy)` in a flipped image.
fn svg_path(data: &str, s: f64, dx: f64, dy: f64) -> Retained<objc2_app_kit::NSBezierPath> {
    let path = objc2_app_kit::NSBezierPath::bezierPath();
    let mut tokens: Vec<String> = Vec::new();
    let mut number = String::new();
    for c in data.chars() {
        if c.is_ascii_alphabetic() || c == ' ' || c == ',' {
            if !number.is_empty() { tokens.push(std::mem::take(&mut number)); }
            if c.is_ascii_alphabetic() { tokens.push(c.to_string()); }
        } else {
            number.push(c);
        }
    }
    if !number.is_empty() { tokens.push(number); }
    let (mut x, mut y) = (0.0, 0.0);
    let mut command = 'M';
    let mut i = 0;
    let num = |t: &str| t.parse::<f64>().unwrap_or(0.0);
    while i < tokens.len() {
        let t = &tokens[i];
        if let Some(c) = t.chars().next().filter(|c| c.is_ascii_alphabetic()) {
            command = c;
            i += 1;
            if c == 'Z' { path.closePath(); }
            continue;
        }
        match command {
            'M' | 'L' => {
                x = num(t);
                y = tokens.get(i + 1).map_or(0.0, |t| num(t));
                i += 2;
                let p = NSPoint::new(x * s + dx, y * s + dy);
                if command == 'M' { path.moveToPoint(p); command = 'L'; } else { path.lineToPoint(p); }
                continue;
            }
            'H' => x = num(t),
            'V' => y = num(t),
            _ => {}
        }
        i += 1;
        path.lineToPoint(NSPoint::new(x * s + dx, y * s + dy));
    }
    path
}

/// The mark drawn from its source vectors into a `MARK`-point square, sized as the menu's 16-pt bitmaps are (Anthropic
/// 14/16 of the width, OpenCode 12.5/16 of the height), so it stays sharp at any scale.
fn vector_mark(provider: &str) -> Option<Retained<NSImage>> {
    // (paths with their opacity, viewBox origin and size, fit by width or height, the fraction of MARK)
    let (paths, origin, size, by_width, fraction): (Vec<(&'static str, f64)>, (f64, f64), (f64, f64), bool, f64) = match provider {
        "anthropic" => (
            vec![
                ("M32.73 0H25.7846L38.4499 32H45.3953L32.73 0Z", 1.0),
                ("M12.6653 0L0 32H7.08167L9.67193 25.28H22.9219L25.5122 32H32.5939L19.9286 0H12.6653ZM11.9626 19.3371L16.2969 8.09143L20.6313 19.3371H11.9626Z", 1.0),
            ],
            (0.0, 0.0), (45.3953, 32.0), true, 14.0 / 16.0,
        ),
        "opencode-go" | "opencode" => (
            vec![("M384 416H128V96H384V416ZM320 160H192V352H320V160Z", 1.0), ("M320 224V352H192V224H320Z", 0.3)],
            (128.0, 96.0), (256.0, 320.0), false, 12.5 / 16.0,
        ),
        _ => return None,
    };
    let draw = block2::RcBlock::new(move |bounds: objc2_foundation::NSRect| -> objc2::runtime::Bool {
        let s = if by_width { bounds.size.width * fraction / size.0 } else { bounds.size.height * fraction / size.1 };
        let dx = (bounds.size.width - size.0 * s) / 2.0 - origin.0 * s;
        let dy = (bounds.size.height - size.1 * s) / 2.0 - origin.1 * s;
        for (data, alpha) in &paths {
            NSColor::colorWithWhite_alpha(0.0, *alpha).setFill();
            let path = svg_path(data, s, dx, dy);
            path.setWindingRule(objc2_app_kit::NSWindingRule::EvenOdd);
            path.fill();
        }
        objc2::runtime::Bool::YES
    });
    Some(NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(MARK, MARK), true, &draw))
}

/// The OpenAI-compatible form's two lines: what the base URL and the key are, the examples in the label colour.
fn hint_text(provider: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    // [voices] Own Voice Engine: the user's one line of help (22:40), the example in the label colour.
    let own: [(&str, bool); 3] = [
        ("Run any OpenAI-compatible speech server on this Mac and add its address here, such as ", false),
        ("http://localhost:8880/v1", true),
        (".", false),
    ];
    let compatible: [(&str, bool); 5] = [
        ("The base URL is where the server\u{2019}s API answers, such as ", false),
        ("https://api.together.xyz/v1", true),
        (" or ", false),
        ("http://localhost:1234/v1", true),
        (".\nThe API key is the secret that service gives you.", false),
    ];
    let parts: &[(&str, bool)] = if provider == crate::speech_voice::OWN_VOICE { &own } else { &compatible };
    let text = objc2_foundation::NSMutableAttributedString::new();
    for &(part, example) in parts {
        let colour = if example { label() } else { secondary() };
        // An example URL never breaks across lines: a word joiner after each of its separators.
        let part = if example { part.chars().flat_map(|c| [Some(c), matches!(c, '/' | ':' | '.').then_some('\u{2060}')]).flatten().collect() } else { part.to_string() };
        text.appendAttributedString(&attributed(&part, &font(12.0), &colour));
    }
    let field = NSTextField::wrappingLabelWithString(&ns(""), mtm);
    field.setAttributedStringValue(&text);
    field.setSelectable(false);
    field
}

fn field_label(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = text_label(text, &font(13.0), &label(), mtm);
    field.setAlignment(NSTextAlignment::Right);
    field
}

impl KeyPanel {
    pub fn new(provider: &str, name: &str, custom: Option<bool>, target: &AnyObject, mtm: MainThreadMarker) -> Self {
        // No close button: Cancel is the one way out, so a pending sign-in always stops polling. The title stays
        // for the Window menu and accessibility; the panel draws its own heading under a transparent title bar.
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect(0.0, 0.0, WIDTH, 200.0),
            NSWindowStyleMask::Titled | NSWindowStyleMask::FullSizeContentView,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe { panel.setReleasedWhenClosed(false) };
        // Stays visible while the browser is in front: the sign-in's outcome lands here.
        panel.setHidesOnDeactivate(false);
        panel.setTitle(&ns(&format!("Connect {name}")));
        panel.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        panel.setTitlebarAppearsTransparent(true);
        panel.setMovableByWindowBackground(true);
        let content = FlippedView::new(rect(0.0, 0.0, WIDTH, 200.0), mtm);
        panel.setContentView(Some(&content));

        let mark = mark_view(provider, mtm);
        content.addSubview(&mark);
        mark.setFrame(rect(PAD, PAD, MARK, MARK));
        let title = text_label(&format!("Connect {name}"), &NSFont::systemFontOfSize_weight(15.0, unsafe { NSFontWeightSemibold }), &label(), mtm);
        title.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
        content.addSubview(&title);

        let chip = NSBox::initWithFrame(NSBox::alloc(mtm), rect(0.0, 0.0, 10.0, 10.0));
        chip.setBoxType(NSBoxType::Custom);
        chip.setTitlePosition(NSTitlePosition::NoTitle);
        chip.setBorderWidth(0.0);
        chip.setCornerRadius(10.0);
        chip.setFillColor(&NSColor::quaternarySystemFillColor());
        content.addSubview(&chip);
        let code = text_label("…", &NSFont::monospacedSystemFontOfSize_weight(26.0, unsafe { NSFontWeightSemibold }), &label(), mtm);
        code.setAlignment(NSTextAlignment::Center);
        content.addSubview(&code);

        let hint = (custom == Some(true) || provider == crate::speech_voice::OWN_VOICE).then(|| hint_text(provider, mtm));
        if let Some(hint) = &hint { content.addSubview(hint); }

        let url_label = field_label("Base URL", mtm);
        let key_label = field_label("API Key", mtm);
        content.addSubview(&url_label);
        content.addSubview(&key_label);
        let url_field = NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(0.0, 0.0, 100.0, FIELD));
        url_field.setControlSize(NSControlSize::Large);
        url_field.setFont(Some(&font(13.0)));
        // [voices] A speech server's usual port (Kokoro-FastAPI's), as the help line's example says.
        let placeholder = if provider == crate::speech_voice::OWN_VOICE { "http://localhost:8880/v1" } else if provider == "anthropic-compatible" { "http://localhost:8080" } else if custom == Some(false) { "http://localhost:1234/v1" } else { "https://api.together.xyz/v1" };
        url_field.setPlaceholderString(Some(&ns(placeholder)));
        single_line(&url_field);
        content.addSubview(&url_field);
        let field = NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm), rect(0.0, 0.0, 100.0, FIELD));
        field.setControlSize(NSControlSize::Large);
        field.setFont(Some(&font(13.0)));
        if custom == Some(false) { field.setPlaceholderString(Some(&ns("Optional"))); }
        single_line(&field);
        content.addSubview(&field);

        let status = text_label("", &font(12.0), &secondary(), mtm);
        status.setMaximumNumberOfLines(3);
        status.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
        if let Some(cell) = status.cell() { cell.setWraps(true); cell.setTruncatesLastVisibleLine(true); }
        content.addSubview(&status);
        let spinner = NSProgressIndicator::initWithFrame(NSProgressIndicator::alloc(mtm), rect(0.0, 0.0, 16.0, 16.0));
        spinner.setStyle(NSProgressIndicatorStyle::Spinning);
        spinner.setControlSize(NSControlSize::Small);
        spinner.setDisplayedWhenStopped(false);
        content.addSubview(&spinner);

        // Open Console: the provider's API-keys page again, an arrow beside the key field.
        let console = push_button("", target, sel!(providerKeyConsole:), mtm);
        console.setBordered(false);
        if let Some(arrow) = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns("arrow.up.forward.square"), Some(&ns("Open Console"))) {
            let config = objc2_app_kit::NSImageSymbolConfiguration::configurationWithPointSize_weight(15.0, unsafe { NSFontWeightRegular });
            console.setImage(arrow.imageWithSymbolConfiguration(&config).as_deref());
        }
        console.setImagePosition(objc2_app_kit::NSCellImagePosition::ImageOnly);
        console.setContentTintColor(Some(&secondary()));
        console.setToolTip(Some(&ns(&format!("Open {name}\u{2019}s API keys page"))));
        console.setFrame(rect(0.0, 0.0, CONSOLE, CONSOLE));
        let use_key = push_button("Use API Key…", target, sel!(providerUseKey:), mtm);
        let reopen = push_button("Open Browser", target, sel!(providerLoginOpen:), mtm);
        let cancel = push_button("Cancel", target, sel!(providerKeyCancel:), mtm);
        let connect = push_button("Connect", target, sel!(providerKeyConnect:), mtm);
        connect.setKeyEquivalent(&ns("\r"));
        cancel.setKeyEquivalent(&ns("\u{1b}"));
        // Measure every button first: sizing one after placing another would change its height. Cancel and
        // Connect share one width, so the pair reads as one row.
        for b in [&use_key, &reopen, &cancel, &connect] {
            b.setControlSize(NSControlSize::Large);
            b.sizeToFit();
            let w = b.frame().size.width.max(BUTTON_MIN);
            b.setFrameSize(NSSize::new(w, BUTTON));
        }
        let pair = cancel.frame().size.width.max(connect.frame().size.width);
        for b in [&cancel, &connect] { b.setFrameSize(NSSize::new(pair, BUTTON)); }
        for b in [&console, &use_key, &reopen, &cancel, &connect] {
            content.addSubview(b);
        }

        let busy = Rc::new(Cell::new(false));
        let mut typing = Vec::new();
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        for watched in [&*url_field, &**field] {
            let (url_field, field, connect, busy) = (url_field.clone(), field.clone(), connect.clone(), busy.clone());
            let block = block2::RcBlock::new(move |_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
                connect.setEnabled(connect_enabled(custom, &url_field.stringValue().to_string(), &field.stringValue().to_string(), busy.get()));
            });
            let object: &AnyObject = watched.as_ref();
            typing.push(unsafe { center.addObserverForName_object_queue_usingBlock(Some(objc2_app_kit::NSControlTextDidChangeNotification), Some(object), None, &block) });
        }

        let mut panel = KeyPanel {
            panel, provider: provider.into(), mode: Mode::SignIn, url: None, code, field, url_field, custom, status, connect, console,
            use_key, reopen, cancel, observer: None, title, hint, url_label, key_label, chip, spinner, shown: Cell::new((0, false)), busy, typing,
            browser: false,
        };
        panel.start_sign_in();
        panel.panel.center();
        panel
    }

    /// Key mode: the fields, [Cancel][Connect] on the right. Sign-in mode: the code, [Use API Key…] on the left,
    /// [Open Browser][Cancel] on the right.
    fn show_mode(&mut self, mode: Mode) {
        self.mode = mode;
        let key = mode == Mode::Key;
        let custom = self.custom.is_some();
        let view = |v: &NSView, hidden: bool| v.setHidden(hidden);
        view(&self.field, !key);
        view(&self.key_label, !key);
        view(&self.connect, !key);
        // A custom provider has no console page.
        view(&self.console, !key || custom);
        view(&self.url_field, !key || !custom);
        view(&self.url_label, !key || !custom);
        if let Some(hint) = &self.hint { view(hint, !key); }
        view(&self.code, key || self.browser);
        view(&self.chip, key || self.browser);
        view(&self.use_key, key || self.browser);
        view(&self.reopen, key);
        if key { self.panel.setInitialFirstResponder(Some(if custom { &*self.url_field } else { &*self.field })); }
        self.layout();
    }

    /// Places everything top-down for the mode and the status, and sizes the panel to it, its top edge where it was.
    fn layout(&self) {
        let key = self.mode == Mode::Key;
        let (lines, problem) = self.shown.get();
        let inner = WIDTH - 2.0 * PAD;
        let mut y = PAD;
        let title_height = self.title.fittingSize().height;
        self.title.setFrame(rect(PAD + MARK + 12.0, y + ((MARK - title_height) / 2.0).round(), inner - MARK - 12.0, title_height));
        y += MARK;
        if key {
            y += 20.0;
            if let Some(hint) = &self.hint {
                hint.setPreferredMaxLayoutWidth(inner);
                let height = hint.cell().map_or(32.0, |c| c.cellSizeForBounds(rect(0.0, 0.0, inner, 1000.0)).height.ceil());
                hint.setFrame(rect(PAD, y, inner, height));
                y += height + 16.0;
            }
            let custom = self.custom.is_some();
            let key_width = self.key_label.fittingSize().width.ceil();
            let column = if custom { key_width.max(self.url_label.fittingSize().width.ceil()) } else { key_width };
            let x = PAD + column + LABEL_GAP;
            let full = WIDTH - PAD - x;
            let label_at = |l: &NSTextField, y: f64| {
                let h = l.fittingSize().height;
                l.setFrame(rect(PAD, y + ((FIELD - h) / 2.0).round(), column, h));
            };
            if custom {
                label_at(&self.url_label, y);
                self.url_field.setFrame(rect(x, y, full, FIELD));
                y += FIELD + 10.0;
            }
            label_at(&self.key_label, y);
            let console = !self.console.isHidden();
            let width = if console { full - CONSOLE - 6.0 } else { full };
            self.field.setFrame(rect(x, y, width, FIELD));
            self.console.setFrame(rect(x + width + 6.0, y + (FIELD - CONSOLE) / 2.0, CONSOLE, CONSOLE));
            y += FIELD;
            if lines > 0 && problem {
                y += 8.0;
                self.status.setAlignment(NSTextAlignment::Left);
                self.status.setFrame(rect(x + 2.0, y, full - 2.0, LINE * lines as f64));
                y += LINE * lines as f64;
            }
        } else if self.browser {
            // No code: one line saying where the sign-in continues.
            y += 18.0;
            let rows = lines.max(1) as f64;
            self.status.setAlignment(NSTextAlignment::Center);
            self.status.setFrame(rect(PAD, y, inner, LINE * rows));
            y += LINE * rows;
        } else {
            y += 16.0;
            self.chip.setFrame(rect(PAD, y, inner, CHIP));
            let h = self.code.fittingSize().height;
            // An ellipsis sits on the baseline: lift it to the well's optical centre.
            let lift = if self.code.stringValue().to_string() == "…" { 7.0 } else { 0.0 };
            self.code.setFrame(rect(PAD, y + ((CHIP - h) / 2.0).round() - lift, inner, h));
            y += CHIP + 10.0;
            let rows = lines.max(1) as f64;
            self.status.setAlignment(NSTextAlignment::Center);
            self.status.setFrame(rect(PAD, y, inner, LINE * rows));
            y += LINE * rows;
        }
        y += 20.0;
        let right = WIDTH - PAD;
        let w = |b: &NSButton| b.frame().size.width;
        let at = |b: &NSButton, x: f64| b.setFrameOrigin(NSPoint::new(x, y));
        at(&self.use_key, PAD);
        if key {
            at(&self.connect, right - w(&self.connect));
            at(&self.cancel, right - w(&self.connect) - 12.0 - w(&self.cancel));
        } else {
            at(&self.cancel, right - w(&self.cancel));
            at(&self.reopen, right - w(&self.cancel) - 12.0 - w(&self.reopen));
        }
        // Progress while a key is checked: a spinner and one grey line at the bottom left.
        let progress = key && lines > 0 && !problem;
        if progress {
            self.spinner.setFrame(rect(PAD, y + (BUTTON - 16.0) / 2.0, 16.0, 16.0));
            let left = PAD + 16.0 + 6.0;
            self.status.setAlignment(NSTextAlignment::Left);
            self.status.setFrame(rect(left, y + ((BUTTON - LINE) / 2.0).round(), w_left(self, left), LINE));
            unsafe { self.spinner.startAnimation(None) };
        } else {
            unsafe { self.spinner.stopAnimation(None) };
        }
        self.spinner.setHidden(!progress);
        y += BUTTON + PAD;
        self.resize(y);

        fn w_left(p: &KeyPanel, left: f64) -> f64 {
            (p.cancel.frame().origin.x - 12.0 - left).max(40.0)
        }
    }

    fn resize(&self, height: f64) {
        if let Some(view) = self.panel.contentView() {
            if (view.frame().size.height - height).abs() < 0.5 { return; }
        }
        let frame = self.panel.frame();
        let content = self.panel.contentRectForFrameRect(frame);
        let mut next = content;
        next.size.height = height;
        next.origin.y = content.origin.y + content.size.height - height;
        self.panel.setFrame_display(self.panel.frameRectForContentRect(next), true);
    }

    /// Sign-in mode as it opens: no code until the provider answers.
    pub fn start_sign_in(&mut self) {
        self.show_mode(Mode::SignIn);
        self.url = None;
        self.set_reopen_title("Open Browser");
        self.set_code("…");
        self.reopen.setEnabled(false);
        self.set_status(Some(("Opening the browser…", false)));
    }

    /// [providers] A sign-in that finishes in the browser on its own (ChatGPT's consent returns to Heimdall): no code
    /// chip and no Use API Key…, only where it continues, Open Browser and Cancel.
    pub fn browser_only(&mut self) {
        self.browser = true;
        self.show_mode(self.mode);
        if self.mode == Mode::SignIn { self.start_sign_in(); }
    }

    /// The code to approve, shown while the app waits for the approval (none for a browser-only sign-in).
    pub fn show_code(&mut self, code: &str, url: &str) {
        if self.browser || code.is_empty() {
            self.url = Some(url.into());
            self.reopen.setEnabled(true);
            self.set_status(Some(("Continue in your browser", false)));
            return;
        }
        self.set_code(code);
        self.url = Some(url.into());
        self.reopen.setEnabled(true);
        self.set_status(Some(("Approve this code in the browser", false)));
    }

    /// The code in the label colour; the ellipsis that waits for one, quieter.
    fn set_code(&self, code: &str) {
        let relayout = (code == "…") != (self.code.stringValue().to_string() == "…");
        self.code.setStringValue(&ns(code));
        if relayout { self.layout(); }
        let colour = if code == "…" { tertiary() } else { label() };
        self.code.setTextColor(Some(&colour));
    }

    /// A failed sign-in: its link is dead, so the browser button becomes Try Again (a new code).
    pub fn sign_in_failed(&mut self) {
        self.url = None;
        if !self.browser { self.set_code("…"); }
        self.set_reopen_title("Try Again");
        self.reopen.setEnabled(true);
    }

    /// Open Browser or Try Again: the button fits its title, its right edge beside Cancel (`layout`).
    fn set_reopen_title(&self, title: &str) {
        if self.reopen.title().to_string() == title { return; }
        self.reopen.setTitle(&ns(title));
        self.reopen.sizeToFit();
        let w = self.reopen.frame().size.width.max(BUTTON_MIN);
        self.reopen.setFrameSize(NSSize::new(w, BUTTON));
        self.layout();
    }

    /// Key mode as it opens: the clipboard's key if it holds one, nothing otherwise.
    pub fn reset(&mut self, clipboard: Option<&str>) {
        self.show_mode(Mode::Key);
        let prefill = clipboard.filter(|t| crate::model::looks_like_api_key(t)).map(str::trim).unwrap_or("");
        self.field.setStringValue(&ns(prefill));
        self.set_status(None);
        self.set_busy(false);
    }

    /// The clipboard's key into the field, if it holds one (nothing else is ever read into it).
    pub fn fill_from(&mut self, clipboard: Option<&str>) {
        if let Some(key) = clipboard.filter(|t| crate::model::looks_like_api_key(t)).map(str::trim) {
            self.field.setStringValue(&ns(key));
            self.update_connect();
        }
    }

    /// `Some((text, problem))`: progress in grey, a failure in orange.
    pub fn set_status(&self, status: Option<(&str, bool)>) {
        self.status.setHidden(status.is_none());
        let mut shown = (0, false);
        if let Some((text, problem)) = status {
            self.status.setStringValue(&ns(text));
            let colour = if problem { orange() } else { secondary() };
            self.status.setTextColor(Some(&colour));
            self.status.setToolTip(Some(&ns(text)));
            let width = match self.mode {
                Mode::SignIn => WIDTH - 2.0 * PAD,
                // Progress sits on the button row, one line; a refusal under the fields may take two.
                Mode::Key if !problem => f64::INFINITY,
                Mode::Key => self.field.frame().size.width + if self.console.isHidden() { 0.0 } else { CONSOLE + 6.0 } - 2.0,
            };
            let lines = self.status.font().map_or(1, |f| status_lines(text_width(text, &f), width));
            shown = (lines, problem);
        }
        self.shown.set(shown);
        self.layout();
    }

    /// [providers] The URL field's text (the saved URL when changing it).
    pub fn set_url(&self, url: &str) {
        self.url_field.setStringValue(&ns(url));
        self.update_connect();
    }

    pub fn url(&self) -> String {
        self.url_field.stringValue().to_string()
    }

    pub fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.field.setEditable(!busy);
        self.url_field.setEditable(!busy);
        self.update_connect();
    }

    /// Connect follows the fields (`connect_enabled`); typing re-runs this through the fields' change notifications.
    fn update_connect(&self) {
        self.connect.setEnabled(connect_enabled(self.custom, &self.url(), &self.key(), self.busy.get()));
    }

    pub fn key(&self) -> String {
        self.field.stringValue().to_string()
    }
}

/// Every state of the connect panels, dark and light, at real size into `dir` (`heimdall-ui-demo --forms <dir>`),
/// with the focus where the user would type. Nothing reaches the screen, so AppKit draws them as an inactive window: no
/// focus ring, and Connect grey where the live panel shows it blue. Returns what failed: a missing file, or a panel
/// whose Connect is enabled when it should not be (or the reverse).
pub(crate) fn render(dir: &std::path::Path, mtm: MainThreadMarker) -> Vec<String> {
    use objc2_app_kit::{NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameAqua, NSAppearanceNameDarkAqua};
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let long_key = format!("sk-ant-usr01-{}", "Q7mZx2Lr9TfA4kVb8NwC1yHs6PgD3jUe5oRi0".repeat(3))[..106].to_string();
    let refused = "Anthropic: This organization\u{2019}s API access has been disabled. Contact your organization administrator to restore access.";
    type Setup<'a> = Box<dyn Fn(&mut KeyPanel) -> bool + 'a>;
    let states: Vec<(&str, &str, &str, Option<bool>, Setup)> = vec![
        ("openai-compatible-empty", "openai-compatible", "OpenAI-Compatible", Some(true), Box::new(|p| { p.reset(None); p.set_url(""); !p.connect.isEnabled() })),
        ("openai-compatible-url-only", "openai-compatible", "OpenAI-Compatible", Some(true), Box::new(|p| { p.reset(None); p.set_url("https://api.together.xyz/v1"); !p.connect.isEnabled() })),
        ("openai-compatible-filled", "openai-compatible", "OpenAI-Compatible", Some(true), Box::new(|p| { p.reset(Some("sk-demo0123456789abcdefghijklmnopqrstuv")); p.set_url("https://api.together.xyz/v1"); p.connect.isEnabled() })),
        ("openai-compatible-checking", "openai-compatible", "OpenAI-Compatible", Some(true), Box::new(|p| { p.reset(Some("sk-demo0123456789abcdefghijklmnopqrstuv")); p.set_url("https://api.together.xyz/v1"); p.set_status(Some(("Checking…", false))); p.set_busy(true); !p.connect.isEnabled() })),
        ("openai-compatible-rejected", "openai-compatible", "OpenAI-Compatible", Some(true), Box::new(|p| { p.reset(Some("sk-demo0123456789abcdefghijklmnopqrstuv")); p.set_url("https://api.together.xyz/v1"); p.set_status(Some(("The server refused the key (401 Unauthorized)", true))); p.connect.isEnabled() })),
        ("custom-openai-empty", "openai-compatible", "OpenAI-Compatible", Some(false), Box::new(|p| { p.reset(None); p.set_url(""); !p.connect.isEnabled() })),
        ("custom-openai-groq", "openai-compatible", "OpenAI-Compatible", Some(false), Box::new(|p| { p.reset(Some("gsk_demo0123456789abcdefghijklmnopqrstuv")); p.set_url("https://api.groq.com/openai/v1"); p.connect.isEnabled() })),
        ("custom-openai-local", "openai-compatible", "OpenAI-Compatible", Some(false), Box::new(|p| { p.reset(None); p.set_url("http://localhost:1234/v1"); p.connect.isEnabled() })),
        ("custom-anthropic-empty", "anthropic-compatible", "Anthropic-Compatible", Some(false), Box::new(|p| { p.reset(None); p.set_url(""); !p.connect.isEnabled() })),
        ("custom-anthropic-filled", "anthropic-compatible", "Anthropic-Compatible", Some(false), Box::new(|p| { p.reset(Some("sk-demo0123456789abcdefghijklmnopqrstuv")); p.set_url("https://llm.example.net"); p.connect.isEnabled() })),
        ("own-voice-empty", crate::speech_voice::OWN_VOICE, crate::speech_voice::OWN_VOICE_NAME, Some(false), Box::new(|p| { p.reset(None); p.set_url(""); !p.connect.isEnabled() })),
        ("custom-openai-unreachable", "openai-compatible", "OpenAI-Compatible", Some(false), Box::new(|p| { p.reset(None); p.set_url("http://localhost:1234/v1"); p.set_status(Some(("http://localhost:1234/v1 unreachable · check the URL", true))); p.connect.isEnabled() })),
        ("anthropic-key-empty", "anthropic", "Anthropic", None, Box::new(|p| { p.reset(None); !p.connect.isEnabled() })),
        ("anthropic-key-prefilled", "anthropic", "Anthropic", None, Box::new(|p| { p.reset(Some("sk-ant-api03-demo0123456789abcdefghijklmnopqrstuvwxyzABCDEFGH")); p.connect.isEnabled() })),
        ("anthropic-key-long", "anthropic", "Anthropic", None, Box::new(|p| { p.reset(Some(&long_key)); p.connect.isEnabled() })),
        ("anthropic-key-checking", "anthropic", "Anthropic", None, Box::new(|p| { p.reset(Some(&long_key)); p.set_status(Some(("Checking…", false))); p.set_busy(true); !p.connect.isEnabled() })),
        ("anthropic-key-refused", "anthropic", "Anthropic", None, Box::new(|p| { p.reset(Some(&long_key)); p.set_status(Some((refused, true))); p.connect.isEnabled() })),
        ("opencode-sign-in-starting", "opencode-go", "OpenCode Go", None, Box::new(|p| !p.code.isHidden())),
        ("opencode-sign-in-code", "opencode-go", "OpenCode Go", None, Box::new(|p| { p.show_code("DLXZ-HLMT", "https://opencode.ai/console/device?user_code=DLXZ-HLMT"); p.reopen.isEnabled() })),
        ("opencode-sign-in-failed", "opencode-go", "OpenCode Go", None, Box::new(|p| { p.show_code("DLXZ-HLMT", "https://opencode.ai/console/device?user_code=DLXZ-HLMT"); p.set_status(Some(("The sign-in code expired", true))); p.sign_in_failed(); p.reopen.isEnabled() })),
        ("chatgpt-sign-in-starting", "openai", "ChatGPT", None, Box::new(|p| { p.browser_only(); p.code.isHidden() && p.use_key.isHidden() && !p.reopen.isEnabled() })),
        ("chatgpt-sign-in-browser", "openai", "ChatGPT", None, Box::new(|p| { p.browser_only(); p.show_code("", "https://auth.openai.com/api/accounts/authorize?client_id=dynamic_agent_client"); p.reopen.isEnabled() && p.chip.isHidden() })),
        ("chatgpt-sign-in-failed", "openai", "ChatGPT", None, Box::new(|p| { p.browser_only(); p.show_code("", "https://auth.openai.com/"); p.set_status(Some(("ChatGPT sign-in was declined", true))); p.sign_in_failed(); p.reopen.isEnabled() && p.use_key.isHidden() })),
        ("opencode-key-rejected", "opencode-go", "OpenCode Go", None, Box::new(|p| { p.reset(Some("sk-demo0123456789abcdefghijklmnopqrstuv")); p.set_status(Some(("OpenCode Go: access rejected / no subscription", true))); p.connect.isEnabled() })),
        ("elevenlabs-key-typed", "elevenlabs", "ElevenLabs", None, Box::new(|p| { p.reset(None); p.field.setStringValue(&ns("sk_0123456789abcdef0123456789abcdef0123456789abcdef")); p.update_connect(); p.connect.isEnabled() })),
    ];
    for (name, provider, title, custom, setup) in &states {
        for (appearance, dark) in [("dark", true), ("light", false)] {
            let path = dir.join(format!("panel-{name}-{appearance}.png"));
            let controller = super::Controller::new(mtm);
            let mut panel = KeyPanel::new(provider, title, *custom, &controller, mtm);
            let look = if dark { unsafe { NSAppearanceNameDarkAqua } } else { unsafe { NSAppearanceNameAqua } };
            panel.panel.setAppearance(NSAppearance::appearanceNamed(look).as_deref());
            let right = setup(&mut panel);
            if panel.mode == Mode::Key {
                // Focus where the user would type next: the empty URL, else the key, the caret at the end.
                let first: &NSTextField = if panel.custom.is_some() && panel.url().is_empty() { &panel.url_field } else { &panel.field };
                panel.panel.makeFirstResponder(Some(first));
                if let Some(editor) = first.currentEditor() {
                    editor.setSelectedRange(objc2_foundation::NSRange::new(first.stringValue().length(), 0));
                }
            }
            let ok = super::agent_render::capture_window(&panel.panel, &path);
            if !right { failures.push(format!("{name}: Connect or the browser button is in the wrong state")); }
            if !ok { failures.push(path.display().to_string()); }
            panel.panel.close();
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_needs_a_key_for_a_key_provider() {
        assert!(!connect_enabled(None, "", "", false));
        assert!(!connect_enabled(None, "", "   \n", false));
        assert!(connect_enabled(None, "", "sk-ant-api03-abc", false));
        assert!(!connect_enabled(None, "", "sk-ant-api03-abc", true), "never while checking");
    }

    #[test]
    fn connect_needs_the_url_and_a_required_key_for_a_custom_provider() {
        // OpenAI-compatible: both.
        assert!(!connect_enabled(Some(true), "", "", false));
        assert!(!connect_enabled(Some(true), "https://api.together.xyz/v1", "", false));
        assert!(!connect_enabled(Some(true), " ", "sk-abc", false));
        assert!(connect_enabled(Some(true), "https://api.together.xyz/v1", "sk-abc", false));
        // Own engine: the URL alone; the key is optional.
        assert!(!connect_enabled(Some(false), "", "sk-abc", false));
        assert!(connect_enabled(Some(false), "http://localhost:1234/v1", "", false));
        assert!(!connect_enabled(Some(false), "http://localhost:1234/v1", "", true));
    }

    #[test]
    fn a_long_status_takes_a_second_line() {
        assert_eq!(status_lines(100.0, 300.0), 1);
        assert_eq!(status_lines(299.0, 300.0), 2);
        assert_eq!(status_lines(600.0, 300.0), 3);
        assert_eq!(status_lines(2000.0, 300.0), 3);
        assert_eq!(status_lines(10.0, f64::INFINITY), 1);
    }
}

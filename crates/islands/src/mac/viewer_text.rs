//! The viewer's text (Swift `Viewer.swift`, "Text"): markdown rendered block by block (headings, lists, task boxes,
//! quotes, code blocks on a dark block, real tables, rules) with inline markdown through Foundation, and source code
//! monospaced with light colouring in Xcode's dark palette. TextKit 1 attributed strings (text blocks and tables).
//!
//! The pure parts (`blocks`, `code_spans`, `family`) are unit-tested; `render_*` build the attributed strings.
#![allow(unused_unsafe)]

use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, AllocAnyThread, Message};
use objc2_app_kit::{
    NSColor, NSFont, NSFontManager, NSLayoutManager, NSMutableParagraphStyle, NSTextBlock, NSTextBlockLayer, NSTextBlockValueType,
    NSTextContainer, NSTextStorage, NSTextTab, NSTextTable, NSTextTableBlock, NSTextTableLayoutAlgorithm,
};
use objc2_core_foundation::CGSize;
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSMutableAttributedString, NSNumber, NSRange, NSRectEdge, NSRegularExpression,
    NSRegularExpressionOptions, NSString,
};

use super::look::{self, any, ns, Weight};
use crate::model::ViewerKind;

/// Text shown at most (a larger file shows its start and a "file truncated" line): layout stays fast on resize.
pub const TEXT_READ_LIMIT: usize = 200_000;
/// Text up to this size is read at once (the island's size is known before it opens); larger files load off the
/// main thread.
pub const SYNC_TEXT_LIMIT: u64 = 150_000;

// MARK: Markdown blocks (pure)

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Heading { level: usize, text: String },
    Para(String),
    /// A list item: "•", "○"/"✓" (task boxes) or "1." (numbered); `depth` from the line's indent (2 spaces a step).
    Item { bullet: String, depth: usize, text: String },
    Quote(String),
    Rule,
    Code { lang: Option<String>, lines: Vec<String> },
    /// Rows of cells; the first row is the header. The separator row is dropped.
    Table(Vec<Vec<String>>),
}

/// `CharacterSet.whitespaces`: tab and the Unicode space separators.
fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

fn trim_ws(s: &str) -> &str {
    s.trim_matches(is_ws)
}

fn table_cells(t: &str) -> Vec<String> {
    let mut s = t;
    if let Some(r) = s.strip_prefix('|') {
        s = r;
    }
    if let Some(r) = s.strip_suffix('|') {
        s = r;
    }
    s.split('|').map(|c| trim_ws(c).to_owned()).collect()
}

/// Splits markdown into blocks line by line, as the Swift renderer does (no nesting beyond list indent).
pub fn blocks(src: &str) -> Vec<Block> {
    let mut out: Vec<Block> = vec![];
    let mut code: Option<(Option<String>, Vec<String>)> = None;
    let mut table: Vec<Vec<String>> = vec![];
    fn flush_table(table: &mut Vec<Vec<String>>, out: &mut Vec<Block>) {
        if !table.is_empty() {
            out.push(Block::Table(std::mem::take(table)));
        }
    }
    for raw in src.split('\n') {
        let line = raw.replace('\t', "    ");
        let t = trim_ws(&line);
        if t.starts_with("```") || t.starts_with("~~~") {
            if let Some((lang, lines)) = code.take() {
                out.push(Block::Code { lang, lines });
                continue;
            }
            flush_table(&mut table, &mut out);
            let lang = trim_ws(&t[3..]).to_lowercase();
            code = Some((if lang.is_empty() { None } else { Some(lang) }, vec![]));
            continue;
        }
        if let Some((_, lines)) = code.as_mut() {
            lines.push(line.clone());
            continue;
        }
        if t.starts_with('|') {
            // The separator row (|---|:--:|) is dropped.
            if t.replace('|', "").chars().all(|c| c == '-' || c == ':' || c == ' ') {
                continue;
            }
            table.push(table_cells(t));
            continue;
        }
        flush_table(&mut table, &mut out);
        if t.is_empty() {
            continue;
        }
        if t == "---" || t == "***" || t == "___" {
            out.push(Block::Rule);
            continue;
        }
        if t.starts_with('#') {
            let level = t.chars().take_while(|c| *c == '#').count();
            out.push(Block::Heading { level, text: trim_ws(&t[level..]).to_owned() });
            continue;
        }
        if let Some(q) = t.strip_prefix('>') {
            out.push(Block::Quote(trim_ws(q).to_owned()));
            continue;
        }
        let depth = line.chars().take_while(|c| *c == ' ').count() / 2;
        if let Some((bullet, rest)) = list_marker(t) {
            out.push(Block::Item { bullet, depth, text: rest.to_owned() });
            continue;
        }
        out.push(Block::Para(t.to_owned()));
    }
    if let Some((lang, lines)) = code.take() {
        out.push(Block::Code { lang, lines });
    }
    flush_table(&mut table, &mut out);
    out
}

/// `- [ ] `, `- [x] ` (task boxes), `- `/`* `/`+ ` (bullets), `12. `/`3) ` (numbers): the marker and the rest.
fn list_marker(t: &str) -> Option<(String, &str)> {
    let b = t.as_bytes();
    if b.len() >= 6 && matches!(b[0], b'-' | b'*' | b'+') && &b[1..3] == b" [" && matches!(b[3], b' ' | b'x' | b'X') && &b[4..6] == b"] " {
        return Some((if b[3] == b' ' { "○" } else { "✓" }.into(), &t[6..]));
    }
    if b.len() >= 2 && matches!(b[0], b'-' | b'*' | b'+') && b[1] == b' ' {
        return Some(("•".into(), &t[2..]));
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && b.len() >= digits + 2 && matches!(b[digits], b'.' | b')') && b[digits + 1] == b' ' {
        return Some((t[..digits + 1].to_owned(), &t[digits + 2..]));
    }
    None
}

// MARK: Code colouring (pure apart from NSRegularExpression, for the Swift build's exact regex semantics)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hue {
    Keyword,
    Str,
    Num,
    Comment,
    Key,
}

impl Hue {
    /// Xcode's dark palette.
    pub fn color(self) -> Retained<NSColor> {
        look::rgb(
            match self {
                Hue::Keyword => 0xFF7AB2,
                Hue::Str => 0xFC6A5D,
                Hue::Num => 0xD0BF69,
                Hue::Comment => 0x7F8C98,
                Hue::Key => 0x67B7A4,
            },
            1.0,
        )
    }
}

/// The keyword family a file extension colours with.
pub fn family(lang: &str) -> String {
    match lang {
        "ts" | "tsx" | "jsx" | "mjs" | "cjs" | "java" | "kt" | "cs" | "php" | "css" | "scss" => "js".into(),
        "zsh" | "bash" | "fish" => "sh".into(),
        "h" | "m" | "mm" | "cpp" | "cc" | "hpp" | "metal" | "glsl" | "zig" => "c".into(),
        "rb" | "lua" => "py".into(),
        "nix" | "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "env" => "conf".into(),
        _ => lang.into(),
    }
}

fn keywords(family: &str) -> Option<&'static str> {
    Some(match family {
        "swift" => "func|let|var|if|else|guard|return|for|in|while|switch|case|default|struct|class|enum|extension|protocol|import|private|public|fileprivate|static|self|init|try|await|async|throws|nil|true|false|final|override|some|any|where|defer|break|continue|lazy|weak",
        "rs" => "fn|let|mut|if|else|match|return|for|in|while|loop|struct|enum|impl|trait|use|mod|pub|crate|self|Self|async|await|move|ref|const|static|true|false|where|type|dyn|unsafe|as|break|continue|Some|None|Ok|Err",
        "py" => "def|class|return|if|elif|else|for|in|while|import|from|as|with|try|except|finally|raise|lambda|yield|None|True|False|and|or|not|is|pass|break|continue|async|await|self",
        "js" => "function|const|let|var|if|else|return|for|of|in|while|class|extends|import|export|from|default|new|this|async|await|try|catch|finally|throw|null|undefined|true|false|typeof|switch|case|break",
        "go" => "func|package|import|var|const|type|struct|interface|if|else|for|range|return|go|defer|chan|map|switch|case|default|nil|true|false|break|continue",
        "sh" => "if|then|else|elif|fi|for|in|do|done|while|case|esac|function|return|export|local|echo|set",
        "c" => "int|char|void|float|double|long|short|unsigned|signed|struct|union|enum|typedef|static|const|return|if|else|for|while|do|switch|case|default|break|continue|sizeof|NULL|include|define",
        "sql" => "SELECT|FROM|WHERE|INSERT|INTO|VALUES|UPDATE|SET|DELETE|CREATE|TABLE|INDEX|JOIN|LEFT|RIGHT|INNER|ON|AND|OR|NOT|NULL|GROUP|BY|ORDER|LIMIT|AS|DISTINCT|COUNT|HAVING|PRIMARY|KEY",
        _ => return None,
    })
}

/// Coloured ranges (UTF-16 location and length) in paint order: a later range wins where they overlap. Empty for
/// plain text and for text over 200 K UTF-16 units.
pub fn code_spans(s: &str, language: Option<&str>) -> Vec<(usize, usize, Hue)> {
    let Some(lang) = language else { return vec![] };
    let n = s.encode_utf16().count();
    if n > 200_000 {
        return vec![];
    }
    let text = ns(s);
    let mut out = vec![];
    let mut paint = |pattern: &str, hue: Hue, opts: NSRegularExpressionOptions, group: usize| {
        let Ok(re) = (unsafe { NSRegularExpression::regularExpressionWithPattern_options_error(&ns(pattern), opts) }) else { return };
        let matches = unsafe { re.matchesInString_options_range(&text, objc2_foundation::NSMatchingOptions(0), NSRange::new(0, n)) };
        for m in matches.iter() {
            let r = unsafe { m.rangeAtIndex(group) };
            if r.location != usize::MAX && r.location != objc2_foundation::NSNotFound as usize {
                out.push((r.location, r.length, hue));
            }
        }
    };
    let none = NSRegularExpressionOptions(0);
    let lines = NSRegularExpressionOptions::AnchorsMatchLines;
    let fam = family(lang);
    paint(r"\b(0x[0-9a-fA-F]+|\d+(\.\d+)?)\b", Hue::Num, none, 0);
    if let Some(words) = keywords(&fam) {
        paint(&format!("\\b({words})\\b"), Hue::Keyword, if fam == "sql" { NSRegularExpressionOptions::CaseInsensitive } else { none }, 0);
    }
    if ["json", "jsonl", "conf", "toml", "yaml", "yml"].contains(&lang) || fam == "conf" {
        paint(r#"^\s*([A-Za-z0-9_.\-"]+)\s*[:=]"#, Hue::Key, lines, 1);
        paint(r#""([^"\\\n]|\\.)*"\s*:"#, Hue::Key, none, 0);
    }
    paint(r#""([^"\\\n]|\\.)*"|'([^'\\\n]|\\.)*'"#, Hue::Str, none, 0);
    if ["json", "jsonl"].contains(&lang) {
        paint(r#""([^"\\\n]|\\.)*"(?=\s*:)"#, Hue::Key, none, 0);
    }
    if ["py", "sh", "conf", "rb"].contains(&fam.as_str()) || ["toml", "yaml", "yml", "nix"].contains(&lang) {
        paint(r"(^|\s)#.*$", Hue::Comment, lines, 0);
    }
    if fam == "sql" || lang == "lua" {
        paint(r"--.*$", Hue::Comment, lines, 0);
    }
    if ["swift", "rs", "js", "go", "c"].contains(&fam.as_str()) {
        paint(r#"(^|[^:"])//.*$"#, Hue::Comment, lines, 0);
        paint(r"/\*[\s\S]*?\*/", Hue::Comment, none, 0);
    }
    out
}

// MARK: Attributed strings

/// Attribute keys by their string values (the framework constants' values, stable since 10.0).
fn key(k: &str) -> Retained<NSString> {
    ns(k)
}

fn dict(pairs: &[(&str, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let ks: Vec<Retained<NSString>> = pairs.iter().map(|(k, _)| key(k)).collect();
    let kr: Vec<&NSString> = ks.iter().map(|k| &**k).collect();
    let vs: Vec<Retained<AnyObject>> = pairs.iter().map(|(_, v)| unsafe { Retained::retain(*v as *const AnyObject as *mut AnyObject) }.unwrap()).collect();
    NSDictionary::from_retained_objects(&kr, &vs)
}

const FONT: &str = "NSFont";
const COLOR: &str = "NSColor";
const BACKGROUND: &str = "NSBackgroundColor";
const PARAGRAPH: &str = "NSParagraphStyle";
const STRIKE: &str = "NSStrikethrough";
const LINK: &str = "NSLink";

fn mutable(s: &str, attrs: Option<&NSDictionary<NSString, AnyObject>>) -> Retained<NSMutableAttributedString> {
    unsafe { NSMutableAttributedString::initWithString_attributes(NSMutableAttributedString::alloc(), &ns(s), attrs) }
}

fn len(a: &NSAttributedString) -> usize {
    a.length()
}

fn add_attr(a: &NSMutableAttributedString, name: &str, value: &AnyObject, range: NSRange) {
    unsafe { a.addAttribute_value_range(&key(name), value, range) };
}

fn add_para(a: &NSMutableAttributedString, p: &NSMutableParagraphStyle) {
    let n = len(a);
    add_attr(a, PARAGRAPH, any(p), NSRange::new(0, n));
}

fn append(a: &NSMutableAttributedString, b: &NSAttributedString) {
    a.appendAttributedString(b);
}

pub fn body_font() -> Retained<NSFont> {
    look::system(14.0, Weight::Regular)
}
pub fn mono_font() -> Retained<NSFont> {
    look::mono(12.5, Weight::Regular)
}

fn para(before: f64, after: f64, indent: f64, line_height: f64) -> Retained<NSMutableParagraphStyle> {
    let p = NSMutableParagraphStyle::new();
    p.setParagraphSpacingBefore(before);
    p.setParagraphSpacing(after);
    p.setLineHeightMultiple(line_height);
    p.setHeadIndent(indent);
    p.setFirstLineHeadIndent(if indent > 0.0 { indent - 14.0 } else { 0.0 });
    p
}

fn gray(a: f64) -> Retained<NSColor> {
    NSColor::colorWithWhite_alpha(1.0, a)
}

pub fn plain(s: &str) -> Retained<NSMutableAttributedString> {
    let p = para(0.0, 0.0, 0.0, 1.15);
    let f = mono_font();
    let c = gray(0.88);
    mutable(s, Some(&dict(&[(FONT, any(&*f)), (COLOR, any(&*c)), (PARAGRAPH, any(&*p))])))
}

/// Source code: monospaced with light colouring (comments, strings, numbers, keywords). Colouring stops at 200 KB;
/// longer files stay plain.
pub fn render_code(s: &str, language: Option<&str>) -> Retained<NSMutableAttributedString> {
    let out = plain(s);
    for (loc, length, hue) in code_spans(s, language) {
        add_attr(&out, COLOR, any(&*hue.color()), NSRange::new(loc, length));
    }
    out
}

const INTENT_EMPHASIZED: usize = 1 << 0;
const INTENT_STRONG: usize = 1 << 1;
const INTENT_CODE: usize = 1 << 2;
const INTENT_STRIKE: usize = 1 << 5;

/// Inline markdown (bold, italic, code, links, strikethrough) through Foundation's parser, restyled run by run.
fn inline(s: &str, font: &NSFont, color: &NSColor) -> Retained<NSMutableAttributedString> {
    let plain_run = || mutable(s, Some(&dict(&[(FONT, any(font)), (COLOR, any(color))])));
    let opts: Retained<AnyObject> = unsafe { msg_send![objc2::class!(NSAttributedStringMarkdownParsingOptions), new] };
    let _: () = unsafe { msg_send![&*opts, setInterpretedSyntax: 2isize] };
    let parsed: Option<Retained<NSAttributedString>> = unsafe {
        msg_send![NSAttributedString::alloc(), initWithMarkdownString: &*ns(s), options: &*opts, baseURL: std::ptr::null::<AnyObject>(), error: std::ptr::null_mut::<*mut AnyObject>()]
    };
    let Some(a) = parsed else { return plain_run() };
    let out = mutable("", None);
    let text = a.string();
    let n = len(&a);
    let mut i = 0usize;
    let fm: Retained<NSFontManager> = unsafe { msg_send![objc2::class!(NSFontManager), sharedFontManager] };
    while i < n {
        let mut r = NSRange::new(0, 0);
        let attrs: Retained<NSDictionary<NSString, AnyObject>> = unsafe { msg_send![&*a, attributesAtIndex: i, effectiveRange: &mut r] };
        let end = (r.location + r.length).max(i + 1).min(n);
        let run_text = unsafe { text.substringWithRange(NSRange::new(i, end - i)) };
        let mut f = font.retain();
        let mut c = color.retain();
        let mut background: Option<Retained<NSColor>> = None;
        let mut strike = false;
        if let Some(v) = attrs.objectForKey(&key("NSInlinePresentationIntent")) {
            let intent: usize = unsafe { msg_send![&*v, unsignedIntegerValue] };
            if intent & INTENT_CODE != 0 {
                f = look::mono(font.pointSize() - 1.5, Weight::Regular);
                c = NSColor::colorWithWhite_alpha(1.0, 0.95);
                background = Some(NSColor::colorWithWhite_alpha(1.0, 0.1));
            }
            if intent & INTENT_STRONG != 0 {
                f = unsafe { msg_send![&*fm, convertFont: &*f, toHaveTrait: objc2_app_kit::NSFontTraitMask::BoldFontMask] };
            }
            if intent & INTENT_EMPHASIZED != 0 {
                f = unsafe { msg_send![&*fm, convertFont: &*f, toHaveTrait: objc2_app_kit::NSFontTraitMask::ItalicFontMask] };
            }
            strike = intent & INTENT_STRIKE != 0;
        }
        let link = attrs.objectForKey(&key(LINK));
        if link.is_some() {
            c = look::blue();
        }
        let mut pairs: Vec<(&str, &AnyObject)> = vec![];
        if let Some(b) = &background {
            pairs.push((BACKGROUND, any(&**b)));
        }
        let one = NSNumber::new_isize(1);
        if strike {
            pairs.push((STRIKE, any(&*one)));
        }
        if let Some(l) = &link {
            pairs.push((LINK, &**l));
        }
        pairs.push((FONT, any(&*f)));
        pairs.push((COLOR, any(&*c)));
        let piece = unsafe { NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &run_text, Some(&dict(&pairs))) };
        append(&out, &piece);
        i = end;
    }
    out
}

fn block() -> Retained<NSTextBlock> {
    NSTextBlock::new()
}

fn width(b: &NSTextBlock, v: f64, layer: NSTextBlockLayer, edge: Option<NSRectEdge>) {
    unsafe {
        match edge {
            Some(e) => b.setWidth_type_forLayer_edge(v, NSTextBlockValueType::AbsoluteValueType, layer, e),
            None => b.setWidth_type_forLayer(v, NSTextBlockValueType::AbsoluteValueType, layer),
        }
    }
}

fn full_width(b: &NSTextBlock) {
    unsafe { b.setContentWidth_type(100.0, NSTextBlockValueType::PercentageValueType) };
}

fn with_blocks(p: &NSMutableParagraphStyle, b: &NSTextBlock) {
    p.setTextBlocks(&NSArray::from_slice(&[b]));
}

/// Markdown rendered: blocks from `blocks`, inline styles through Foundation.
pub fn render_markdown(src: &str) -> Retained<NSMutableAttributedString> {
    let out = mutable("", None);
    let body = gray(0.86);
    let mut first = true;
    let newline = mutable("\n", None);
    let add = |a: &NSAttributedString, first: &mut bool| {
        if !*first {
            append(&out, &newline);
        }
        append(&out, a);
        *first = false;
    };
    let bf = body_font();
    for b in blocks(src) {
        match b {
            Block::Code { lang, lines } => {
                let tb = block();
                unsafe { tb.setBackgroundColor(Some(&gray(0.07))) };
                width(&tb, 10.0, NSTextBlockLayer::Padding, None);
                width(&tb, if first { 0.0 } else { 8.0 }, NSTextBlockLayer::Margin, Some(NSRectEdge::MinY));
                width(&tb, 4.0, NSTextBlockLayer::Margin, Some(NSRectEdge::MaxY));
                full_width(&tb);
                let p = para(0.0, 0.0, 0.0, 1.12);
                with_blocks(&p, &tb);
                let code = render_code(&lines.join("\n"), Some(lang.as_deref().unwrap_or("swift")));
                add_para(&code, &p);
                add(&code, &mut first);
            }
            Block::Table(rows) => {
                let cols = rows.iter().map(|r| r.len()).max().unwrap_or(1);
                let tt = NSTextTable::new();
                tt.setNumberOfColumns(cols);
                tt.setCollapsesBorders(true);
                tt.setHidesEmptyCells(false);
                tt.setLayoutAlgorithm(NSTextTableLayoutAlgorithm::AutomaticLayoutAlgorithm);
                full_width(&tt);
                width(&tt, if first { 0.0 } else { 10.0 }, NSTextBlockLayer::Margin, Some(NSRectEdge::MinY));
                width(&tt, 6.0, NSTextBlockLayer::Margin, Some(NSRectEdge::MaxY));
                let header_font = look::system(13.0, Weight::Semibold);
                let cell_font = look::system(13.0, Weight::Regular);
                let border = gray(0.16);
                let head_fill = gray(0.07);
                let white = gray(1.0);
                for (r, row) in rows.iter().enumerate() {
                    for c in 0..cols {
                        let cell = unsafe {
                            NSTextTableBlock::initWithTable_startingRow_rowSpan_startingColumn_columnSpan(
                                NSTextTableBlock::alloc(),
                                &tt,
                                r as isize,
                                1,
                                c as isize,
                                1,
                            )
                        };
                        width(&cell, 0.75, NSTextBlockLayer::Border, None);
                        unsafe { cell.setBorderColor(Some(&border)) };
                        width(&cell, 7.0, NSTextBlockLayer::Padding, Some(NSRectEdge::MinX));
                        width(&cell, 7.0, NSTextBlockLayer::Padding, Some(NSRectEdge::MaxX));
                        width(&cell, 4.0, NSTextBlockLayer::Padding, Some(NSRectEdge::MinY));
                        width(&cell, 4.0, NSTextBlockLayer::Padding, Some(NSRectEdge::MaxY));
                        if r == 0 {
                            unsafe { cell.setBackgroundColor(Some(&head_fill)) };
                        }
                        let text = row.get(c).map(|s| s.as_str()).unwrap_or("");
                        let a = inline(
                            if text.is_empty() { " " } else { text },
                            if r == 0 { &header_font } else { &cell_font },
                            if r == 0 { &white } else { &body },
                        );
                        let p = para(0.0, 0.0, 0.0, 1.15);
                        with_blocks(&p, &cell);
                        add_para(&a, &p);
                        add(&a, &mut first);
                    }
                }
            }
            Block::Rule => {
                let tb = block();
                width(&tb, 0.75, NSTextBlockLayer::Border, Some(NSRectEdge::MinY));
                width(&tb, 12.0, NSTextBlockLayer::Margin, Some(NSRectEdge::MinY));
                width(&tb, 4.0, NSTextBlockLayer::Margin, Some(NSRectEdge::MaxY));
                unsafe { tb.setBorderColor(Some(&gray(0.2))) };
                full_width(&tb);
                let p = para(0.0, 0.0, 0.0, 1.25);
                with_blocks(&p, &tb);
                let f = look::system(4.0, Weight::Regular);
                let a = mutable(" ", Some(&dict(&[(FONT, any(&*f)), (PARAGRAPH, any(&*p))])));
                add(&a, &mut first);
            }
            Block::Heading { level, text } => {
                let size = match level {
                    1 => 22.0,
                    2 => 17.0,
                    _ => 15.0,
                };
                let a = inline(&text, &look::system(size, if level == 1 { Weight::Bold } else { Weight::Semibold }), &gray(1.0));
                add_para(&a, &para(if first { 0.0 } else if level == 1 { 16.0 } else { 13.0 }, 4.0, 0.0, 1.25));
                add(&a, &mut first);
            }
            Block::Quote(text) => {
                let a = inline(&text, &bf, &gray(0.6));
                let p = para(0.0, 0.0, 0.0, 1.25);
                let tb = block();
                width(&tb, 8.0, NSTextBlockLayer::Margin, Some(NSRectEdge::MinY));
                width(&tb, 2.0, NSTextBlockLayer::Margin, Some(NSRectEdge::MaxY));
                width(&tb, 2.5, NSTextBlockLayer::Border, Some(NSRectEdge::MinX));
                unsafe { tb.setBorderColor(Some(&gray(0.25))) };
                width(&tb, 12.0, NSTextBlockLayer::Padding, Some(NSRectEdge::MinX));
                full_width(&tb);
                with_blocks(&p, &tb);
                add_para(&a, &p);
                add(&a, &mut first);
            }
            Block::Item { bullet, depth, text } => {
                let marker_color = gray(0.5);
                let a = mutable(&format!("{bullet}\t"), Some(&dict(&[(FONT, any(&*bf)), (COLOR, any(&*marker_color))])));
                append(&a, &inline(&text, &bf, &body));
                let p = NSMutableParagraphStyle::new();
                let d = depth as f64;
                p.setParagraphSpacingBefore(3.0);
                p.setLineHeightMultiple(1.25);
                p.setFirstLineHeadIndent(d * 18.0);
                p.setHeadIndent(d * 18.0 + 20.0);
                let tab = unsafe {
                    NSTextTab::initWithTextAlignment_location_options(
                        NSTextTab::alloc(),
                        objc2_app_kit::NSTextAlignment::Left,
                        d * 18.0 + 20.0,
                        &NSDictionary::new(),
                    )
                };
                p.setTabStops(Some(&NSArray::from_retained_slice(&[tab])));
                add_para(&a, &p);
                add(&a, &mut first);
            }
            Block::Para(text) => {
                let a = inline(&text, &bf, &body);
                add_para(&a, &para(if first { 0.0 } else { 8.0 }, 0.0, 0.0, 1.25));
                add(&a, &mut first);
            }
        }
    }
    out
}

/// Text read with a bound: the first 200 KB, rendered as markdown or (coloured) code.
pub fn read_text(path: &Path, kind: &ViewerKind) -> Retained<NSMutableAttributedString> {
    use std::io::Read;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Ok(f) = std::fs::File::open(path) else { return plain(&format!("Cannot read {name}")) };
    let mut data = Vec::with_capacity(64 * 1024);
    if f.take(TEXT_READ_LIMIT as u64 + 1).read_to_end(&mut data).is_err() {
        return plain(&format!("Cannot read {name}"));
    }
    let over = data.len() > TEXT_READ_LIMIT;
    data.truncate(TEXT_READ_LIMIT);
    let mut s = String::from_utf8_lossy(&data).into_owned();
    if over {
        s.push_str("\n\n… file truncated: showing the first 200 KB");
    }
    match kind {
        ViewerKind::Markdown => render_markdown(&s),
        ViewerKind::Text(lang) => render_code(&s, lang.as_deref()),
        _ => plain(&s),
    }
}

/// The laid-out height of text in the viewer's text view (TextKit 1, as drawn: text blocks, tables).
pub fn text_height(t: &NSAttributedString, width: f64) -> f64 {
    unsafe {
        let storage: Retained<NSTextStorage> = msg_send![NSTextStorage::alloc(), initWithAttributedString: t];
        let lm = NSLayoutManager::new();
        let tc = NSTextContainer::initWithSize(NSTextContainer::alloc(), CGSize::new(width, f64::MAX));
        lm.addTextContainer(&tc);
        storage.addLayoutManager(&lm);
        lm.ensureLayoutForTextContainer(&tc);
        lm.usedRectForTextContainer(&tc).size.height + 4.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_blocks_follow_the_swift_line_rules() {
        let src = "# Title\n\nSome **bold** text.\n\n## Part\n\n1. one\n2) two\n   - nested\n- [x] done\n- [ ] open\n\n| A | B |\n|---|:-:|\n| `x` | 2 |\n\n> quoted\n\n```RS \nfn a() {}\n\n```\n---\nend";
        let b = blocks(src);
        assert_eq!(
            b,
            vec![
                Block::Heading { level: 1, text: "Title".into() },
                Block::Para("Some **bold** text.".into()),
                Block::Heading { level: 2, text: "Part".into() },
                Block::Item { bullet: "1.".into(), depth: 0, text: "one".into() },
                Block::Item { bullet: "2)".into(), depth: 0, text: "two".into() },
                Block::Item { bullet: "•".into(), depth: 1, text: "nested".into() },
                Block::Item { bullet: "✓".into(), depth: 0, text: "done".into() },
                Block::Item { bullet: "○".into(), depth: 0, text: "open".into() },
                Block::Table(vec![vec!["A".into(), "B".into()], vec!["`x`".into(), "2".into()]]),
                Block::Quote("quoted".into()),
                Block::Code { lang: Some("rs".into()), lines: vec!["fn a() {}".into(), "".into()] },
                Block::Rule,
                Block::Para("end".into()),
            ]
        );
    }

    #[test]
    fn markdown_edge_cases() {
        // A tab indents like four spaces; an unclosed fence keeps the rest as code; a table ends at a blank line.
        assert_eq!(blocks("\t- deep"), vec![Block::Item { bullet: "•".into(), depth: 2, text: "deep".into() }]);
        assert_eq!(blocks("```\nx\n# not a heading"), vec![Block::Code { lang: None, lines: vec!["x".into(), "# not a heading".into()] }]);
        assert_eq!(blocks("|a|\n\n|b|"), vec![Block::Table(vec![vec!["a".into()]]), Block::Table(vec![vec!["b".into()]])]);
        assert_eq!(blocks("###"), vec![Block::Heading { level: 3, text: "".into() }]);
        assert_eq!(blocks("-x"), vec![Block::Para("-x".into())]);
        assert!(blocks("").is_empty());
    }

    fn spans_text<'a>(s: &'a str, lang: &str) -> Vec<(String, Hue)> {
        let u: Vec<u16> = s.encode_utf16().collect();
        code_spans(s, Some(lang)).into_iter().map(|(l, n, h)| (String::from_utf16_lossy(&u[l..l + n]), h)).collect()
    }

    #[test]
    fn code_colouring_ranges() {
        let s = "let x = 0x1F; // note\nfn main() { \"hi\" }";
        let spans = spans_text(s, "rs");
        assert!(spans.contains(&("0x1F".into(), Hue::Num)));
        assert!(spans.contains(&("let".into(), Hue::Keyword)));
        assert!(spans.contains(&("fn".into(), Hue::Keyword)));
        assert!(spans.contains(&("\"hi\"".into(), Hue::Str)));
        // The comment range takes the character before the slashes (the Swift pattern does).
        assert!(spans.contains(&(" // note".into(), Hue::Comment)));
        // Paint order: numbers, keywords, strings, comments last.
        let order: Vec<Hue> = spans.iter().map(|s| s.1).collect();
        assert_eq!(order.first(), Some(&Hue::Num));
        assert_eq!(order.last(), Some(&Hue::Comment));
        // JSON keys, SQL case-insensitively, hash comments in shell, nothing for plain text.
        assert!(spans_text("{\"a\": 1}", "json").contains(&("\"a\"".into(), Hue::Key)));
        assert!(spans_text("select * from t", "sql").contains(&("select".into(), Hue::Keyword)));
        assert!(spans_text("echo hi # c", "zsh").contains(&(" # c".into(), Hue::Comment)));
        assert!(code_spans("let x", None).is_empty());
        // UTF-16 offsets: after an emoji (two units).
        let e = spans_text("\u{1F600} let", "rs");
        assert_eq!(e, vec![("let".into(), Hue::Keyword)]);
    }

    #[test]
    fn attribute_keys_match_the_framework() {
        unsafe {
            assert_eq!(objc2_app_kit::NSFontAttributeName.to_string(), FONT);
            assert_eq!(objc2_app_kit::NSForegroundColorAttributeName.to_string(), COLOR);
            assert_eq!(objc2_app_kit::NSBackgroundColorAttributeName.to_string(), BACKGROUND);
            assert_eq!(objc2_app_kit::NSParagraphStyleAttributeName.to_string(), PARAGRAPH);
            assert_eq!(objc2_app_kit::NSStrikethroughStyleAttributeName.to_string(), STRIKE);
            assert_eq!(objc2_app_kit::NSLinkAttributeName.to_string(), LINK);
            assert_eq!(objc2_foundation::NSInlinePresentationIntentAttributeName.to_string(), "NSInlinePresentationIntent");
        }
    }

    #[test]
    fn families() {
        assert_eq!(family("tsx"), "js");
        assert_eq!(family("rb"), "py");
        assert_eq!(family("toml"), "conf");
        assert_eq!(family("rs"), "rs");
    }
}

//! The startup probes of chat_templates.py: what a template does with a system message after the first, the
//! structural patch that renders it in place, and the generation prompt.
//!
//! Python finds the construct responsible in Jinja2's AST. minijinja exposes no AST, so the same search runs over the
//! template's tags: a lexer that reads tags as Jinja2 does (strings, comments, raw blocks, `-`/`+` markers, braces
//! inside expressions) and a block stack that tracks `for`/`if`/`elif`/`else` nesting. It finds the tags Python's
//! `_rejections` and `_skips` find:
//! - rejects: a `{{ raise_exception(...) }}` tag anywhere inside an `if`/`elif` branch whose test has an `or`-term
//!   `<m>.role == 'system'` (or `<m>['role']`), within a `for <m> in messages` loop's body;
//! - drops: an `if` directly in such a loop's body, without `elif` or `else`, whose test has an `and`-term
//!   `<m>.role != 'system'`.
//!
//! Exactly one such tag gets the same replacement text Python writes, and the patch is kept only when the same
//! verification renders pass. For Qwen3.8 this replaces `raise_exception('System message must be at the
//! beginning.')` with `'<|im_start|>system\n' + (message.content|trim) + '<|im_end|>\n'`, as Python does.

use super::template::{template_options, Compiled, Original, RenderError, REASONING_EFFORTS};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

const MARKER: &str = "Splash later-system canary 5d0c8e";

fn system() -> Value {
    json!({"role": "system", "content": "Leading instructions"})
}
fn ask() -> Value {
    json!({"role": "user", "content": "First question"})
}
fn answer() -> Value {
    json!({"role": "assistant", "content": "First answer"})
}
fn later() -> Value {
    json!({"role": "system", "content": MARKER})
}
fn next() -> Value {
    json!({"role": "user", "content": "Next question"})
}
fn thought() -> Value {
    json!({"role": "assistant", "content": "First answer", "reasoning_content": "Hm"})
}
fn call() -> Value {
    json!({
        "role": "assistant",
        "content": "",
        "reasoning_content": "Look it up",
        "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": {"key": "alpha"}}}],
    })
}
fn result() -> Value {
    json!({"role": "tool", "tool_call_id": "call_1", "content": "beta"})
}
fn image() -> Value {
    json!({"role": "user", "content": [
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,"}},
        {"type": "text", "text": "Describe it"},
    ]})
}
fn tools() -> Vec<Value> {
    vec![json!({"type": "function", "function": {
        "name": "lookup",
        "description": "Look up a key.",
        "parameters": {"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]},
    }})]
}

/// Pairs of a canary and the same conversation without its later system message.
fn canaries() -> [(Value, Value); 2] {
    [
        (json!([system(), ask(), answer(), later(), next()]), json!([system(), ask(), answer(), next()])),
        (json!([ask(), answer(), later(), next()]), json!([ask(), answer(), next()])),
    ]
}

fn ordinary() -> [Value; 6] {
    [
        json!([ask()]),
        json!([system(), ask()]),
        json!([system(), ask(), thought(), next()]),
        json!([system(), ask(), call(), result()]),
        json!([system(), ask(), call(), result(), thought(), next()]),
        json!([system(), image()]),
    ]
}

/// Conversations the generation prompt must be the same for: a question, a follow-up after a reasoned answer, and
/// a tool result.
fn followed() -> [Value; 3] {
    [json!([ask()]), json!([system(), ask(), thought(), next()]), json!([system(), ask(), call(), result()])]
}

fn verification_options() -> Vec<Map<String, Value>> {
    let tools = tools();
    let mut all = Vec::new();
    for with_tools in [false, true] {
        for effort in std::iter::once(None).chain(REASONING_EFFORTS.iter().map(|e| Some(*e))) {
            for preserve in [None, Some(true), Some(false)] {
                let t = with_tools.then_some(tools.as_slice());
                all.push(template_options(effort, preserve, t, true, None));
            }
        }
    }
    let mut last = Map::new();
    last.insert("add_generation_prompt".into(), false.into());
    all.push(last);
    all
}

/// How the unmodified template treats the canaries' later system message.
pub(crate) fn original(compiled: &Compiled) -> Original {
    let options = Map::from_iter([("add_generation_prompt".to_owned(), Value::Bool(true))]);
    for (canary, _) in canaries() {
        let Ok(rendered) = compiled.render(&canary, &options) else { return Original::Rejects };
        if !rendered.contains(MARKER) {
            return Original::Drops;
        }
        if !in_place(&rendered) {
            return Original::Misplaces;
        }
    }
    Original::Renders
}

/// Python's `str.find` in code points is order-preserving with byte offsets, so byte offsets compare the same.
fn in_place(rendered: &str) -> bool {
    let (Some(marker), Some(before), Some(after)) =
        (rendered.find(MARKER), rendered.find("First answer"), rendered.find("Next question"))
    else {
        return false;
    };
    rendered.matches(MARKER).count() == 1 && before + "First answer".len() <= marker && marker + MARKER.len() <= after
}

type Outcome = Result<String, RenderError>;

fn outcome(compiled: &Compiled, messages: &Value, options: &Map<String, Value>) -> Outcome {
    compiled.render(messages, options)
}

/// Ordinary conversations render byte-identically, and wherever the original renders a canary's conversation the
/// patch renders the canary in place.
pub(crate) fn verified(original: &Compiled, patched: &Compiled) -> bool {
    let ordinary = ordinary();
    let canaries = canaries();
    for options in verification_options() {
        for messages in &ordinary {
            if outcome(original, messages, &options) != outcome(patched, messages, &options) {
                return false;
            }
        }
        for (canary, without) in &canaries {
            if outcome(original, without, &options).is_err() {
                continue;
            }
            match outcome(patched, canary, &options) {
                Ok(rendered) if in_place(&rendered) => {}
                _ => return false,
            }
        }
    }
    true
}

/// What add_generation_prompt appends under these options: kept where every probe conversation renders as a prefix
/// of its prompt and gains the same non-empty text.
pub(crate) fn generation_prompt(compiled: &Compiled, options: &Map<String, Value>) -> Option<String> {
    let mut history_options = options.clone();
    history_options.insert("add_generation_prompt".into(), false.into());
    let mut texts = BTreeSet::new();
    for messages in followed() {
        let prompt = compiled.render(&messages, options).ok()?;
        let history = compiled.render(&messages, &history_options).ok()?;
        texts.insert(prompt.strip_prefix(history.as_str())?.to_owned());
    }
    match texts.len() {
        1 => texts.pop_first().filter(|t| !t.is_empty()),
        _ => None,
    }
}

/// The template's own block around a system message's content: what a leading system message adds to the front of
/// a one-question conversation without tools or thinking instructions.
fn system_block(compiled: &Compiled) -> Option<(String, String)> {
    let options = template_options(Some("none"), None, None, false, None);
    let with_system =
        compiled.render(&json!([{"role": "system", "content": MARKER}, ask()]), &options).ok()?;
    let without = compiled.render(&json!([ask()]), &options).ok()?;
    let block = with_system.strip_suffix(without.as_str())?;
    if block.matches(MARKER).count() != 1 {
        return None;
    }
    let (prefix, suffix) = block.split_once(MARKER)?;
    Some((prefix.to_owned(), suffix.to_owned()))
}

/// A string literal minijinja reads back as `text`.
fn literal(text: &str) -> String {
    let mut out = String::from("'");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The template with its rejecting or dropping construct replaced, or None.
pub(crate) fn patch(compiled: &Compiled, source: &str, original: Original) -> Option<String> {
    let source = source.replace("\r\n", "\n").replace('\r', "\n");
    let tags = lex(&source)?;
    let constructs = find_constructs(&source, &tags, original == Original::Rejects)?;
    let [(start, end, variable)] = constructs.as_slice() else { return None };
    let (prefix, suffix) = system_block(compiled)?;
    let later = format!("{{{{- {} + ({variable}.content|trim) + {} -}}}}", literal(&prefix), literal(&suffix));
    let replacement = if original == Original::Rejects {
        later
    } else {
        let text = &source[*start..*end];
        let head_len = 2 + text[2..].find(|c: char| !(c == '-' || c == '+')).unwrap_or(0);
        let head_len = head_len + text[head_len..].find(|c: char| !c.is_whitespace()).unwrap_or(0);
        let head = &text[..head_len];
        format!("{head}if not loop.first and {variable}.role == 'system' %}}{later}{head}el{}", &text[head_len..])
    };
    Some(format!("{}{replacement}{}", &source[..*start], &source[*end..]))
}

/// The source as minijinja should read it to render as Jinja2 does: string literals in tags rewritten from
/// Python's escapes (`\a`, `\v`, `\/` kept, octal, lone `\uD83D` as U+FFFD, which tokenizable() makes of it) to
/// minijinja's, and `{% generation %}` as a bare `{% with %}` block, the scope transformers' call block gives its body.
pub(crate) fn rewrite_for_minijinja(source: &str) -> String {
    let Some(tags) = lex(source) else { return source.to_owned() };
    let mut out = String::with_capacity(source.len());
    let mut copied = 0;
    for tag in &tags {
        let body_start = tag.start + tag.open_len;
        let body_end = tag.end - tag.close_len;
        out.push_str(&source[copied..body_start]);
        let inner = &source[body_start..body_end];
        match (tag.block, inner.trim()) {
            (true, "generation") => out.push_str(" with "),
            (true, "endgeneration") => out.push_str(" endwith "),
            _ => out.push_str(&rewrite_literals(inner)),
        }
        copied = body_end;
    }
    out.push_str(&source[copied..]);
    out
}

/// Each string literal in a tag's expression, decoded as Python's unicode-escape and written for minijinja.
fn rewrite_literals(inner: &str) -> std::borrow::Cow<'_, str> {
    if !inner.contains('\\') {
        return inner.into();
    }
    let bytes = inner.as_bytes();
    let mut out = String::with_capacity(inner.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\'' && bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let quote = bytes[i];
        let start = i;
        i += 1;
        while i < bytes.len() && bytes[i] != quote {
            i += if bytes[i] == b'\\' { 2 } else { 1 };
        }
        let end = (i + 1).min(bytes.len());
        let body = &inner[start + 1..i.min(bytes.len())];
        if body.contains('\\') {
            if let Some(text) = python_unescape(body) {
                out.push_str(&inner[copied..start]);
                out.push_str(&literal(&text));
                copied = end;
            }
        }
        i = end;
    }
    out.push_str(&inner[copied..]);
    out.into()
}

/// Python's `unicode-escape` decoding of a literal's body; None where Python would refuse it.
fn python_unescape(body: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    let hex = |chars: &mut std::iter::Peekable<std::str::Chars>, n: usize| -> Option<u32> {
        let digits: String = (0..n).map(|_| chars.next()).collect::<Option<String>>()?;
        u32::from_str_radix(&digits, 16).ok().filter(|_| digits.chars().all(|c| c.is_ascii_hexdigit()))
    };
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let e = chars.next()?;
        match e {
            '\n' => {}
            '\\' | '\'' | '"' => out.push(e),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0b'),
            '0'..='7' => {
                let mut value = e.to_digit(8)?;
                for _ in 0..2 {
                    match chars.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            value = value * 8 + d;
                            chars.next();
                        }
                        None => break,
                    }
                }
                out.push(char::from_u32(value)?);
            }
            'x' => out.push(char::from_u32(hex(&mut chars, 2)?)?),
            'u' | 'U' => {
                let value = hex(&mut chars, if e == 'u' { 4 } else { 8 })?;
                out.push(char::from_u32(value).unwrap_or(if value <= 0x10FFFF { '\u{fffd}' } else { return None }));
            }
            // \N{name} needs Unicode's name table; such a literal stays as written.
            'N' => return None,
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------------------------
// Tags, as Jinja2's lexer finds them.

#[derive(Debug)]
pub(crate) struct Tag {
    /// `{%` rather than `{{`.
    block: bool,
    /// From the opening `{` to just past the closing `}`.
    start: usize,
    end: usize,
    /// The delimiter with its `-`/`+` marker, at each end.
    open_len: usize,
    close_len: usize,
    /// The text between the markers.
    inner: String,
}

/// Every expression and statement tag in order, skipping comments and raw blocks; None if a tag is unterminated.
pub(crate) fn lex(source: &str) -> Option<Vec<Tag>> {
    let bytes = source.as_bytes();
    let mut tags = Vec::new();
    let mut i = 0;
    while let Some(offset) = source[i..].find('{') {
        let start = i + offset;
        let Some(&kind) = bytes.get(start + 1) else { break };
        match kind {
            b'#' => {
                let close = source[start + 2..].find("#}")?;
                i = start + 2 + close + 2;
            }
            b'{' | b'%' => {
                let mut open_len = 2;
                if matches!(bytes.get(start + 2), Some(b'-' | b'+')) {
                    open_len = 3;
                }
                let close_text = if kind == b'{' { "}}" } else { "%}" };
                let body_end = scan_to_close(source, start + open_len, close_text)?;
                let mut inner_end = body_end;
                let mut close_len = 2;
                if body_end > start + open_len && matches!(bytes[body_end - 1], b'-' | b'+') {
                    inner_end -= 1;
                    close_len = 3;
                }
                let end = body_end + 2;
                let inner = source[start + open_len..inner_end].to_owned();
                if kind == b'%' && statement(&inner) == "raw" {
                    // Skip to past `{% endraw %}`.
                    let mut j = end;
                    loop {
                        let k = j + source[j..].find("{%")?;
                        let mut o = k + 2;
                        if matches!(bytes.get(o), Some(b'-' | b'+')) {
                            o += 1;
                        }
                        let rest = source[o..].trim_start();
                        if let Some(after) = rest.strip_prefix("endraw") {
                            let after = after.trim_start();
                            let after = after.strip_prefix(['-', '+']).unwrap_or(after);
                            if after.starts_with("%}") {
                                i = source.len() - after.len() + 2;
                                break;
                            }
                        }
                        j = k + 2;
                    }
                    continue;
                }
                tags.push(Tag { block: kind == b'%', start, end, open_len, close_len, inner });
                i = end;
            }
            _ => i = start + 1,
        }
    }
    Some(tags)
}

/// The index of the closing delimiter, past strings and balanced brackets.
fn scan_to_close(source: &str, from: usize, close: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b'}' if depth > 0 => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            let at = if matches!(bytes.get(i), Some(b'-' | b'+')) { i + 1 } else { i };
            if source[at..].starts_with(close) && (at == i || at > from) {
                return Some(at);
            }
        }
        i += 1;
    }
    None
}

fn statement(inner: &str) -> &str {
    let inner = inner.trim_start();
    let end = inner.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(inner.len());
    &inner[..end]
}

/// Statements that open a block closed by `end<name>`. `set` does only without `=`.
fn opens_block(word: &str, inner: &str) -> bool {
    match word {
        "for" | "if" | "macro" | "call" | "filter" | "with" | "autoescape" | "block" | "generation" | "trans" => true,
        "set" => split_top(inner, "=").len() == 1,
        _ => false,
    }
}

enum Frame {
    /// A loop; `messages_var` is the variable of a `for <m> in messages` without a filter test.
    For { messages_var: Option<String>, in_else: bool },
    /// An if; `test` is the current branch's (None in else). `skip` is a candidate skip condition with its loop
    /// variable, which holds only if the if gets no elif or else.
    If { test: Option<String>, start: usize, end: usize, skip: Option<String>, branched: bool },
    Other,
}

/// The (start, end, loop variable) of each distinct construct the probe's original outcome points to.
fn find_constructs(source: &str, tags: &[Tag], rejects: bool) -> Option<Vec<(usize, usize, String)>> {
    let mut stack: Vec<Frame> = Vec::new();
    let mut found: Vec<(usize, usize, String)> = Vec::new();
    for tag in tags {
        if !tag.block {
            if rejects && is_raise_call(&tag.inner) {
                for (li, frame) in stack.iter().enumerate() {
                    let Frame::For { messages_var: Some(var), in_else: false } = frame else { continue };
                    let inside_branch = stack[li + 1..].iter().any(|f| match f {
                        Frame::If { test: Some(test), .. } => or_terms(test).iter().any(|t| compares_role(t, var, "==")),
                        _ => false,
                    });
                    if inside_branch && !found.iter().any(|(s, _, _)| *s == tag.start) {
                        found.push((tag.start, tag.end, var.clone()));
                    }
                }
            }
            continue;
        }
        let word = statement(&tag.inner);
        let rest = tag.inner.trim_start()[word.len()..].trim();
        match word {
            "for" => stack.push(Frame::For { messages_var: messages_loop(rest), in_else: false }),
            "if" => {
                let skip = match stack.last() {
                    Some(Frame::For { messages_var: Some(var), in_else: false })
                        if and_terms(rest).iter().any(|t| compares_role(t, var, "!=")) =>
                    {
                        Some(var.clone())
                    }
                    _ => None,
                };
                stack.push(Frame::If { test: Some(rest.to_owned()), start: tag.start, end: tag.end, skip, branched: false });
            }
            "elif" => {
                if let Some(Frame::If { test, branched, .. }) = stack.last_mut() {
                    *test = Some(rest.to_owned());
                    *branched = true;
                }
            }
            "else" => match stack.last_mut() {
                Some(Frame::If { test, branched, .. }) => {
                    *test = None;
                    *branched = true;
                }
                Some(Frame::For { in_else, .. }) => *in_else = true,
                _ => {}
            },
            w if w.starts_with("end") => {
                if let Some(Frame::If { start, end, skip: Some(var), branched: false, .. }) = stack.pop() {
                    if !rejects {
                        found.push((start, end, var));
                    }
                }
            }
            w if opens_block(w, rest) => stack.push(Frame::Other),
            _ => {}
        }
    }
    let _ = source;
    Some(found)
}

/// `<var> in messages` without `if` filter, as `for <var> in messages`; None otherwise.
fn messages_loop(rest: &str) -> Option<String> {
    let parts = split_top(rest, "in");
    let [target, iter] = parts.as_slice() else { return None };
    let iter = iter.trim();
    let iter = iter.strip_suffix("recursive").map(str::trim).unwrap_or(iter);
    let target = target.trim();
    (is_identifier(target) && iter == "messages").then(|| target.to_owned())
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_') && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Whether an expression tag's whole expression is a call of `raise_exception`.
fn is_raise_call(inner: &str) -> bool {
    let text = inner.trim();
    let Some(args) = text.strip_prefix("raise_exception") else { return false };
    let args = args.trim_start();
    if !args.starts_with('(') || !args.ends_with(')') {
        return false;
    }
    // The opening parenthesis must close at the very end.
    let bytes = args.as_bytes();
    let mut depth = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 && i + 1 != bytes.len() {
                    return false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    depth == 0
}

/// `expr` split at each top-level keyword `word` (outside strings and brackets).
fn split_top<'a>(expr: &'a str, word: &str) -> Vec<&'a str> {
    let bytes = expr.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let mut i = 0;
    let is_word_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ if depth == 0
                && expr[i..].starts_with(word)
                && (word == "=" || ((i == 0 || !is_word_char(bytes[i - 1]))
                    && bytes.get(i + word.len()).is_none_or(|b| !is_word_char(*b)))) =>
            {
                if word == "=" {
                    // `==`, `!=`, `<=`, `>=` are comparisons, not assignment.
                    let prev = if i > 0 { bytes[i - 1] } else { b' ' };
                    let next = bytes.get(i + 1).copied().unwrap_or(b' ');
                    if next == b'=' || matches!(prev, b'=' | b'!' | b'<' | b'>') {
                        i += if next == b'=' { 2 } else { 1 };
                        continue;
                    }
                }
                parts.push(&expr[start..i]);
                i += word.len();
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&expr[start..]);
    parts
}

/// `expr` without parentheses that wrap all of it, as Jinja2's AST drops them.
fn unwrapped(expr: &str) -> &str {
    let mut expr = expr.trim();
    while expr.starts_with('(') && expr.ends_with(')') && is_raise_call(&format!("raise_exception{expr}")) {
        expr = expr[1..expr.len() - 1].trim();
    }
    expr
}

/// The operands of a chain of `or`, nested chains flattened (chat_templates._terms over Or).
fn or_terms(expr: &str) -> Vec<&str> {
    let expr = unwrapped(expr);
    let parts = split_top(expr, "or");
    if parts.len() == 1 {
        return vec![expr];
    }
    parts.into_iter().flat_map(or_terms).collect()
}

/// The operands of a chain of `and`; an `or` binds looser, so a test with one at its top is a single term.
fn and_terms(expr: &str) -> Vec<&str> {
    let expr = unwrapped(expr);
    if split_top(expr, "or").len() > 1 {
        return vec![expr];
    }
    let parts = split_top(expr, "and");
    if parts.len() == 1 {
        return vec![expr];
    }
    parts.into_iter().flat_map(and_terms).collect()
}

/// Whether `term` is `<var>.role <op> 'system'` or `<var>['role'] <op> 'system'`, parentheses aside.
fn compares_role(term: &str, var: &str, op: &str) -> bool {
    let term = unwrapped(term);
    let Some(rest) = term.strip_prefix(var) else { return false };
    let rest = rest.trim_start();
    let rest = if let Some(r) = rest.strip_prefix('.') {
        let Some(r) = r.trim_start().strip_prefix("role") else { return false };
        if r.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            return false;
        }
        r
    } else if let Some(r) = rest.strip_prefix('[') {
        let r = r.trim_start();
        let Some(r) = r.strip_prefix("'role'").or_else(|| r.strip_prefix("\"role\"")) else { return false };
        let Some(r) = r.trim_start().strip_prefix(']') else { return false };
        r
    } else {
        return false;
    };
    let Some(rest) = rest.trim_start().strip_prefix(op) else { return false };
    matches!(rest.trim(), "'system'" | "\"system\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_comparisons() {
        assert!(compares_role(" message.role == 'system'", "message", "=="));
        assert!(compares_role("(message['role'] == \"system\")", "message", "=="));
        assert!(!compares_role("message.roles == 'system'", "message", "=="));
        assert!(!compares_role("'system' == message.role", "message", "=="));
        assert!(compares_role("m.role != 'system'", "m", "!="));
    }

    #[test]
    fn splits_top_level_keywords() {
        assert_eq!(split_top("a == 'x or y' or (b or c) or d", "or").len(), 3);
        assert_eq!(split_top("ns = namespace(a=1)", "=").len(), 2);
        assert_eq!(split_top("x == 1", "=").len(), 1);
        assert_eq!(split_top("message in messages", "in").len(), 2);
    }

    #[test]
    fn lexes_tags_past_strings_and_comments() {
        let tags = lex("{# {{ no }} #}a{{ '}}' }}{%- raw %}{{ x }}{% endraw %}{% if {'a': {'b': 1}} -%}").unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].inner.trim(), "'}}'");
        assert_eq!(tags[1].inner.trim(), "if {'a': {'b': 1}}");
        assert_eq!(tags[1].close_len, 3);
    }

    #[test]
    fn generation_tags_render_their_body_in_a_scope() {
        assert_eq!(rewrite_for_minijinja("{%- generation %}x{% endgeneration -%}"), "{%- with %}x{% endwith -%}");
    }

    #[test]
    fn parenthesised_tests_split_as_jinja_parses_them() {
        assert_eq!(or_terms("(m.role == 'system' or (m.role == 'developer'))"), ["m.role == 'system'", "m.role == 'developer'"]);
        assert_eq!(and_terms("(m.role != 'system' and m.content)"), ["m.role != 'system'", "m.content"]);
        assert_eq!(and_terms("m.role != 'system' and a or b").len(), 1);
    }

    #[test]
    fn literals_take_python_escapes() {
        assert_eq!(rewrite_for_minijinja(r"{{ 'a\/b\a\v\101' }}x\a"), "{{ 'a\\\\/b\\u0007\\u000bA' }}x\\a");
        assert_eq!(rewrite_for_minijinja(r#"{{ "it's" }}"#), r#"{{ "it's" }}"#);
    }
}

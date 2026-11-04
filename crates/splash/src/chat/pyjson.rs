//! Python's text spellings that reach the prompt or the output: `json.dumps` (transformers' `tojson`, Splash's
//! `json_codec`), `repr`/`str` of floats, lists and dicts (what Jinja2 prints), and `str.strip`'s whitespace.
//! The template and the tool-call projector must write these byte for byte as the Python server did.

use serde_json::{Map, Value};
use std::borrow::Cow;
use std::fmt::Write;

/// Python's `str.isspace()`: the characters `str.strip()` and `str.split()` take as whitespace. Wider than
/// `char::is_whitespace` by U+001C..U+001F.
pub fn is_py_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\x0b' | '\x0c' | '\r' | '\x1c'..='\x1f' | ' ' | '\u{85}' | '\u{a0}' | '\u{1680}'
            | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// Python's truthiness of a JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn py_strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}

/// `repr(float)`: the shortest digits that read back, positional between 1e-4 and 1e16, else `d.ddde+XX`.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf".into() } else { "-inf".into() };
    }
    // Rust's `{:e}` gives the same shortest round-trip digits Python's repr picks.
    let sci = format!("{value:e}");
    let (mantissa, exponent) = sci.split_once('e').expect("exponent");
    let exponent: i32 = exponent.parse().expect("exponent digits");
    let (sign, mantissa) = mantissa.strip_prefix('-').map_or(("", mantissa), |m| ("-", m));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    // Position of the decimal point relative to the digits: value = 0.digits * 10^point.
    let point = exponent + 1;
    if (-3..=16).contains(&point) {
        let mut out = String::from(sign);
        if point <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-point) as usize));
            out.push_str(&digits);
        } else if point as usize >= digits.len() {
            out.push_str(&digits);
            out.extend(std::iter::repeat_n('0', point as usize - digits.len()));
            out.push_str(".0");
        } else {
            out.push_str(&digits[..point as usize]);
            out.push('.');
            out.push_str(&digits[point as usize..]);
        }
        out
    } else {
        let mut out = format!("{sign}{}", &digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs());
        out
    }
}

/// `json.dumps`'s options that templates pass to `tojson`.
#[derive(Clone, Debug)]
pub struct Dumps<'a> {
    pub ensure_ascii: bool,
    pub indent: Option<Cow<'a, str>>,
    pub item_separator: Cow<'a, str>,
    pub key_separator: Cow<'a, str>,
    pub sort_keys: bool,
}

impl Default for Dumps<'_> {
    /// transformers' `tojson`: `json.dumps(x, ensure_ascii=False)`.
    fn default() -> Self {
        Dumps { ensure_ascii: false, indent: None, item_separator: ", ".into(), key_separator: ": ".into(), sort_keys: false }
    }
}

impl Dumps<'_> {
    /// Splash's json_codec: compact, UTF-8, with U+0085, U+2028 and U+2029 escaped for SSE readers.
    pub fn codec() -> Dumps<'static> {
        Dumps { item_separator: ",".into(), key_separator: ":".into(), ..Dumps::default() }
    }

    pub fn dumps(&self, value: &Value) -> String {
        let mut out = String::new();
        self.write(&mut out, value, 0);
        out
    }

    fn newline(&self, out: &mut String, depth: usize) {
        if let Some(indent) = &self.indent {
            out.push('\n');
            for _ in 0..depth {
                out.push_str(indent);
            }
        }
    }

    fn write(&self, out: &mut String, value: &Value, depth: usize) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&number_text(n)),
            Value::String(s) => write_string(out, s, self.ensure_ascii),
            Value::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&self.item_separator);
                    }
                    self.newline(out, depth + 1);
                    self.write(out, item, depth + 1);
                }
                self.newline(out, depth);
                out.push(']');
            }
            Value::Object(map) => {
                if map.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                let mut entries: Vec<_> = map.iter().collect();
                if self.sort_keys {
                    entries.sort_by(|a, b| py_str_cmp(a.0, b.0));
                }
                for (i, (key, item)) in entries.into_iter().enumerate() {
                    if i > 0 {
                        out.push_str(&self.item_separator);
                    }
                    self.newline(out, depth + 1);
                    write_string(out, key, self.ensure_ascii);
                    out.push_str(&self.key_separator);
                    self.write(out, item, depth + 1);
                }
                self.newline(out, depth);
                out.push('}');
            }
        }
    }
}

/// Python compares strings by code point; UTF-8 byte order is the same order.
fn py_str_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.cmp(b)
}

fn number_text(n: &serde_json::Number) -> String {
    if n.is_f64() {
        float_repr(n.as_f64().expect("f64"))
    } else {
        n.to_string()
    }
}

fn write_string(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ensure_ascii && (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `json_codec.dumps`: compact JSON with separators SSE readers split lines at escaped.
pub fn codec_dumps(value: &Value) -> String {
    escape_separators(Dumps::codec().dumps(value))
}

/// `json_codec.dumps(text)` for a string.
pub fn codec_dumps_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    write_string(&mut out, text, false);
    escape_separators(out)
}

fn escape_separators(text: String) -> String {
    if !text.contains(['\u{85}', '\u{2028}', '\u{2029}']) {
        return text;
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\u{85}' | '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// JSON nests at most this deep in what the server reads (json_codec.MAX_DEPTH).
pub const MAX_DEPTH: usize = 128;

/// `json_codec.loads`: strict JSON (no NaN or Infinity, finite numbers) nesting at most MAX_DEPTH deep.
///
/// Divergence: integers beyond 64 bits read as floats here (serde_json without arbitrary_precision), where Python
/// keeps them exact.
pub fn codec_loads(text: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut level: Vec<&Value> = vec![&value];
    let mut depth = 0;
    loop {
        level.retain(|v| v.is_array() || v.is_object());
        if level.is_empty() {
            return Ok(value);
        }
        depth += 1;
        if depth > MAX_DEPTH {
            return Err(format!("JSON nests deeper than {MAX_DEPTH} levels"));
        }
        level = level
            .into_iter()
            .flat_map(|v| match v {
                Value::Array(items) => items.iter().collect::<Vec<_>>(),
                Value::Object(map) => map.values().collect(),
                _ => Vec::new(),
            })
            .collect();
    }
}

/// JSON text with each `\uXXXX` escape of an unpaired surrogate replaced by `\ufffd`.
///
/// Python reads such an escape as a lone surrogate, which the request then carries until `tokenizable()` turns it
/// into U+FFFD before encoding; Rust strings cannot hold one, so the replacement happens before parsing. Rendered
/// prompts then equal Python's `tokenizable(rendered)` and token ids match exactly.
pub fn replace_lone_surrogates(json: &str) -> Cow<'_, str> {
    if !json.contains("\\u") && !json.contains("\\U") {
        return Cow::Borrowed(json);
    }
    let bytes = json.as_bytes();
    let surrogate_at = |i: usize| -> Option<u16> {
        // `\uXXXX` at i whose value is a surrogate.
        let hex = bytes.get(i + 2..i + 6)?;
        if bytes[i] != b'\\' || (bytes[i + 1] != b'u') {
            return None;
        }
        let value = u16::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
        (0xD800..=0xDFFF).contains(&value).then_some(value)
    };
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_string {
            if b == b'"' {
                in_string = true;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = false;
                i += 1;
            }
            b'\\' => {
                let Some(unit) = surrogate_at(i) else {
                    i += 2;
                    continue;
                };
                let paired = (0xD800..0xDC00).contains(&unit)
                    && surrogate_at(i + 6).is_some_and(|low| (0xDC00..=0xDFFF).contains(&low));
                if paired {
                    i += 12;
                    continue;
                }
                let buffer = out.get_or_insert_with(|| String::with_capacity(json.len()));
                buffer.push_str(&json[copied..i]);
                buffer.push_str("\\ufffd");
                i += 6;
                copied = i;
            }
            _ => i += 1,
        }
    }
    match out {
        None => Cow::Borrowed(json),
        Some(mut buffer) => {
            buffer.push_str(&json[copied..]);
            Cow::Owned(buffer)
        }
    }
}

/// Python's `repr(str)`.
pub fn str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !is_printable(c) => {
                let n = c as u32;
                let _ = if n < 0x100 {
                    write!(out, "\\x{n:02x}")
                } else if n < 0x10000 {
                    write!(out, "\\u{n:04x}")
                } else {
                    write!(out, "\\U{n:08x}")
                };
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `str.isprintable()` for one character: not a control, format, separator other than space, private-use
/// or surrogate character. Unassigned code points (which Python also escapes) are not known here and print as is.
fn is_printable(c: char) -> bool {
    let n = c as u32;
    !(n < 0x20
        || (0x7f..=0xa0).contains(&n)
        || n == 0xad
        || (0x600..=0x605).contains(&n)
        || n == 0x61c
        || n == 0x6dd
        || n == 0x70f
        || n == 0x890
        || n == 0x891
        || n == 0x8e2
        || n == 0x1680
        || n == 0x180e
        || (0x2000..=0x200f).contains(&n)
        || (0x2028..=0x202f).contains(&n)
        || (0x205f..=0x2064).contains(&n)
        || (0x2066..=0x206f).contains(&n)
        || n == 0x3000
        || (0xe000..=0xf8ff).contains(&n)
        || n == 0xfeff
        || (0xfff9..=0xfffb).contains(&n)
        || n == 0x110bd
        || n == 0x110cd
        || (0x13430..=0x1343f).contains(&n)
        || (0x1bca0..=0x1bca3).contains(&n)
        || (0x1d173..=0x1d17a).contains(&n)
        || n == 0xe0001
        || (0xe0020..=0xe007f).contains(&n)
        || n >= 0xf0000)
}

/// What Python's `str()` prints for a JSON-shaped value: `True`, `None`, `1e+16`, `{'a': [1, 'b']}`.
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

pub fn py_repr(value: &Value) -> String {
    let mut out = String::new();
    write_repr(&mut out, value);
    out
}

fn write_repr(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("None"),
        Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Value::Number(n) => out.push_str(&number_text(n)),
        Value::String(s) => out.push_str(&str_repr(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_repr(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => write_map_repr(out, map),
    }
}

fn write_map_repr(out: &mut String, map: &Map<String, Value>) {
    out.push('{');
    for (i, (key, item)) in map.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&str_repr(key));
        out.push_str(": ");
        write_repr(out, item);
    }
    out.push('}');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_as_python() {
        for (value, text) in [
            (1.0, "1.0"),
            (2.5, "2.5"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1e15, "1000000000000000.0"),
            (1e-4, "0.0001"),
            (1e-5, "1e-05"),
            (1e-7, "1e-07"),
            (-0.0, "-0.0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (123456.789, "123456.789"),
            (1e300, "1e+300"),
        ] {
            assert_eq!(float_repr(value), text, "{value}");
        }
    }

    #[test]
    fn lone_surrogates_become_replacement() {
        assert_eq!(replace_lone_surrogates(r#"{"a":"x\ud83d y"}"#), r#"{"a":"x\ufffd y"}"#);
        assert_eq!(replace_lone_surrogates(r#"{"a":"\ud83d\ude00"}"#), r#"{"a":"\ud83d\ude00"}"#);
        assert_eq!(replace_lone_surrogates(r#"{"a":"\\ud83d"}"#), r#"{"a":"\\ud83d"}"#);
        assert_eq!(replace_lone_surrogates(r#"{"a":"\udc00\ud83d"}"#), r#"{"a":"\ufffd\ufffd"}"#);
    }

    #[test]
    fn strings_repr_as_python() {
        assert_eq!(str_repr("it's"), "\"it's\"");
        assert_eq!(str_repr("say \"hi\""), "'say \"hi\"'");
        assert_eq!(str_repr("both ' \""), "'both \\' \"'");
        assert_eq!(str_repr("é\n\x01\u{2028}"), "'é\\n\\x01\\u2028'");
    }
}

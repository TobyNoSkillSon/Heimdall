//! JSON text exactly as Foundation's `JSONSerialization` writes it with `.sortedKeys, .withoutEscapingSlashes` (and
//! `.prettyPrinted` for files), so the Rust build's files and `--json` output are byte-for-byte the Swift build's:
//! doubles as `%.17g`, keys in Foundation's sorted order (numeric, case-insensitive, forced ordering), `" : "` and two
//! spaces when pretty, empty containers as `[\n\n]` at the current indent, `/` unescaped.

use std::cmp::Ordering;

use objc2_foundation::{NSComparisonResult, NSLocale, NSRange, NSString, NSStringCompareOptions};
use serde_json::Value;

/// One value as JSONSerialization writes it (no trailing newline).
pub fn to_string(v: &Value, pretty: bool) -> String {
    let mut out = String::new();
    write(&mut out, v, pretty, 0);
    out
}

/// The order `.sortedKeys` puts two keys in.
pub fn key_order(a: &str, b: &str) -> Ordering {
    let (x, y) = (NSString::from_str(a), NSString::from_str(b));
    let opts = NSStringCompareOptions::NumericSearch
        | NSStringCompareOptions::CaseInsensitiveSearch
        | NSStringCompareOptions::ForcedOrderingSearch;
    let locale = NSLocale::systemLocale();
    // SAFETY: the locale is an NSLocale; the range is the receiver's whole length.
    let r = unsafe { x.compare_options_range_locale(&y, opts, NSRange::new(0, x.length()), Some(&locale)) };
    match r {
        NSComparisonResult::Ascending => Ordering::Less,
        NSComparisonResult::Descending => Ordering::Greater,
        _ => a.cmp(b),
    }
}

/// A double as `%.17g` (what JSONSerialization writes for every non-integer NSNumber).
pub fn double(x: f64) -> String {
    let mut buf = [0u8; 64];
    // SAFETY: the buffer is large enough for any %.17g output and NUL-terminated by snprintf.
    let n = unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), c"%.17g".as_ptr(), x) };
    String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned()
}

fn number(n: &serde_json::Number) -> String {
    if n.is_i64() || n.is_u64() {
        return n.to_string();
    }
    double(n.as_f64().unwrap_or(0.0))
}

fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write(out: &mut String, v: &Value, pretty: bool, level: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n)),
        Value::String(s) => string(out, s),
        Value::Array(a) => {
            out.push('[');
            if pretty {
                out.push('\n');
                if a.is_empty() {
                    out.push('\n');
                }
            }
            for (k, e) in a.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                    if pretty {
                        out.push('\n');
                    }
                }
                if pretty {
                    indent(out, level + 1);
                }
                write(out, e, pretty, level + 1);
            }
            if pretty {
                if !a.is_empty() {
                    out.push('\n');
                }
                indent(out, level);
            }
            out.push(']');
        }
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_by(|a, b| key_order(a, b));
            out.push('{');
            if pretty {
                out.push('\n');
                if keys.is_empty() {
                    out.push('\n');
                }
            }
            for (k, key) in keys.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                    if pretty {
                        out.push('\n');
                    }
                }
                if pretty {
                    indent(out, level + 1);
                }
                string(out, key);
                out.push_str(if pretty { " : " } else { ":" });
                write(out, &o[*key], pretty, level + 1);
            }
            if pretty {
                if !keys.is_empty() {
                    out.push('\n');
                }
                indent(out, level);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_like_json_serialization() {
        let v: Value = serde_json::from_str(r#"{"b":[],"a":{"x":1.0,"y":0.1,"z":[1,"a/b\u0001"]},"c":{}}"#).unwrap();
        assert_eq!(to_string(&v, false), r#"{"a":{"x":1,"y":0.10000000000000001,"z":[1,"a/b\u0001"]},"b":[],"c":{}}"#);
        assert_eq!(
            to_string(&v, true),
            "{\n  \"a\" : {\n    \"x\" : 1,\n    \"y\" : 0.10000000000000001,\n    \"z\" : [\n      1,\n      \"a/b\\u0001\"\n    ]\n  },\n  \"b\" : [\n\n  ],\n  \"c\" : {\n\n  }\n}"
        );
        assert_eq!(double(1e17), "1e+17");
        assert_eq!(double(1.5e-7), "1.4999999999999999e-07");
        assert_eq!(double(-0.0), "-0");
        assert_eq!(double(1792715824.8), "1792715824.8");
    }

    #[test]
    fn keys_sort_like_foundation() {
        // Measured on the Swift build (23 Oct): numeric, case-insensitive, lowercase first on a tie, é with e.
        let mut k = vec!["B", "a", "_", "é", "Z1", "z", "10", "9", "aB", "ab", "id", "s", "started"];
        k.sort_by(|a, b| key_order(a, b));
        assert_eq!(k, ["_", "9", "10", "a", "ab", "aB", "B", "é", "id", "s", "started", "z", "Z1"]);
    }
}

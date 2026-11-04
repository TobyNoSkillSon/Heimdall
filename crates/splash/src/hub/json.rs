//! JSON encoded byte-for-byte as Python's `json.dumps` encodes it, because Splash keys assemblies and metadata
//! entries by the SHA-256 of those bytes: `canonical` is models.json_bytes (sorted keys, two-space indent,
//! `ensure_ascii=False`, trailing newline) and `compact` is `json.dumps(v, separators=(",", ":"))`.
use serde_json::Value;

/// models.json_bytes: the same value always encodes to the same bytes, as Splash writes them.
pub fn canonical(value: &Value) -> Vec<u8> {
    let mut out = String::new();
    write_indented(&mut out, value, 0, 2, true, false);
    out.push('\n');
    out.into_bytes()
}

/// `json.dumps(value, separators=(",", ":"))`: insertion order, ASCII only.
pub fn compact(value: &Value) -> String {
    let mut out = String::new();
    write_compact(&mut out, value);
    out
}

/// `json.dump(value, indent=1)` as huggingface_hub writes its tree cache: insertion order, ASCII only.
pub fn indent_one(value: &Value) -> String {
    let mut out = String::new();
    write_indented(&mut out, value, 0, 1, false, true);
    out
}

fn write_indented(out: &mut String, value: &Value, depth: usize, step: usize, sort: bool, ascii: bool) {
    let pad = |out: &mut String, depth: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', depth * step));
    };
    match value {
        Value::Array(items) if !items.is_empty() => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pad(out, depth + 1);
                write_indented(out, item, depth + 1, step, sort, ascii);
            }
            pad(out, depth);
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            let mut entries: Vec<_> = map.iter().collect();
            if sort {
                // Python sorts str keys by code point, which is UTF-8 byte order.
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pad(out, depth + 1);
                write_string(out, key, ascii);
                out.push_str(": ");
                write_indented(out, item, depth + 1, step, sort, ascii);
            }
            pad(out, depth);
            out.push('}');
        }
        other => write_scalar(out, other, ascii),
    }
}

fn write_compact(out: &mut String, value: &Value) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key, true);
                out.push(':');
                write_compact(out, item);
            }
            out.push('}');
        }
        other => write_scalar(out, other, true),
    }
}

fn write_scalar(out: &mut String, value: &Value, ascii: bool) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) if !n.is_f64() => out.push_str(&i.to_string()),
            (_, Some(u), _) if !n.is_f64() => out.push_str(&u.to_string()),
            (_, _, Some(f)) => out.push_str(&float_repr(f)),
            _ => out.push_str(&n.to_string()),
        },
        Value::String(s) => write_string(out, s, ascii),
        Value::Array(_) => out.push_str("[]"),
        Value::Object(_) => out.push_str("{}"),
    }
}

/// Python's json string escaping: `"`, `\`, and control characters; with `ascii`, every non-ASCII character as
/// `\uXXXX` (UTF-16 surrogate pairs above the BMP).
fn write_string(out: &mut String, s: &str, ascii: bool) {
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
            c if ascii && (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python's `float.__repr__`: the shortest round-tripping digits, positional for decimal exponents in [-4, 16),
/// else scientific with a signed exponent of at least two digits; integral values keep `.0`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    // Rust's `{:e}` gives the same shortest digits: d[.ddd]e[-]x.
    let sci = format!("{:e}", f.abs());
    let (mantissa, exponent) = sci.split_once('e').expect("exponent");
    let exponent: i32 = exponent.parse().expect("exponent digits");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let sign = if f < 0.0 { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        };
        format!("{sign}{body}")
    } else {
        let (head, tail) = digits.split_at(1);
        let fraction = if tail.is_empty() { String::new() } else { format!(".{tail}") };
        let esign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{head}{fraction}e{esign}{:02}", exponent.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_print_as_python_repr() {
        for (value, repr) in [
            (0.5, "0.5"),
            (1.0, "1.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123456789012345.0, "123456789012345.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1e-6f32 as f64, "9.999999974752427e-07"),
            (-2.5e-7, "-2.5e-07"),
            (1e100, "1e+100"),
            (10000000.0, "10000000.0"),
        ] {
            assert_eq!(float_repr(value), repr);
        }
    }

    #[test]
    fn canonical_matches_json_bytes() {
        let value = json!({"b": [1, {"y": [], "x": {}}], "a": "é\"\n\u{1}", "c": 0.5, "d": null});
        let expected = "{\n  \"a\": \"é\\\"\\n\\u0001\",\n  \"b\": [\n    1,\n    {\n      \"x\": {},\n      \"y\": []\n    }\n  ],\n  \"c\": 0.5,\n  \"d\": null\n}\n";
        assert_eq!(String::from_utf8(canonical(&value)).unwrap(), expected);
        assert_eq!(compact(&json!(["a/b:V", null, true, "é😀"])), r#"["a/b:V",null,true,"\u00e9\ud83d\ude00"]"#);
    }
}

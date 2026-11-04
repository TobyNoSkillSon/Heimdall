//! Swift and Foundation string behaviour the CLI's output depends on.

use objc2_foundation::NSString;
use serde_json::Value;

use crate::model::jsonfmt;

/// `Double(String)` (decimal, scientific, inf, nan).
pub fn swift_double(s: &str) -> Option<f64> {
    crate::model::plan::swift_double(s)
}

/// `Character.isWhitespace` split, empty pieces dropped.
pub fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

/// `String.count`: user-perceived characters (Foundation's composed character sequences).
pub fn count(s: &str) -> usize {
    let ns = NSString::from_str(s);
    let len = ns.length();
    let (mut i, mut n) = (0, 0);
    while i < len {
        let r = ns.rangeOfComposedCharacterSequenceAtIndex(i);
        i = (r.location + r.length).max(i + 1);
        n += 1;
    }
    n
}

/// `String.prefix(n)` in characters.
pub fn prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `CharacterSet.whitespaces`: tab and the space separators (not newlines).
fn is_space(c: char) -> bool {
    c == '\t' || matches!(c, ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// `trimmingCharacters(in: .whitespaces)`.
pub fn trim_spaces(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `localizedCaseInsensitiveContains` (an empty needle matches nothing).
pub fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    NSString::from_str(hay).localizedCaseInsensitiveContainsString(&NSString::from_str(needle))
}

/// `"\(value)"` for a value from JSONSerialization: strings as they are, NSNumber's description, `<null>`.
pub fn describe(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        Value::Null => "<null>".into(),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            let mut buf = [0u8; 64];
            // SAFETY: a %.16g fits the buffer; snprintf NUL-terminates it.
            let k = unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), c"%.16g".as_ptr(), x) };
            String::from_utf8_lossy(&buf[..k.max(0) as usize]).into_owned()
        }
        v => jsonfmt::to_string(v, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::file::{deleting_path_extension, last_path_component};

    #[test]
    fn swift_string_rules() {
        assert_eq!(count("e\u{301}x"), 2);
        assert_eq!(count("👍🏽!"), 2);
        assert_eq!(words("  a  b\tc\n"), 3);
        assert_eq!(trim_spaces(" \tk \n"), "k \n");
        assert!(contains_ci("Arsenal: push", "PUSH"));
        assert!(!contains_ci("x", ""));
        assert_eq!(deleting_path_extension("a.tar.gz"), "a.tar");
        assert_eq!(deleting_path_extension(".hidden"), ".hidden");
        assert_eq!(deleting_path_extension("dot."), "dot.");
        assert_eq!(last_path_component("/a/b.md"), "b.md");
        assert_eq!(describe(&serde_json::json!(0.5)), "0.5");
        assert_eq!(describe(&serde_json::json!(null)), "<null>");
    }
}

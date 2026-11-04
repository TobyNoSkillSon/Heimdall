//! JSON as Python holds it while tool_schema.py frames schemas: values are shared references, so one node can sit in
//! several places and be told apart from an equal copy, as `is` and `id()` do there. Framing deduplicates by identity
//! and memoizes readers by it, and its grammars serialize the result with `json.dumps`; keeping the same object model
//! keeps the grammar bytes identical.
use serde_json::{Number, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A JSON value with Python's reference semantics: cloning shares, `deep_copy` copies.
#[derive(Clone, Debug)]
pub(crate) enum J {
    Null,
    Bool(bool),
    Num(Rc<Number>),
    Str(Rc<str>),
    Arr(Rc<RefCell<Vec<J>>>),
    Obj(Rc<RefCell<Obj>>),
}

/// An object in insertion order, as a Python dict keeps it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Obj(pub Vec<(String, J)>);

impl Obj {
    pub fn get(&self, key: &str) -> Option<&J> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn contains(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }
    /// `d[key] = value`: in place where the key exists, else at the end.
    pub fn set(&mut self, key: &str, value: J) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key.to_string(), value)),
        }
    }
    /// `d.pop(key, None)`.
    pub fn remove(&mut self, key: &str) -> Option<J> {
        let index = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(index).1)
    }
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
}

/// Where `id()` would place a value: shared references compare by address, singletons by value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Id {
    Null,
    Bool(bool),
    Ptr(usize),
}

impl J {
    pub fn str(s: &str) -> J {
        J::Str(Rc::from(s))
    }
    pub fn obj(entries: Vec<(String, J)>) -> J {
        J::Obj(Rc::new(RefCell::new(Obj(entries))))
    }
    pub fn arr(items: Vec<J>) -> J {
        J::Arr(Rc::new(RefCell::new(items)))
    }
    pub fn empty_obj() -> J {
        J::obj(Vec::new())
    }

    pub fn id(&self) -> Id {
        match self {
            J::Null => Id::Null,
            J::Bool(b) => Id::Bool(*b),
            J::Num(n) => Id::Ptr(Rc::as_ptr(n) as usize),
            J::Str(s) => Id::Ptr(Rc::as_ptr(s) as *const u8 as usize),
            J::Arr(a) => Id::Ptr(Rc::as_ptr(a) as usize),
            J::Obj(o) => Id::Ptr(Rc::as_ptr(o) as usize),
        }
    }
    /// `a is b`.
    pub fn is(&self, other: &J) -> bool {
        self.id() == other.id()
    }
    pub fn is_bool(&self, b: bool) -> bool {
        matches!(self, J::Bool(x) if *x == b)
    }
    pub fn as_obj(&self) -> Option<&Rc<RefCell<Obj>>> {
        match self {
            J::Obj(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&Rc<RefCell<Vec<J>>>> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }
    /// `node.get(key)` on a dict; None for anything else.
    pub fn get(&self, key: &str) -> Option<J> {
        self.as_obj().and_then(|o| o.borrow().get(key).cloned())
    }
    pub fn has(&self, key: &str) -> bool {
        self.as_obj().is_some_and(|o| o.borrow().contains(key))
    }
    /// Python's `isinstance(value, (int, float))`, which a bool also is: its number, if any.
    pub fn number(&self) -> Option<f64> {
        match self {
            J::Bool(b) => Some(*b as u8 as f64),
            J::Num(n) => n.as_f64(),
            _ => None,
        }
    }

    pub fn from_value(value: &Value) -> J {
        match value {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Number(n) => J::Num(Rc::new(n.clone())),
            Value::String(s) => J::str(s),
            Value::Array(items) => J::arr(items.iter().map(J::from_value).collect()),
            Value::Object(map) => J::obj(map.iter().map(|(k, v)| (k.clone(), J::from_value(v))).collect()),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            J::Null => Value::Null,
            J::Bool(b) => Value::Bool(*b),
            J::Num(n) => Value::Number((**n).clone()),
            J::Str(s) => Value::String(s.to_string()),
            J::Arr(a) => Value::Array(a.borrow().iter().map(J::to_value).collect()),
            J::Obj(o) => Value::Object(o.borrow().0.iter().map(|(k, v)| (k.clone(), v.to_value())).collect()),
        }
    }

    /// `copy.deepcopy`: containers copied once each, sharing within the value kept, atoms shared.
    pub fn deep_copy(&self) -> J {
        fn copy(value: &J, memo: &mut HashMap<Id, J>) -> J {
            match value {
                J::Arr(_) | J::Obj(_) => {
                    if let Some(done) = memo.get(&value.id()) {
                        return done.clone();
                    }
                    let out = match value {
                        J::Arr(a) => {
                            let out = J::arr(Vec::new());
                            memo.insert(value.id(), out.clone());
                            let items: Vec<J> = a.borrow().iter().map(|v| copy(v, memo)).collect();
                            *out.as_arr().unwrap().borrow_mut() = items;
                            out
                        }
                        J::Obj(o) => {
                            let out = J::empty_obj();
                            memo.insert(value.id(), out.clone());
                            let entries: Vec<(String, J)> =
                                o.borrow().0.iter().map(|(k, v)| (k.clone(), copy(v, memo))).collect();
                            out.as_obj().unwrap().borrow_mut().0 = entries;
                            out
                        }
                        _ => unreachable!(),
                    };
                    out
                }
                other => other.clone(),
            }
        }
        copy(self, &mut HashMap::new())
    }

    /// Python `==` between JSON values (1 == 1.0, True == 1).
    pub fn py_eq(&self, other: &J) -> bool {
        match (self, other) {
            (J::Null, J::Null) => true,
            (J::Str(a), J::Str(b)) => a == b,
            (J::Arr(a), J::Arr(b)) => {
                let (a, b) = (a.borrow(), b.borrow());
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.py_eq(y))
            }
            (J::Obj(a), J::Obj(b)) => {
                let (a, b) = (a.borrow(), b.borrow());
                a.0.len() == b.0.len() && a.0.iter().all(|(k, v)| b.get(k).is_some_and(|w| v.py_eq(w)))
            }
            (a, b) => match (a.number(), b.number()) {
                (Some(x), Some(y)) => match (a, b) {
                    (J::Num(m), J::Num(n)) if !m.is_f64() && !n.is_f64() => m == n,
                    _ => x == y,
                },
                _ => false,
            },
        }
    }

    /// The JSON type Python's json reads this value as (tool_schema.JSON_TYPES).
    pub fn json_type(&self) -> &'static str {
        match self {
            J::Null => "null",
            J::Bool(_) => "boolean",
            J::Num(n) if n.is_f64() => "number",
            J::Num(_) => "integer",
            J::Str(_) => "string",
            J::Arr(_) => "array",
            J::Obj(_) => "object",
        }
    }
}

/// `json.dumps(value)` with ASCII escapes; `compact` is `separators=(",", ":")`.
pub(crate) fn dumps(value: &J, compact: bool) -> String {
    let mut out = String::new();
    write_json(value, compact, &mut out);
    out
}

fn write_json(value: &J, compact: bool, out: &mut String) {
    let (item, key) = if compact { (",", ":") } else { (", ", ": ") };
    match value {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(n) => out.push_str(&number_repr(n)),
        J::Str(s) => out.push_str(&dumps_str(s)),
        J::Arr(a) => {
            out.push('[');
            for (i, v) in a.borrow().iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                write_json(v, compact, out);
            }
            out.push(']');
        }
        J::Obj(o) => {
            out.push('{');
            for (i, (k, v)) in o.borrow().0.iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                out.push_str(&dumps_str(k));
                out.push_str(key);
                write_json(v, compact, out);
            }
            out.push('}');
        }
    }
}

/// `json.dumps(text)`: quoted, with everything outside printable ASCII escaped.
pub(crate) fn dumps_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

/// `repr(text)` for a str: single quotes unless the text holds one and no double quote; control characters escaped.
pub(crate) fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
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
            c if (c as u32) < 0x20 || (0x7f..0xa0).contains(&(c as u32)) => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// A number as Python writes it: integers in full, floats by `repr` (shortest digits, exponent past 1e16 or under
/// 1e-4, at least two exponent digits).
fn number_repr(n: &Number) -> String {
    if !n.is_f64() {
        return n.to_string();
    }
    let x = n.as_f64().unwrap_or(0.0);
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let sci = format!("{:e}", x.abs());
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let point = exponent + 1;
    let sign = if x < 0.0 { "-" } else { "" };
    if -4 < point && point <= 16 {
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if (point as usize) >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        };
        format!("{sign}{body}")
    } else {
        let mantissa = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits };
        let esign = if exponent < 0 { "-" } else { "+" };
        format!("{sign}{mantissa}e{esign}{:02}", exponent.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repr(text: &str) -> String {
        number_repr(&serde_json::from_str::<Number>(text).unwrap())
    }

    #[test]
    fn floats_print_as_python_repr() {
        assert_eq!(repr("1.0"), "1.0");
        assert_eq!(repr("1e16"), "1e+16");
        assert_eq!(repr("1e15"), "1000000000000000.0");
        assert_eq!(repr("0.0001"), "0.0001");
        assert_eq!(repr("0.00001"), "1e-05");
        assert_eq!(repr("-2.5e-7"), "-2.5e-07");
        assert_eq!(repr("123.456"), "123.456");
        assert_eq!(repr("1e100"), "1e+100");
        assert_eq!(repr("12"), "12");
    }

    #[test]
    fn repr_quotes_as_python() {
        assert_eq!(py_repr("("), "'('");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'\"\\\n"), "'a\\'\"\\\\\\n'");
    }

    #[test]
    fn strings_escape_as_python_ascii() {
        assert_eq!(dumps_str("a\"\\\n\u{7f}\u{e9}\u{1f600}"), r#""a\"\\\n\u007f\u00e9\ud83d\ude00""#);
    }
}

//! The island file format, shared by the app and the `islands` CLI (Swift: `IslandModel`).
//! One JSON object per island in `<root>/islands/<id>.json`; clicks go to `<root>/responses/<id>-<ms>-<pid>-<n>.json`.

pub mod due;
pub mod file;
pub mod flash;
pub mod goals;
pub mod geometry;
pub mod glance;
pub mod html;
pub mod html_charts;
pub mod island;
pub mod jsonfmt;
pub mod layout;
pub mod onesys;
pub mod plan;
pub mod share;
pub mod settled;
pub mod time;
pub mod timeline;
pub mod toggle;
pub mod viewer_kind;
pub mod zoom;

pub use due::Due;
pub use file::{IslandFile, Paths, Visibility};
pub use goals::{Goal, GoalState, GoalTask, Goals, TaskState};
pub use geometry::{Point, Rect, Size, Vector};
pub use island::{Dock, FrameOrigin, Island, OpenButton, OpenRoute, OpenTarget, Presets, Step, TodoGroup};
pub use layout::{auto_column, edge_column, edge_drop, left_column, left_edge_column, snap, top_row};
pub use plan::{Attention, ItemDiff, ItemLive, Lifetime, Plan, PlanItem, PlanList, Proposal, Responses, PLAN_SYMBOLS};
pub use viewer_kind::{ViewerAction, ViewerKind};

pub use serde_json::Value;

/// One JSON object (Swift `[String: Any]`).
pub type Json = serde_json::Map<String, Value>;

/// Readers that behave like Swift's `as?` casts on `JSONSerialization` output, so the two builds read every file the
/// same way (an NSNumber bridges to Bool only for 0 and 1, to Int only when whole; an array of objects is all or
/// nothing).
pub mod j {
    use super::{Json, Value};

    pub fn str(v: Option<&Value>) -> Option<&str> {
        v.and_then(Value::as_str)
    }

    /// `as? String`, empty treated as missing.
    pub fn nonempty(v: Option<&Value>) -> Option<&str> {
        str(v).filter(|s| !s.is_empty())
    }

    /// `as? NSNumber` → doubleValue (JSON booleans are NSNumbers too).
    pub fn num(v: Option<&Value>) -> Option<f64> {
        match v? {
            Value::Number(n) => n.as_f64(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    /// `as? Bool`: a JSON boolean, or a number that is exactly 0 or 1.
    pub fn boolean(v: Option<&Value>) -> Option<bool> {
        match v? {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => match n.as_f64() {
                Some(x) if x == 0.0 => Some(false),
                Some(x) if x == 1.0 => Some(true),
                _ => None,
            },
            _ => None,
        }
    }

    /// `as? Int`: a whole number.
    pub fn int(v: Option<&Value>) -> Option<i64> {
        match v? {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|x| x.fract() == 0.0 && x.abs() < 9e15).map(|x| x as i64)),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    pub fn obj(v: Option<&Value>) -> Option<&Json> {
        v.and_then(Value::as_object)
    }

    pub fn arr(v: Option<&Value>) -> Option<&Vec<Value>> {
        v.and_then(Value::as_array)
    }

    /// `as? [JSONObject]`: every element an object, else nothing.
    pub fn objs(v: Option<&Value>) -> Option<Vec<&Json>> {
        arr(v)?.iter().map(Value::as_object).collect()
    }

    /// A number as JSONSerialization writes it: whole values without a fraction.
    pub fn number(x: f64) -> Value {
        if x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15 {
            Value::from(x as i64)
        } else {
            serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
        }
    }

    /// NSDictionary/NSArray equality: numbers compare by value (1 == 1.0), everything else structurally.
    pub fn eq(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
            (Value::Bool(x), Value::Bool(y)) => x == y,
            // NSNumber(bool) and NSNumber(int) are equal when the values match.
            (Value::Bool(x), Value::Number(y)) | (Value::Number(y), Value::Bool(x)) => y.as_f64() == Some(if *x { 1.0 } else { 0.0 }),
            (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| eq(a, b)),
            (Value::Object(x), Value::Object(y)) => obj_eq(x, y),
            _ => a == b,
        }
    }

    pub fn obj_eq(a: &Json, b: &Json) -> bool {
        a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| eq(v, w)))
    }

    /// Optional values equal (both missing, or both there and equal).
    pub fn opt_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => eq(a, b),
            _ => false,
        }
    }

    /// Builds an object from pairs (tests and the app's own writes).
    pub fn object<const N: usize>(pairs: [(&str, Value); N]) -> Json {
        pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
    }
}

/// `serde_json::json!` as a `Json` object.
#[macro_export]
macro_rules! jobj {
    ($($t:tt)*) => {
        match serde_json::json!({ $($t)* }) { serde_json::Value::Object(o) => o, _ => unreachable!() }
    };
}

/// Swift's `Island.format`: whole values without a fraction, else at most two decimals without trailing zeros.
pub fn format_number(v: f64) -> String {
    if v == v.round() && v.abs() < 1e9 {
        return format!("{}", v as i64);
    }
    let s = if v.abs() >= 100.0 {
        format!("{v:.0}")
    } else if v.abs() >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    };
    if s.contains('.') {
        let t = s.trim_end_matches('0');
        t.trim_end_matches('.').to_owned()
    } else {
        s
    }
}

//! JSON text for island files and the CLI's `--json` output: keys sorted in every build (the workspace may turn on
//! serde_json's `preserve_order`), and whole doubles written as integers, as the Swift build's JSONSerialization did,
//! so a reader that asks for an integer (`as_i64`, a `due` or a count) still gets one. It replaced a byte-for-byte copy
//! of JSONSerialization's output when the Swift build was retired (14 Nov); every reader parses the JSON.

use serde_json::Value;

/// One value as JSON text (pretty: two-space indent; no trailing newline).
pub fn to_string(v: &Value, pretty: bool) -> String {
    let v = canonical(v);
    if pretty { serde_json::to_string_pretty(&v) } else { serde_json::to_string(&v) }.unwrap_or_default()
}

/// `v` with sorted keys and whole doubles (within ±2^53) as integers.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().map(|k| (k.clone(), canonical(&o[k]))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        Value::Number(n) if n.is_f64() => match n.as_f64() {
            Some(x) if x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_992.0 => Value::from(x as i64),
            _ => v.clone(),
        },
        _ => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_sorted_and_whole_doubles_as_integers() {
        let v: Value = serde_json::from_str(r#"{"b":[],"a":{"z":[1,"a/b\u0001"],"x":1.0,"y":0.1},"c":{}}"#).unwrap();
        assert_eq!(to_string(&v, false), r#"{"a":{"x":1,"y":0.1,"z":[1,"a/b\u0001"]},"b":[],"c":{}}"#);
        let pretty = to_string(&v, true);
        assert!(pretty.starts_with("{\n  \"a\": {\n    \"x\": 1,\n"), "{pretty}");
        let back: Value = serde_json::from_str(&pretty).unwrap();
        assert_eq!(back["a"]["x"].as_i64(), Some(1), "a whole double reads back as an integer");
        assert_eq!(back["a"]["y"].as_f64(), Some(0.1));
        let due: Value = serde_json::json!({"due": 1792715824.0, "edgeY": 412.5, "big": 1e300});
        assert_eq!(to_string(&due, false), r#"{"big":1e+300,"due":1792715824,"edgeY":412.5}"#);
    }
}

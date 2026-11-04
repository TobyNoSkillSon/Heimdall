//! Response validation (output.validate_response_content): what the grammar leaves out is checked on the final answer.
use serde_json::json;
use splash::constrain::{normalize_response_format, validate_response_content};

#[test]
fn constrain_response_validation() {
    let format = json!({"type": "json_schema", "json_schema": {"schema": {
        "type": "object",
        "properties": {
            "note": {"type": "string", "pattern": "^(?!no)"},
            "day": {"type": "string", "format": "date"},
            "n": {"type": "integer", "maximum": 3}
        },
        "required": ["note"],
        "additionalProperties": false
    }}});
    let format = normalize_response_format(Some(&format)).unwrap().expect("a schema");
    assert!(validate_response_content(r#"{"note": "yes", "day": "not a date", "n": 3}"#, Some(&format)).is_ok());
    for bad in [r#"{"note": "nope"}"#, r#"{"note": "a", "n": 4}"#, r#"{"note": "a", "x": 1}"#, r#"{"note": "a""#, "NaN"] {
        let error = validate_response_content(bad, Some(&format)).unwrap_err();
        assert_eq!((error.status, error.code), (500, "invalid_model_output"), "{bad}");
        assert!(error.message.starts_with("model returned invalid structured output: "), "{bad}");
    }
    assert!(validate_response_content("anything", None).is_ok());
    // json_object is any object; text and null are no format.
    let object = normalize_response_format(Some(&json!({"type": "json_object"}))).unwrap().unwrap();
    assert!(validate_response_content("[1]", Some(&object)).is_err());
    assert!(normalize_response_format(Some(&json!({"type": "text"}))).unwrap().is_none());
    assert!(normalize_response_format(Some(&serde_json::Value::Null)).unwrap().is_none());
    // Draft 7 by declaration; Draft 3 is not read here.
    let draft7 = json!({"type": "json_schema", "json_schema": {"schema": {"$schema": "http://json-schema.org/draft-07/schema#", "items": [{"type": "string"}]}}});
    let draft7 = normalize_response_format(Some(&draft7)).unwrap().unwrap();
    assert!(validate_response_content(r#"["a", 1]"#, Some(&draft7)).is_ok());
    assert!(validate_response_content("[1]", Some(&draft7)).is_err());
    let draft3 = json!({"type": "json_schema", "json_schema": {"schema": {"$schema": "http://json-schema.org/draft-03/schema#"}}});
    assert_eq!(normalize_response_format(Some(&draft3)).unwrap_err().status, 400);
}

#[test]
fn constrain_response_reference_chain_does_not_overflow() {
    let mut defs = serde_json::Map::new();
    for i in 0..9999 {
        defs.insert(format!("d{i}"), json!({"$ref": format!("#/$defs/d{}", i + 1)}));
    }
    defs.insert("d9999".into(), json!({"type": "integer"}));
    let format = json!({"type": "json_schema", "json_schema": {"schema": {"$ref": "#/$defs/d0", "$defs": defs}}});
    let error = normalize_response_format(Some(&format)).unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (400, "invalid response schema: references nest too deeply"));
    // Recursion is not depth: a self-referencing tree is fine.
    let tree = json!({"type": "json_schema", "json_schema": {"schema": {"type": "object", "properties": {"kids": {"type": "array", "items": {"$ref": "#"}}}}}});
    let tree = normalize_response_format(Some(&tree)).unwrap().unwrap();
    assert!(validate_response_content(r#"{"kids": [{"kids": []}]}"#, Some(&tree)).is_ok());
}

//! JSON Schema checks (schema_validation.py, tool_schema.normalize_response_format, output.validate_response_content):
//! a strict tool's schema and a response format's must be valid in their dialect, and a structured answer is checked
//! against its response format once complete, catching what the grammar leaves out (look-around patterns, large
//! bounds, cross-field assertions).
use super::lru::LruCache;
use super::pyjson::J;
use super::pyjson::py_repr;
use super::schema::{check_depth, json_objects, reference_depth, remote_ref, subschemas, walk};
use super::ApiError;
use jsonschema::{Draft, Validator};
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};

const CHECKED_SCHEMAS: usize = 1024;
const VALIDATORS: usize = 256;
const CACHE_BYTES: usize = 8 * 1024 * 1024;
/// The validator compiles references by recursing; deeper chains would overflow its stack (Python fails them with
/// RecursionError).
const MAX_REFERENCE_DEPTH: usize = 512;

/// A schema's dialect by its `$schema`, the latest where it names none or one unknown, as jsonschema's validator_for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    /// Python's jsonschema reads Draft 3; the Rust validator does not, so such schemas are refused.
    Draft3,
    Draft(Draft),
}

fn dialect(schema: &Value) -> Dialect {
    let uri = schema.get("$schema").and_then(Value::as_str).unwrap_or("");
    match uri.strip_suffix('#').unwrap_or(uri) {
        "http://json-schema.org/draft-03/schema" => Dialect::Draft3,
        "http://json-schema.org/draft-04/schema" => Dialect::Draft(Draft::Draft4),
        "http://json-schema.org/draft-06/schema" => Dialect::Draft(Draft::Draft6),
        "http://json-schema.org/draft-07/schema" => Dialect::Draft(Draft::Draft7),
        "https://json-schema.org/draft/2019-09/schema" => Dialect::Draft(Draft::Draft201909),
        _ => Dialect::Draft(Draft::Draft202012),
    }
}

fn meta_check(schema: &Value, dialect: Dialect) -> Result<(), String> {
    let result = match dialect {
        Dialect::Draft3 => return Err("Draft 3 schemas are not supported".into()),
        Dialect::Draft(Draft::Draft4) => jsonschema::draft4::meta::validate(schema),
        Dialect::Draft(Draft::Draft6) => jsonschema::draft6::meta::validate(schema),
        Dialect::Draft(Draft::Draft7) => jsonschema::draft7::meta::validate(schema),
        Dialect::Draft(Draft::Draft201909) => jsonschema::draft201909::meta::validate(schema),
        Dialect::Draft(_) => jsonschema::draft202012::meta::validate(schema),
    };
    result.map_err(|error| error.to_string())?;
    check_patterns(schema, dialect)
}

/// Python's meta-check asserts format "regex" on `pattern` in every dialect and on `patternProperties` names from
/// Draft 6 on. Python compiles with `re`; this compiles with fancy-regex, the engine the validator matches with.
fn check_patterns(schema: &Value, dialect: Dialect) -> Result<(), String> {
    let names = dialect != Dialect::Draft(Draft::Draft4);
    let compiles = |pattern: &str| fancy_regex::Regex::new(pattern).is_ok();
    let mut refused = None;
    walk(&J::from_value(schema), &mut |node| {
        let pattern = node.get("pattern");
        if let Some(pattern) = pattern.as_ref().and_then(J::as_str) {
            if !compiles(pattern) {
                refused = Some(pattern.to_string());
                return false;
            }
        }
        if let Some(properties) = node.get("patternProperties").filter(|_| names) {
            if let Some(object) = properties.as_obj() {
                if let Some(name) = object.borrow().keys().find(|k| !compiles(k)) {
                    refused = Some(name.to_string());
                    return false;
                }
            }
        }
        true
    });
    match refused {
        Some(pattern) => Err(format!("{} is not a 'regex'", py_repr(&pattern))),
        None => Ok(()),
    }
}

/// Whether `schema` is valid in the dialect it declares; why not otherwise. A client sends its tools every turn, so
/// each outcome is kept.
pub(crate) fn check_schema(schema: &Value) -> Result<(), String> {
    static CHECKED: OnceLock<Mutex<LruCache<String, Result<(), String>>>> = OnceLock::new();
    let checked = CHECKED.get_or_init(|| Mutex::new(LruCache::new(CACHE_BYTES, Some(CHECKED_SCHEMAS))));
    let key = schema.to_string();
    if let Some(known) = checked.lock().unwrap().get(&key) {
        return known.clone();
    }
    let outcome = meta_check(schema, dialect(schema));
    let size = key.len();
    checked.lock().unwrap().put(key, outcome.clone(), size);
    outcome
}

/// A request's response_format: the schema its answer is generated to and checked against.
#[derive(Clone)]
pub struct ResponseFormat {
    pub schema: Value,
    pub validator: Arc<Validator>,
}

impl std::fmt::Debug for ResponseFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseFormat").field("schema", &self.schema).finish_non_exhaustive()
    }
}

/// The schema and validator of a request's response_format; None for none or `{"type": "text"}`.
pub fn normalize_response_format(value: Option<&Value>) -> Result<Option<ResponseFormat>, ApiError> {
    let value = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(map)) if map.len() == 1 && map.get("type") == Some(&Value::from("text")) => return Ok(None),
        Some(Value::Object(map)) => map,
        Some(_) => return Err(ApiError::bad_request("response_format must be an object")),
    };
    let schema = match value.get("type").and_then(Value::as_str) {
        Some("json_object") => serde_json::json!({"type": "object"}),
        Some("json_schema") => match value.get("json_schema").and_then(Value::as_object).and_then(|w| w.get("schema")) {
            Some(schema @ (Value::Object(_) | Value::Bool(_))) => schema.clone(),
            _ => return Err(ApiError::bad_request("response_format.json_schema.schema is required")),
        },
        _ => return Err(ApiError::bad_request("unsupported response_format")),
    };
    let tree = J::from_value(&schema);
    check_depth(&tree, "invalid response schema")?;
    if let Some(reference) = remote_ref(&tree)? {
        return Err(ApiError::bad_request(format!("remote schema reference is not allowed: {reference}")));
    }
    let validator = build_validator(&schema)?;
    Ok(Some(ResponseFormat { schema, validator }))
}

/// A response_format schema is the same on every turn of a conversation, so its validator is kept.
fn build_validator(schema: &Value) -> Result<Arc<Validator>, ApiError> {
    static VALIDATORS_CACHE: OnceLock<Mutex<LruCache<String, Arc<Validator>>>> = OnceLock::new();
    let cache = VALIDATORS_CACHE.get_or_init(|| Mutex::new(LruCache::new(CACHE_BYTES, Some(VALIDATORS))));
    let key = schema.to_string();
    if let Some(known) = cache.lock().unwrap().get(&key) {
        return Ok(known.clone());
    }
    let validator = Arc::new(validator(schema)?);
    let size = key.len();
    cache.lock().unwrap().put(key, validator.clone(), size);
    Ok(validator)
}

fn validator(schema: &Value) -> Result<Validator, ApiError> {
    let invalid = |message: String| ApiError::bad_request(format!("invalid response schema: {message}"));
    let base = dialect(schema);
    meta_check(schema, base).map_err(invalid)?;
    let tree = J::from_value(schema);
    if reference_depth(&tree) > MAX_REFERENCE_DEPTH {
        return Err(invalid("references nest too deeply".into()));
    }
    let mut keywords = std::collections::HashSet::new();
    for node in json_objects(&tree) {
        keywords.extend(node.as_obj().unwrap().borrow().keys().map(str::to_string));
    }
    // Kept from Python, whose unbounded standard regex engine found unevaluatedProperties' properties.
    if keywords.contains("patternProperties") && keywords.contains("unevaluatedProperties") {
        return Err(ApiError::bad_request("unevaluatedProperties with patternProperties is not supported"));
    }
    // A document uses one dialect; the declarations go so that a nested one cannot switch it.
    let validated = tree.deep_copy();
    for node in subschemas(&validated) {
        let Some(object) = node.as_obj() else { continue };
        if !object.borrow().contains("$schema") {
            continue;
        }
        if dialect(&node.to_value()) != base {
            return Err(ApiError::bad_request("mixed schema dialects are not supported"));
        }
        object.borrow_mut().remove("$schema");
    }
    let Dialect::Draft(draft) = base else { unreachable!("refused by meta_check") };
    // Python's jsonschema treats format as an annotation unless given a checker; so does this.
    jsonschema::options()
        .with_draft(draft)
        .should_validate_formats(false)
        .build(&validated.to_value())
        .map_err(|error| invalid(error.to_string()))
}

/// Checks a structured answer against its response format; 500 where the model's output breaks it.
pub fn validate_response_content(content: &str, format: Option<&ResponseFormat>) -> Result<(), ApiError> {
    let Some(format) = format else { return Ok(()) };
    let failed = |message: String| ApiError {
        status: 500,
        message: format!("model returned invalid structured output: {message}"),
        code: "invalid_model_output",
    };
    let value: Value = serde_json::from_str(content).map_err(|error| failed(error.to_string()))?;
    format.validator.validate(&value).map_err(|error| failed(error.to_string()))
}

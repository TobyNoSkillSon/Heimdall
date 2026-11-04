//! A request's tools and how their calls are generated and read (tool_schema.py: normalize_tools, ToolPolicy,
//! ParameterTypes and the framing of a tool's arguments that the grammars and the output projector share).
use super::pyjson::{J, Id};
use super::schema::{check_depth, lookup_reference, referenced, remote_ref, subschemas, walk};
use super::validate::check_schema;
use super::ApiError;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// A function's or parameter's name is at most this long, and reading strips this space around it.
pub const MAX_NAME_LENGTH: usize = 256;
const NAME_SPACE: &[char] = &[' ', '\t', '\n', '\r'];
pub const TAG_NAME_RULE: &str =
    "1 to 256 characters in NFC, without <, >, line breaks, lone surrogates or surrounding whitespace";

/// Framing may produce about this many bytes of schemas: for a request's strict tools together, and as much for its
/// other tools, which are framed for their parameters' types alone. Pathological schemas make framing quadratic.
pub const MAX_FRAMED_SCHEMA_BYTES: i64 = 16 * 1024 * 1024;

/// Framing and reading recurse through references and compositions at most this deep. Python's interpreter stack
/// fails such a request with RecursionError; here the request is refused before the thread's stack is.
pub const MAX_READ_DEPTH: usize = 256;

fn too_complex() -> ApiError {
    ApiError::bad_request("tool parameter schemas are too complex")
}

pub(crate) const SCHEMA_ANNOTATIONS: [&str; 8] =
    ["$comment", "title", "description", "default", "examples", "deprecated", "readOnly", "writeOnly"];
pub(crate) const SCHEMA_IDENTIFIERS: [&str; 6] = ["$schema", "$id", "$anchor", "$dynamicAnchor", "$defs", "definitions"];
const IN_PLACE_APPLICATORS: [&str; 10] =
    ["allOf", "not", "if", "then", "else", "dependentSchemas", "dependencies", "extends", "$dynamicRef", "$recursiveRef"];
const MORE_PROPERTIES: [&str; 3] = ["additionalProperties", "unevaluatedProperties", "patternProperties"];

/// Whether `name` reads back as written from a call's tags, where a function's name and a strict tool's parameter's
/// are written. A tokenizer may normalize text to NFC, as Qwen's does.
pub fn is_tag_name(name: &str) -> bool {
    let length = name.chars().count();
    0 < length
        && length <= MAX_NAME_LENGTH
        && name == name.trim_matches(NAME_SPACE)
        && !name.contains(['<', '>', '\n', '\r'])
        && is_nfc(name)
}

fn is_nfc(text: &str) -> bool {
    let mut normalized = tokenizers::NormalizedString::from(text);
    normalized.nfc();
    normalized.get() == text
}

/// How a request's tool calls are generated and read.
#[derive(Clone, Debug, Default)]
pub struct ToolPolicy {
    /// The declared parameter schema of each tool a call may name, in the request's order; None for a tool whose
    /// schema cannot be read, which only describes it to the model.
    pub schemas: Vec<(String, Option<Value>)>,
    pub required: bool,
    pub parallel: bool,
    /// The strict tools, whose arguments are generated to their schemas.
    pub strict: HashSet<String>,
    /// Whether a grammar constrains the output: under a required or named choice, with parallel_tool_calls false,
    /// beside a strict tool, and under tool_choice none, whose grammar lets no call start.
    pub constrained: bool,
    /// Responses API aliases: alias -> (namespace, name).
    pub namespaces: HashMap<String, (String, String)>,
}

impl ToolPolicy {
    pub fn schema(&self, name: &str) -> Option<&Value> {
        self.schemas.iter().find(|(n, _)| n == name).and_then(|(_, s)| s.as_ref())
    }
    pub fn offers(&self, name: &str) -> bool {
        self.schemas.iter().any(|(n, _)| n == name)
    }
    pub fn is_strict(&self, name: &str) -> bool {
        self.strict.contains(name)
    }
    /// The strict tools in request order.
    pub fn strict_names(&self) -> impl Iterator<Item = &str> {
        self.schemas.iter().map(|(n, _)| n.as_str()).filter(|n| self.strict.contains(*n))
    }
}

/// A request's tools as the chat template declares them, and its policy.
#[derive(Clone, Debug)]
pub struct NormalizedTools {
    pub tools: Vec<Value>,
    pub policy: ToolPolicy,
}

fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| !v.is_null())
}

/// The tools of a request and its ToolPolicy, or None where it offers none (normalize_tools). Absent and null fields
/// read alike, as Python's `body.get` reads them.
pub fn normalize_tools(
    tools: Option<&Value>,
    tool_choice: Option<&Value>,
    parallel: Option<&Value>,
    namespaces: HashMap<String, (String, String)>,
) -> Result<Option<NormalizedTools>, ApiError> {
    let parallel = match present(parallel) {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(ApiError::bad_request("parallel_tool_calls must be a boolean")),
    };
    let choice = present(tool_choice);
    let choice_is = |text: &str| choice.and_then(Value::as_str) == Some(text);
    let Some(tools) = present(tools) else {
        if choice.is_some() && !choice_is("none") && !choice_is("auto") {
            return Err(ApiError::bad_request("tool_choice requires tools"));
        }
        return Ok(None);
    };
    let Some(tools) = tools.as_array() else { return Err(ApiError::bad_request("tools must be an array")) };
    let mut schemas: Vec<(String, Option<Value>)> = Vec::new();
    let mut marked = HashSet::new();
    let mut offered = Vec::new();
    for tool in tools {
        let kind = tool.get("type").filter(|t| !t.is_null());
        let Some(object) = tool.as_object().filter(|_| kind.is_none() || kind == Some(&Value::from("function"))) else {
            return Err(ApiError::bad_request("only function tools are supported"));
        };
        // A tool that names no type is a function, and the template declares it as one.
        let tool = if kind.is_none() {
            let mut typed = serde_json::Map::new();
            typed.insert("type".into(), "function".into());
            typed.extend(object.iter().filter(|(k, _)| *k != "type").map(|(k, v)| (k.clone(), v.clone())));
            Value::Object(typed)
        } else {
            tool.clone()
        };
        let function = tool.get("function").and_then(Value::as_object).cloned();
        offered.push(tool);
        let Some(function) = function else {
            return Err(ApiError::bad_request("a function tool needs a function object"));
        };
        let name = match function.get("name").and_then(Value::as_str) {
            Some(name) if is_tag_name(name) => name.to_string(),
            _ => return Err(ApiError::bad_request(format!("tool name must be {TAG_NAME_RULE}"))),
        };
        if schemas.iter().any(|(n, _)| *n == name) {
            return Err(ApiError::bad_request(format!("duplicate tool name: {name}")));
        }
        let schema = match function.get("parameters") {
            None | Some(Value::Null) => Value::Object(Default::default()),
            Some(s @ (Value::Object(_) | Value::Bool(_))) => s.clone(),
            Some(_) => return Err(ApiError::bad_request(format!("invalid tool schema for {name}"))),
        };
        let strict = match function.get("strict") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(ApiError::bad_request(format!("strict must be a boolean for tool {name}"))),
        };
        let schema = if strict {
            check_tool_schema(&name, &schema)?;
            Some(schema)
        } else {
            // Only a strict tool's grammar is built from its schema. Any other's gives the types its values convert
            // by wherever it can be read; one nested too deep to read only describes the tool.
            check_depth(&J::from_value(&schema), &format!("invalid tool schema for {name}")).ok().map(|_| schema)
        };
        if strict {
            marked.insert(name.clone());
        }
        schemas.push((name, schema));
    }
    if tools.is_empty() {
        if choice.is_some() && !choice_is("auto") && !choice_is("none") {
            return Err(ApiError::bad_request("tool_choice requires at least one tool"));
        }
        return Ok(None);
    }
    if choice_is("none") {
        // The prompt keeps every tool; the grammar lets no call start.
        let policy = ToolPolicy { parallel, constrained: true, namespaces, ..Default::default() };
        return Ok(Some(NormalizedTools { tools: offered, policy }));
    }
    let mut parallel = parallel;
    let required = match choice {
        Some(Value::Object(choice)) => {
            let name = choice.get("function").and_then(Value::as_object).and_then(|f| f.get("name")).and_then(Value::as_str);
            let kind = choice.get("type").filter(|t| !t.is_null());
            let named = name.filter(|n| schemas.iter().any(|(s, _)| s == n));
            match named {
                Some(name) if kind.is_none() || kind == Some(&Value::from("function")) => {
                    // The prompt keeps every tool; the grammar forces exactly one call.
                    let name = name.to_string();
                    schemas.retain(|(s, _)| *s == name);
                    parallel = false;
                }
                _ => return Err(ApiError::bad_request("invalid named tool_choice")),
            }
            true
        }
        None => false,
        Some(_) if choice_is("auto") => false,
        Some(_) if choice_is("required") => true,
        Some(_) => return Err(ApiError::bad_request("invalid tool_choice")),
    };
    let strict: HashSet<String> = schemas.iter().map(|(n, _)| n.clone()).filter(|n| marked.contains(n)).collect();
    // A strict tool's schema compiles into its grammar, so its references must resolve.
    for (name, schema) in &schemas {
        if !strict.contains(name) {
            continue;
        }
        let root = J::from_value(schema.as_ref().expect("a strict tool's schema is kept"));
        for node in subschemas(&root) {
            if node.has("$ref") {
                lookup_reference(node.get("$ref").as_ref(), &root)?;
            }
        }
    }
    let constrained = required || !parallel || !strict.is_empty();
    let policy = ToolPolicy { schemas, required, parallel, strict, constrained, namespaces };
    Ok(Some(NormalizedTools { tools: offered, policy }))
}

/// Refuses strict tool `name`'s schema if it nests too deep, refers to another document or breaks its dialect.
fn check_tool_schema(name: &str, schema: &Value) -> Result<(), ApiError> {
    let tree = J::from_value(schema);
    check_depth(&tree, &format!("invalid tool schema for {name}"))?;
    if let Some(reference) = remote_ref(&tree)? {
        return Err(ApiError::bad_request(format!("remote tool schema reference is not allowed: {reference}")));
    }
    check_schema(schema).map_err(|message| ApiError::bad_request(format!("invalid tool schema for {name}: {message}")))
}

// ---- JSON types ----------------------------------------------------------------------------------------------

/// A set of JSON types, by which a parameter's text converts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct JsonTypes(u8);

pub const JSON_TYPE_NAMES: [&str; 7] = ["null", "boolean", "integer", "number", "string", "array", "object"];

impl JsonTypes {
    pub const EMPTY: JsonTypes = JsonTypes(0);
    fn bit(name: &str) -> u8 {
        JSON_TYPE_NAMES.iter().position(|n| *n == name).map_or(0, |i| 1 << i)
    }
    pub fn of(names: &[&str]) -> JsonTypes {
        JsonTypes(names.iter().fold(0, |acc, n| acc | Self::bit(n)))
    }
    pub fn contains(self, name: &str) -> bool {
        let bit = Self::bit(name);
        bit != 0 && self.0 & bit != 0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn names(self) -> Vec<&'static str> {
        JSON_TYPE_NAMES.iter().enumerate().filter(|(i, _)| self.0 & (1 << i) != 0).map(|(_, n)| *n).collect()
    }
    fn union(self, other: JsonTypes) -> JsonTypes {
        JsonTypes(self.0 | other.0)
    }
}

/// The JSON type a schema's type name stands for, whatever its case and surrounding space, by name or common alias.
fn type_name(kind: &str) -> Option<&'static str> {
    let key = kind.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)).to_lowercase();
    if let Some(name) = JSON_TYPE_NAMES.iter().find(|n| **n == key) {
        return Some(name);
    }
    Some(match key.as_str() {
        "str" | "text" => "string",
        "int" => "integer",
        "float" => "number",
        "bool" => "boolean",
        "dict" => "object",
        "list" => "array",
        _ => return None,
    })
}

fn spelled_types(kind: &J) -> J {
    match kind {
        J::Arr(items) => J::arr(items.borrow().iter().map(|item| if item.as_str().is_some() { spelled_types(item) } else { item.clone() }).collect()),
        J::Str(text) => type_name(text).map_or_else(|| kind.clone(), J::str),
        _ => kind.clone(),
    }
}

/// `schema` with its types spelled as the JSON types they stand for: a copy where any is spelled otherwise, else
/// `schema` itself.
pub(crate) fn json_type_names(schema: &J) -> J {
    let spelled = subschemas(schema).iter().all(|node| match node.get("type") {
        Some(kind) if node.as_obj().is_some() => spelled_types(&kind).py_eq(&kind),
        _ => true,
    });
    if spelled {
        return schema.clone();
    }
    let output = schema.deep_copy();
    walk(&output, &mut |node| {
        if let (Some(object), Some(kind)) = (node.as_obj(), node.get("type")) {
            object.borrow_mut().set("type", spelled_types(&kind));
        }
        true
    });
    output
}

fn json_type(name: &J) -> Option<&'static str> {
    name.as_str().and_then(|n| JSON_TYPE_NAMES.iter().find(|t| **t == n).copied())
}

fn narrowed(types: Option<JsonTypes>, other: Option<JsonTypes>) -> Option<JsonTypes> {
    let (Some(types), Some(other)) = (types, other) else { return if types.is_none() { other } else { types } };
    let mut both = JsonTypes(types.0 & other.0);
    if (types.contains("number") && other.contains("integer")) || (types.contains("integer") && other.contains("number")) {
        both = both.union(JsonTypes::of(&["integer"]));
    }
    Some(both)
}

pub(crate) fn enumeration(node: &J) -> Option<Vec<J>> {
    let object = node.as_obj()?.borrow();
    if let Some(value) = object.get("const") {
        return Some(vec![value.clone()]);
    }
    object.get("enum").and_then(|v| v.as_arr().map(|a| a.borrow().clone()))
}

/// Reads the JSON types values of schemas within `root` may take (tool_schema._type_reader), each node once.
pub(crate) struct TypeReader {
    root: J,
    types: HashMap<Id, Option<JsonTypes>>,
    depth: usize,
    /// Set when a read went past MAX_READ_DEPTH; its results are then not to be used.
    pub too_deep: bool,
}

impl TypeReader {
    pub fn new(root: &J) -> Self {
        TypeReader { root: root.clone(), types: HashMap::new(), depth: 0, too_deep: false }
    }

    pub fn read(&mut self, node: &J) -> Option<JsonTypes> {
        if node.as_obj().is_none() {
            return if node.is_bool(false) { Some(JsonTypes::EMPTY) } else { None };
        }
        if let Some(known) = self.types.get(&node.id()) {
            return *known;
        }
        if self.depth >= MAX_READ_DEPTH {
            self.too_deep = true;
            return None;
        }
        self.types.insert(node.id(), None);
        self.depth += 1;
        let result = self.own(node);
        self.depth -= 1;
        self.types.insert(node.id(), result);
        result
    }

    fn own(&mut self, node: &J) -> Option<JsonTypes> {
        let kind = node.get("type");
        let values = enumeration(node);
        let members = node.get("anyOf").or_else(|| node.get("oneOf"));
        let mut result = match (&kind, &values, members.as_ref().and_then(J::as_arr)) {
            (Some(kind @ (J::Str(_) | J::Arr(_))), _, _) => {
                let kinds = match kind {
                    J::Arr(items) => items.borrow().clone(),
                    _ => vec![kind.clone()],
                };
                let named: Vec<Option<&str>> = kinds.iter().map(json_type).collect();
                if named.contains(&None) { None } else { Some(JsonTypes::of(&named.into_iter().flatten().collect::<Vec<_>>())) }
            }
            (_, Some(values), _) => Some(JsonTypes::of(&values.iter().map(J::json_type).collect::<Vec<_>>())),
            (_, _, Some(members)) if !members.borrow().is_empty() => {
                let members = members.borrow().clone();
                let union: Vec<JsonTypes> = members.iter().filter_map(|m| self.read(m)).collect();
                if union.is_empty() { None } else { Some(union.into_iter().fold(JsonTypes::EMPTY, JsonTypes::union)) }
            }
            _ => None,
        };
        let mut narrowing = node.get("allOf").and_then(|a| a.as_arr().map(|a| a.borrow().clone())).unwrap_or_default();
        if node.has("$ref") {
            if let Some(target) = referenced(node, &self.root) {
                narrowing.push(target);
            }
        }
        for member in narrowing {
            let member_types = self.read(&member);
            result = narrowed(result, member_types);
        }
        result
    }
}

/// The JSON types a value of `schema` may take; None where it constrains no type (tool_schema.schema_types).
pub(crate) fn schema_types(schema: &J, root: &J) -> Result<Option<JsonTypes>, ApiError> {
    let mut reader = TypeReader::new(root);
    let types = reader.read(schema);
    if reader.too_deep { Err(too_complex()) } else { Ok(types) }
}

/// The values a parameter of `schema` takes when the schema lists them all, as a call's value tags write each: a
/// string as its text, anything else as JSON; None when it does not list them (tool_schema._literal_texts).
pub(crate) fn literal_texts(schema: &J, root: &J) -> Result<Option<Vec<String>>, ApiError> {
    struct Reader<'a> {
        root: &'a J,
        texts: HashMap<Id, Option<Vec<String>>>,
        depth: usize,
        too_deep: bool,
    }
    fn unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut seen = HashSet::new();
        items.into_iter().filter(|t| seen.insert(t.clone())).collect()
    }
    impl Reader<'_> {
        fn read(&mut self, node: &J) -> Option<Vec<String>> {
            if let Some(known) = self.texts.get(&node.id()) {
                return known.clone();
            }
            if self.depth >= MAX_READ_DEPTH {
                self.too_deep = true;
                return None;
            }
            self.texts.insert(node.id(), None);
            node.as_obj()?;
            self.depth += 1;
            let result = self.own(node);
            self.depth -= 1;
            self.texts.insert(node.id(), result.clone());
            result
        }

        fn own(&mut self, node: &J) -> Option<Vec<String>> {
            let values = enumeration(node);
            let kind = match node.get("type") {
                Some(J::Arr(items)) if items.borrow().len() == 1 => Some(items.borrow()[0].clone()),
                other => other,
            };
            let kind = kind.as_ref().and_then(J::as_str);
            let members = node.get("anyOf").or_else(|| node.get("oneOf"));
            let members = members.as_ref().and_then(J::as_arr).map(|m| m.borrow().clone()).filter(|m| !m.is_empty());
            let mut result = if let Some(values) = values {
                Some(unique(values.iter().map(|v| match v.as_str() {
                    Some(text) => text.to_string(),
                    None => super::pyjson::dumps(v, true),
                })))
            } else if kind == Some("null") {
                Some(vec!["null".to_string()])
            } else if kind == Some("boolean") {
                Some(vec!["true".to_string(), "false".to_string()])
            } else if let Some(members) = members {
                let mut all = Vec::new();
                let mut listed = true;
                for member in &members {
                    match self.read(member) {
                        Some(texts) => all.extend(texts),
                        None => {
                            listed = false;
                            break;
                        }
                    }
                }
                listed.then(|| unique(all))
            } else if let Some(target) = node.has("$ref").then(|| referenced(node, self.root)).flatten() {
                self.read(&target)
            } else {
                None
            };
            let narrowing = node.get("allOf").and_then(|a| a.as_arr().map(|a| a.borrow().clone())).unwrap_or_default();
            for member in narrowing {
                if let Some(member_texts) = self.read(&member) {
                    result = Some(match result {
                        None => member_texts,
                        Some(texts) => texts.into_iter().filter(|t| member_texts.contains(t)).collect(),
                    });
                }
            }
            result
        }
    }
    let mut reader = Reader { root, texts: HashMap::new(), depth: 0, too_deep: false };
    let texts = reader.read(schema);
    if reader.too_deep { Err(too_complex()) } else { Ok(texts) }
}

// ---- framing --------------------------------------------------------------------------------------------------

fn extended(node: &J) -> bool {
    let Some(object) = node.as_obj() else { return false };
    let object = object.borrow();
    let keys: BTreeSet<&str> =
        object.keys().filter(|k| !SCHEMA_ANNOTATIONS.contains(k) && !SCHEMA_IDENTIFIERS.contains(k)).collect();
    if keys.iter().any(|k| IN_PLACE_APPLICATORS.contains(k)) || (keys.contains("$ref") && keys.len() != 1) {
        return true;
    }
    let unions = keys.iter().filter(|k| **k == "anyOf" || **k == "oneOf").count();
    unions > 0 && keys.iter().any(|k| !["anyOf", "oneOf", "type"].contains(k))
}

/// The schema a strict tool's arguments are generated to, as XGrammar's strict mode generates them: objects and
/// arrays that do not say what they take beyond what they declare take nothing more. A schema other schemas of the
/// same instance extend, as an allOf does, is left as declared.
pub(crate) fn strict_schema(schema: &J) -> J {
    if subschemas(schema).iter().any(extended) {
        return schema.clone();
    }
    let output = schema.deep_copy();
    walk(&output, &mut |node| {
        let Some(object) = node.as_obj() else { return true };
        let keys: HashSet<String> = object.borrow().keys().map(str::to_string).collect();
        if ["anyOf", "oneOf", "$ref"].iter().any(|k| keys.contains(*k)) {
            return true;
        }
        let kind = object.borrow().get("type").cloned();
        let kinds = match &kind {
            Some(J::Arr(items)) => items.borrow().clone(),
            Some(other) => vec![other.clone()],
            None => vec![J::Null],
        };
        let names = |name: &str| kinds.iter().any(|k| k.as_str() == Some(name));
        let untyped = matches!(kind, None | Some(J::Null));
        if (names("object") || (untyped && keys.contains("properties"))) && !MORE_PROPERTIES.iter().any(|k| keys.contains(*k)) {
            object.borrow_mut().set("additionalProperties", J::Bool(false));
        }
        if names("array") || (untyped && keys.contains("prefixItems")) {
            let items_list = object.borrow().get("items").is_some_and(|i| i.as_arr().is_some());
            if items_list {
                if !keys.contains("additionalItems") && !keys.contains("unevaluatedItems") {
                    object.borrow_mut().set("additionalItems", J::Bool(false));
                }
            } else if !keys.contains("items") && !keys.contains("unevaluatedItems") {
                object.borrow_mut().set("items", J::Bool(false));
            }
        }
        true
    });
    output
}

fn definitions(schema: &J) -> Vec<String> {
    schema.get("$defs").and_then(|d| d.as_obj().map(|o| o.borrow().keys().map(str::to_string).collect())).unwrap_or_default()
}

/// `schema` made self-contained where it refers locally to `root`: root copied under a fresh `$defs` name and the
/// references rebased onto it. Returns whether anything changed (Python's `is not`).
pub(crate) fn schema_with_root(schema: &J, root: &J) -> (J, bool) {
    fn local_refs(value: &J) -> Vec<J> {
        subschemas(value)
            .into_iter()
            .filter(|n| n.as_obj().is_some() && n.get("$ref").is_some_and(|r| r.as_str().is_some_and(|r| r.starts_with('#'))))
            .collect()
    }
    if local_refs(schema).is_empty() {
        return (schema.clone(), false);
    }
    let (root_defs, schema_defs) = (definitions(root), definitions(schema));
    let mut name = "__splash_root".to_string();
    while root_defs.contains(&name) || schema_defs.contains(&name) {
        name.push('_');
    }
    let prefix = format!("#/$defs/{name}");
    let rebase = |value: &J| {
        let output = value.deep_copy();
        for node in local_refs(&output) {
            let reference = node.get("$ref").and_then(|r| r.as_str().map(str::to_string)).unwrap_or_default();
            if reference == "#" || reference.starts_with("#/") {
                node.as_obj().unwrap().borrow_mut().set("$ref", J::str(&format!("{prefix}{}", &reference[1..])));
            }
        }
        output
    };
    let output = rebase(schema);
    let mut defs: Vec<(String, J)> = output.get("$defs").and_then(|d| d.as_obj().map(|o| o.borrow().0.clone())).unwrap_or_default();
    let rebased_root = rebase(root);
    match defs.iter_mut().find(|(k, _)| *k == name) {
        Some(entry) => entry.1 = rebased_root,
        None => defs.push((name, rebased_root)),
    }
    output.as_obj().expect("a schema with references is an object").borrow_mut().set("$defs", J::obj(defs));
    (output, true)
}

/// `allOf`/`anyOf` of `values`, each reached once, with the keyword's identity dropped and its absorbing bool winning.
fn schema_combination(keyword: &str, values: &[J]) -> J {
    let identity = keyword == "allOf";
    let mut seen = HashSet::new();
    let values: Vec<J> = values.iter().filter(|v| !v.is_bool(identity) && seen.insert(v.id())).cloned().collect();
    if values.is_empty() {
        return J::Bool(identity);
    }
    if values.iter().any(|v| matches!(v, J::Bool(_))) {
        return J::Bool(!identity);
    }
    if values.len() == 1 {
        return values[0].clone();
    }
    J::obj(vec![(keyword.to_string(), J::arr(values))])
}

/// Approximate serialized size of `value`, counted no further than `limit`.
fn json_size(value: &J, limit: i64) -> i64 {
    let mut size = 0i64;
    let mut pending = vec![value.clone()];
    while size <= limit {
        let Some(value) = pending.pop() else { break };
        size += match &value {
            J::Obj(o) => {
                let o = o.borrow();
                pending.extend(o.0.iter().map(|(k, _)| J::str(k)));
                pending.extend(o.0.iter().map(|(_, v)| v.clone()));
                1
            }
            J::Arr(a) => {
                pending.extend(a.borrow().iter().cloned());
                1
            }
            J::Str(s) => s.chars().count() as i64,
            _ => 1,
        };
    }
    size
}

fn charge(budget: &mut i64, size: i64) -> Result<(), ApiError> {
    *budget -= size;
    if *budget < 0 {
        return Err(ApiError::bad_request("tool parameter schemas are too complex"));
    }
    Ok(())
}

/// A tool's object fields projected for framing: its properties, required names and additionalProperties.
#[derive(Clone, Debug)]
pub(crate) struct Shape {
    pub properties: Vec<(String, J)>,
    pub required: Vec<String>,
    pub additional: J,
}

impl Shape {
    fn property(&self, name: &str) -> J {
        self.properties.iter().find(|(n, _)| n == name).map_or_else(|| self.additional.clone(), |(_, v)| v.clone())
    }
}

/// Projects a tool's object fields for framing (tool_schema._argument_shape). Every value a field may take is kept
/// rather than choosing a branch before the model has written a discriminator, so cross-field assertions go
/// unenforced. A keyword holding the wrong kind of value constrains nothing, as one its dialect does not define.
pub(crate) fn argument_shape(root: &J, budget: &mut i64) -> Result<Shape, ApiError> {
    struct Projector<'a> {
        root: &'a J,
        budget: &'a mut i64,
        references: HashMap<String, Option<Shape>>,
        /// The references on the path being projected.
        visiting: Vec<String>,
        depth: usize,
    }
    impl Projector<'_> {
        fn combine(&mut self, shapes: Vec<Option<Shape>>, union: bool) -> Result<Option<Shape>, ApiError> {
            let shapes: Vec<Shape> = if union {
                let kept: Vec<Shape> = shapes.into_iter().flatten().collect();
                if kept.is_empty() {
                    return Ok(None);
                }
                kept
            } else {
                match shapes.into_iter().collect::<Option<Vec<Shape>>>() {
                    Some(shapes) => shapes,
                    None => return Ok(None),
                }
            };
            if shapes.is_empty() {
                return Ok(Some(Shape { properties: Vec::new(), required: Vec::new(), additional: J::Bool(true) }));
            }
            let mut names: Vec<String> = Vec::new();
            let mut seen = HashSet::new();
            for shape in &shapes {
                for (name, _) in &shape.properties {
                    if seen.insert(name.clone()) {
                        names.push(name.clone());
                    }
                }
            }
            charge(self.budget, (names.len() * shapes.len()) as i64)?;
            let mut required: BTreeSet<String> = shapes[0].required.iter().cloned().collect();
            for shape in &shapes[1..] {
                let other: BTreeSet<String> = shape.required.iter().cloned().collect();
                required = if union { required.intersection(&other).cloned().collect() } else { required.union(&other).cloned().collect() };
            }
            let keyword = if union { "anyOf" } else { "allOf" };
            let properties = names
                .iter()
                .map(|name| {
                    let values: Vec<J> = shapes.iter().map(|s| s.property(name)).collect();
                    (name.clone(), schema_combination(keyword, &values))
                })
                .collect();
            let additional: Vec<J> = shapes.iter().map(|s| s.additional.clone()).collect();
            Ok(Some(Shape { properties, required: required.into_iter().collect(), additional: schema_combination(keyword, &additional) }))
        }

        fn project(&mut self, node: &J) -> Result<Option<Shape>, ApiError> {
            if self.depth >= MAX_READ_DEPTH {
                return Err(too_complex());
            }
            self.depth += 1;
            let shape = self.project_node(node);
            self.depth -= 1;
            shape
        }

        fn project_node(&mut self, node: &J) -> Result<Option<Shape>, ApiError> {
            if node.is_bool(false) {
                return Ok(None);
            }
            // True constrains nothing, as does a value that is not a schema.
            let node = if node.as_obj().is_some() { node.clone() } else { J::empty_obj() };
            let names: Vec<J> = match node.get("type") {
                Some(J::Arr(items)) => items.borrow().clone(),
                Some(kind @ J::Str(_)) => vec![kind],
                _ => vec![J::str("object")],
            };
            // A type allows no object only where it names JSON types alone, none of them object.
            if names.iter().all(|n| !matches!(json_type(n), None | Some("object"))) {
                return Ok(None);
            }
            let mut properties: Vec<(String, J)> =
                node.get("properties").and_then(|p| p.as_obj().map(|o| o.borrow().0.clone())).unwrap_or_default();
            let mut additional = node.get("additionalProperties").unwrap_or(J::Bool(true));
            let patterns: Vec<J> = node
                .get("patternProperties")
                .and_then(|p| p.as_obj().map(|o| o.borrow().0.iter().map(|(_, v)| v.clone()).collect()))
                .unwrap_or_default();
            if !patterns.is_empty() {
                let mut members = vec![additional];
                members.extend(patterns);
                additional = schema_combination("anyOf", &members);
            }
            let required: Vec<String> = node
                .get("required")
                .and_then(|r| r.as_arr().map(|a| a.borrow().iter().filter_map(|n| n.as_str().map(str::to_string)).collect()))
                .unwrap_or_default();
            for name in &required {
                if !properties.iter().any(|(n, _)| n == name) {
                    properties.push((name.clone(), additional.clone()));
                }
            }
            let mut shapes = vec![Some(Shape { properties, required, additional })];
            if let Some(reference) = node.get("$ref").and_then(|r| r.as_str().map(str::to_string)) {
                if self.visiting.contains(&reference) {
                    return Err(ApiError::bad_request("cyclic direct tool argument reference"));
                }
                if !self.references.contains_key(&reference) {
                    let target = referenced(&node, self.root).unwrap_or(J::Null);
                    self.visiting.push(reference.clone());
                    let projected = self.project(&target);
                    self.visiting.pop();
                    let projected = projected?;
                    self.references.insert(reference.clone(), projected);
                }
                shapes.push(self.references[&reference].clone());
            }
            let list = |key: &str| node.get(key).and_then(|v| v.as_arr().map(|a| a.borrow().clone()));
            for child in list("allOf").unwrap_or_default() {
                shapes.push(self.project(&child)?);
            }
            for keyword in ["anyOf", "oneOf"] {
                if let Some(children) = list(keyword) {
                    let projected = children.iter().map(|c| self.project(c)).collect::<Result<Vec<_>, _>>()?;
                    shapes.push(self.combine(projected, true)?);
                }
            }
            if node.has("if") {
                let then = node.get("then").unwrap_or_else(J::empty_obj);
                let otherwise = node.get("else").unwrap_or_else(J::empty_obj);
                let branches = vec![self.project(&then)?, self.project(&otherwise)?];
                shapes.push(self.combine(branches, true)?);
            }
            let dependent: Vec<J> = node
                .get("dependentSchemas")
                .and_then(|d| d.as_obj().map(|o| o.borrow().0.iter().map(|(_, v)| v.clone()).collect()))
                .unwrap_or_default();
            for child in dependent {
                let branches = vec![self.project(&child)?, self.project(&J::empty_obj())?];
                shapes.push(self.combine(branches, true)?);
            }
            if let Some(choices) = enumeration(&node) {
                let objects: Vec<J> = choices.into_iter().filter(|v| v.as_obj().is_some()).collect();
                if objects.is_empty() {
                    return Ok(None);
                }
                let listed = objects
                    .iter()
                    .map(|object| {
                        let entries = object.as_obj().unwrap().borrow().0.clone();
                        Some(Shape {
                            properties: entries.iter().map(|(n, v)| (n.clone(), J::obj(vec![("const".into(), v.clone())]))).collect(),
                            required: entries.iter().map(|(n, _)| n.clone()).collect(),
                            additional: J::Bool(false),
                        })
                    })
                    .collect();
                shapes.push(self.combine(listed, true)?);
            }
            self.combine(shapes, false)
        }
    }
    let mut projector = Projector { root, budget, references: HashMap::new(), visiting: Vec::new(), depth: 0 };
    projector
        .project(root)?
        .ok_or_else(|| ApiError::bad_request("tool parameters must allow a top-level JSON object"))
}

/// A strict tool's arguments as its grammar generates them: the projection with each value a schema of its own, a
/// copy of the root schema where it refers to the root (tool_schema.tool_argument_schema).
pub(crate) fn tool_argument_schema(root: &J, budget: &mut i64) -> Result<Shape, ApiError> {
    let framed = |value: &J, budget: &mut i64| -> Result<J, ApiError> {
        // Shared definitions repeat in the serialized grammar; count them all.
        charge(budget, json_size(value, *budget))?;
        let (framed, changed) = schema_with_root(value, root);
        if changed {
            charge(budget, json_size(root, *budget))?;
        }
        Ok(framed)
    };
    let shape = argument_shape(root, budget)?;
    let mut properties = Vec::with_capacity(shape.properties.len());
    for (name, value) in &shape.properties {
        properties.push((name.clone(), framed(value, budget)?));
    }
    let additional = framed(&shape.additional, budget)?;
    Ok(Shape { properties, required: shape.required, additional })
}

/// The framed arguments of each strict tool of `policy`, in request order, under one budget.
pub(crate) fn argument_schemas(policy: &ToolPolicy) -> Result<Vec<(String, Shape)>, ApiError> {
    let mut budget = MAX_FRAMED_SCHEMA_BYTES;
    policy
        .strict_names()
        .map(|name| {
            let schema = J::from_value(policy.schema(name).expect("a strict tool's schema is kept"));
            Ok((name.to_string(), tool_argument_schema(&strict_schema(&json_type_names(&schema)), &mut budget)?))
        })
        .collect()
}

// ---- parameter types ------------------------------------------------------------------------------------------

/// The JSON types a tool's parameters convert by: those of each declared parameter, and those of any other; None
/// where a schema constrains no type.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolTypes {
    pub parameters: HashMap<String, Option<JsonTypes>>,
    pub other: Option<JsonTypes>,
}

impl ToolTypes {
    /// The types of parameter `name`, declared or not.
    pub fn of(&self, name: &str) -> Option<JsonTypes> {
        self.parameters.get(name).copied().unwrap_or(self.other)
    }
}

/// The types by which the values of a request's tool calls convert, read as its output's calls name tools
/// (tool_schema.ParameterTypes). A strict tool's are those of the arguments its grammar generates; any other's are
/// projected from its schema within what the budget of the request's other tools has left; one that cannot be read
/// or projected declares no types. One per request, owned by its output projector.
pub struct ParameterTypes {
    policy: Arc<ToolPolicy>,
    types: HashMap<String, Arc<ToolTypes>>,
    budget: i64,
}

impl ParameterTypes {
    pub fn new(policy: Arc<ToolPolicy>) -> Self {
        ParameterTypes { policy, types: HashMap::new(), budget: MAX_FRAMED_SCHEMA_BYTES }
    }

    /// The types of tool `name`'s parameters, and those of any other.
    pub fn of(&mut self, name: &str) -> Arc<ToolTypes> {
        if let Some(known) = self.types.get(name) {
            return known.clone();
        }
        let read = Arc::new(self.read(name));
        self.types.insert(name.to_string(), read.clone());
        read
    }

    fn read(&mut self, name: &str) -> ToolTypes {
        let policy = &self.policy;
        if policy.is_strict(name) {
            let schema = J::from_value(policy.schema(name).expect("a strict tool's schema is kept"));
            let mut budget = MAX_FRAMED_SCHEMA_BYTES;
            // A strict tool's grammar framed it already; the request failed if framing did.
            let Ok(shape) = tool_argument_schema(&strict_schema(&json_type_names(&schema)), &mut budget) else {
                return ToolTypes::default();
            };
            // A framed value is a schema of its own.
            let read = |value: &J| schema_types(value, value);
            let parameters: Result<HashMap<_, _>, ApiError> = shape.properties.iter().map(|(n, v)| Ok((n.clone(), read(v)?))).collect();
            return match (parameters, read(&shape.additional)) {
                (Ok(parameters), Ok(other)) => ToolTypes { parameters, other },
                _ => ToolTypes::default(),
            };
        }
        let Some(schema) = policy.schema(name) else { return ToolTypes::default() };
        let schema = json_type_names(&J::from_value(schema));
        let Ok(shape) = argument_shape(&schema, &mut self.budget) else { return ToolTypes::default() };
        let mut reader = TypeReader::new(&schema);
        let parameters = shape.properties.iter().map(|(n, v)| (n.clone(), reader.read(v))).collect();
        let other = reader.read(&shape.additional);
        // Python's projector would fail on such a schema; its values convert as untyped instead.
        if reader.too_deep {
            return ToolTypes::default();
        }
        ToolTypes { parameters, other }
    }
}

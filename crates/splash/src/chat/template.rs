//! The model's chat template (server/chat_templates.py over transformers' `apply_chat_template`), rendered with
//! minijinja so prompts come out byte-identical to the Python server's.
//!
//! transformers renders with Jinja2 (`trim_blocks`, `lstrip_blocks`, `loopcontrols`), its own `tojson`
//! (`json.dumps(x, ensure_ascii=False)`), `raise_exception` and `strftime_now`, and the tokenizer's special tokens
//! as variables. minijinja is configured the same way, with Python's spellings where Jinja2 prints Python objects:
//! `{{ x }}` and `|string` print `True`, `None`, `1e+16` and `{'a': 1}`; `trim`, `strip()` and `split()` take
//! Python's whitespace; `{% generation %}` (transformers' assistant-mask tag) renders its body.
//!
//! Startup probing is chat_templates.py's, rendered the same way: the later-system canary with its structural patch
//! (`later_system.rs`), and the generation prompt for every reasoning effort, other options probed on first use.

use super::later_system;
use super::pyjson::{self, Dumps};
use minijinja::syntax::SyntaxConfig;
use minijinja::value::{Enumerator, Kwargs, Object, ObjectRepr, Rest, ValueKind};
use minijinja::{AutoEscape, Environment, Error, ErrorKind, State, Value as JinjaValue};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The reasoning efforts a request may name (serve_options.REASONING_EFFORTS).
pub const REASONING_EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Template arguments request preparation sets itself, and apply_chat_template's own: a request's
/// chat_template_kwargs cannot set them.
pub const RESERVED_TEMPLATE_KWARGS: [&str; 9] = [
    "add_generation_prompt",
    "chat_template",
    "continue_final_message",
    "conversation",
    "documents",
    "messages",
    "return_dict",
    "tokenize",
    "tools",
];

/// apply_chat_template parameters that never reach the template; a request's kwargs naming them are consumed there.
const APPLY_CONTROLS: [&str; 6] =
    ["padding", "truncation", "max_length", "return_tensors", "return_assistant_tokens_mask", "tokenizer_kwargs"];

/// What a template that rejects one of the efforts renders instead (REASONING_EFFORT_ALIASES).
fn effort_alias(effort: &str) -> Option<&'static str> {
    match effort {
        "high" | "max" => Some("xhigh"),
        "minimal" => Some("low"),
        _ => None,
    }
}

pub const LATER_SYSTEM_UNSUPPORTED: &str =
    "this model's chat template does not accept system messages after the first message";

/// Why a render failed, in Python's classes: `Raised` is the template's own `raise_exception` and `Template` another
/// Jinja2 TemplateError (syntax, undefined, unknown names), both reported to the client with their reason; `Internal`
/// is what Python raises as TypeError or ValueError (bad operands or arguments), reported without one. minijinja's
/// wording of a `Template` reason differs from Jinja2's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("{0}")]
    Raised(String),
    #[error("{0}")]
    Template(String),
    #[error("{0}")]
    Internal(String),
}

impl RenderError {
    /// Whether Python would raise a jinja2 TemplateError, which the alias retry and the client's reason follow.
    pub fn is_template_error(&self) -> bool {
        !matches!(self, RenderError::Internal(_))
    }

    /// The 400 message the client reads (frontend._render_prompt): a template error's reason, such as a system
    /// message the template refuses, and none for other errors, which may carry internals.
    pub fn client_message(&self) -> String {
        match self {
            RenderError::Raised(reason) | RenderError::Template(reason) => format!("messages could not be rendered: {reason}"),
            RenderError::Internal(_) => "messages could not be rendered".to_owned(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChatTemplateError {
    #[error("the tokenizer defines no chat template")]
    Missing,
    #[error("the tokenizer's named chat templates have no default")]
    NoDefault,
    #[error("chat template: {0}")]
    Io(String),
}

/// What Splash does with a system message after the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaterSystem {
    Native,
    Patched,
    Unsupported,
}

/// What the unmodified template does with the canary's later system message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Original {
    Renders,
    Rejects,
    Drops,
    Misplaces,
}

impl LaterSystem {
    pub fn as_str(self) -> &'static str {
        match self {
            LaterSystem::Native => "native",
            LaterSystem::Patched => "patched",
            LaterSystem::Unsupported => "unsupported",
        }
    }
}

impl Original {
    pub fn as_str(self) -> &'static str {
        match self {
            Original::Renders => "renders",
            Original::Rejects => "rejects",
            Original::Drops => "drops",
            Original::Misplaces => "misplaces",
        }
    }
}

/// Template variables exactly as request preparation passes them before the request's other chat_template_kwargs
/// (chat_templates.template_options): thinking follows enable_thinking, else the effort; both switches are set.
pub fn template_options(
    reasoning_effort: Option<&str>,
    preserve_thinking: Option<bool>,
    tools: Option<&[Value]>,
    add_generation_prompt: bool,
    enable_thinking: Option<bool>,
) -> Map<String, Value> {
    let mut options = Map::new();
    options.insert("add_generation_prompt".into(), add_generation_prompt.into());
    let enable_thinking = enable_thinking.or(reasoning_effort.map(|effort| effort != "none"));
    if let Some(enable) = enable_thinking {
        options.insert("enable_thinking".into(), enable.into());
        if !enable {
            options.insert("reasoning_effort".into(), "none".into());
        } else if let Some(effort) = reasoning_effort.filter(|e| *e != "none") {
            options.insert("reasoning_effort".into(), effort.into());
        }
    }
    if let Some(preserve) = preserve_thinking {
        options.insert("preserve_thinking".into(), preserve.into());
    }
    if let Some(tools) = tools.filter(|t| !t.is_empty()) {
        options.insert("tools".into(), Value::Array(tools.to_vec()));
    }
    options
}

/// A template's sources as the tokenizer directory defines them: one, or named ones with a default.
#[derive(Clone, Debug)]
pub struct TemplateFiles {
    /// `None` names the single template.
    pub templates: Vec<(Option<String>, String)>,
    /// The tokenizer's special tokens by role (`bos_token`, `eos_token`, ...), which templates read as variables.
    pub special_tokens: Map<String, Value>,
}

const SPECIAL_TOKEN_KEYS: [&str; 7] =
    ["bos_token", "eos_token", "unk_token", "sep_token", "pad_token", "cls_token", "mask_token"];

impl TemplateFiles {
    /// Read `chat_template.jinja` (with `additional_chat_templates/*.jinja` as named ones) or tokenizer_config.json's
    /// `chat_template`, and the config's special tokens, from a prepared model's tokenizer directory.
    pub fn read(dir: &Path) -> Result<Self, ChatTemplateError> {
        let io = |e: std::io::Error| ChatTemplateError::Io(e.to_string());
        let config: Value = match std::fs::read_to_string(dir.join("tokenizer_config.json")) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| ChatTemplateError::Io(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
            Err(e) => return Err(io(e)),
        };
        let mut special_tokens = Map::new();
        for key in SPECIAL_TOKEN_KEYS {
            let token = match config.get(key) {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Object(o)) => o.get("content").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            };
            if let Some(token) = token {
                special_tokens.insert(key.into(), token.into());
            }
        }
        // Template files replace the config's templates: chat_template.jinja is the default, each
        // additional_chat_templates/<name>.jinja a named one (overriding a same-named default), and a lone default is
        // the single template.
        let mut files: Vec<(String, String)> = Vec::new();
        match std::fs::read_to_string(dir.join("chat_template.jinja")) {
            Ok(text) => files.push(("default".into(), text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        if let Ok(entries) = std::fs::read_dir(dir.join("additional_chat_templates")) {
            let mut paths: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
            paths.sort();
            for path in paths.into_iter().filter(|p| p.extension().is_some_and(|e| e == "jinja")) {
                let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                let text = std::fs::read_to_string(&path).map_err(io)?;
                match files.iter_mut().find(|(n, _)| *n == name) {
                    Some(entry) => entry.1 = text,
                    None => files.push((name, text)),
                }
            }
        }
        let templates = match (files.as_slice(), config.get("chat_template")) {
            ([(name, source)], _) if name == "default" => vec![(None, source.clone())],
            ([_, ..], _) => files.into_iter().map(|(n, s)| (Some(n), s)).collect(),
            ([], Some(Value::String(source))) => vec![(None, source.clone())],
            ([], Some(Value::Array(list))) => list
                .iter()
                .filter_map(|t| Some((Some(t.get("name")?.as_str()?.to_owned()), t.get("template")?.as_str()?.to_owned())))
                .collect(),
            _ => Vec::new(),
        };
        Ok(TemplateFiles { templates, special_tokens })
    }
}

/// Encodes text as the prompt tokenizer does (for the generation prompt's tokens).
pub type Encoder = Arc<dyn Fn(&str) -> Vec<u32> + Send + Sync>;

/// The generation prompt's text and tokens: what `add_generation_prompt` appends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationPrompt {
    pub text: String,
    pub tokens: Vec<u32>,
}

/// One compiled template source with the environment it renders in.
pub(crate) struct Compiled {
    env: Environment<'static>,
    special_tokens: Map<String, Value>,
}

impl Compiled {
    pub(crate) fn new(source: &str, special_tokens: &Map<String, Value>) -> Result<Self, RenderError> {
        let mut env = environment();
        env.add_template_owned("chat", prepare_source(source))
            .map_err(|e| render_error(&e))?;
        Ok(Compiled { env, special_tokens: special_tokens.clone() })
    }

    /// One render, as `apply_chat_template(messages, **options, tokenize=False)`.
    pub(crate) fn render_once(&self, messages: &Value, options: &Map<String, Value>) -> Result<String, RenderError> {
        // apply_chat_template refuses an assistant mask without tokenizing, before it renders (a ValueError).
        if options.get("return_assistant_tokens_mask").is_some_and(pyjson::truthy) {
            return Err(RenderError::Internal(
                "`return_assistant_tokens_mask=True` requires `return_dict=True` and `tokenize=True`".into(),
            ));
        }
        let mut context = self.special_tokens.clone();
        let mut add_generation_prompt = Value::Bool(false);
        let mut tools = Value::Null;
        for (key, value) in options {
            match key.as_str() {
                "add_generation_prompt" => add_generation_prompt = value.clone(),
                "tools" => tools = value.clone(),
                k if APPLY_CONTROLS.contains(&k) || RESERVED_TEMPLATE_KWARGS.contains(&k) => {}
                _ => {
                    context.insert(key.clone(), value.clone());
                }
            }
        }
        context.insert("messages".into(), messages.clone());
        context.insert("tools".into(), tools);
        context.insert("documents".into(), Value::Null);
        context.insert("add_generation_prompt".into(), add_generation_prompt);
        let template = self.env.get_template("chat").map_err(|e| render_error(&e))?;
        template.render(to_jinja(&Value::Object(context))).map_err(|e| render_error(&e))
    }

    /// As render_chat_template: a template that rejects the effort renders its alias instead, and one that rejects
    /// "none" while thinking is off renders by enable_thinking alone. The first error is the one reported.
    pub(crate) fn render(&self, messages: &Value, options: &Map<String, Value>) -> Result<String, RenderError> {
        let error = match self.render_once(messages, options) {
            Ok(text) => return Ok(text),
            Err(error) if error.is_template_error() => error,
            Err(error) => return Err(error),
        };
        let effort = options.get("reasoning_effort").and_then(Value::as_str);
        let mut retry = options.clone();
        match effort {
            Some(effort) if effort_alias(effort).is_some() => {
                retry.insert("reasoning_effort".into(), effort_alias(effort).unwrap().into());
            }
            Some("none") if options.get("enable_thinking") == Some(&Value::Bool(false)) => {
                retry.shift_remove("reasoning_effort");
            }
            _ => return Err(error),
        }
        self.render_once(messages, &retry).map_err(|_| error)
    }
}

/// Jinja2 lexes CRLF and CR as LF and reads string literals with Python's escapes; `{% generation %}` renders its
/// body in a scope of its own (transformers' AssistantTracker call block).
fn prepare_source(source: &str) -> String {
    let source = source.replace("\r\n", "\n").replace('\r', "\n");
    later_system::rewrite_for_minijinja(&source)
}

const RAISED: &str = "\u{1}splash-raise:";

fn render_error(error: &Error) -> RenderError {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = current {
        if let Some(found) = e.downcast_ref::<Error>().and_then(|e| e.detail()).and_then(|d| d.strip_prefix(RAISED)) {
            return RenderError::Raised(found.to_owned());
        }
        current = e.source();
    }
    // Jinja2's TemplateError family: syntax, undefined names and values, unknown filters and tests.
    match error.kind() {
        ErrorKind::SyntaxError
        | ErrorKind::BadEscape
        | ErrorKind::InvalidDelimiter
        | ErrorKind::TemplateNotFound
        | ErrorKind::BadInclude
        | ErrorKind::UnknownBlock
        | ErrorKind::UnknownFilter
        | ErrorKind::UnknownTest
        | ErrorKind::UnknownFunction
        | ErrorKind::UnknownMethod
        | ErrorKind::UndefinedError
        | ErrorKind::EvalBlock => RenderError::Template(error_text(error)),
        _ => RenderError::Internal(error_text(error)),
    }
}

fn error_text(error: &Error) -> String {
    match error.detail() {
        Some(detail) => format!("{}: {detail}", error.kind()),
        None => error.kind().to_string(),
    }
}

fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_syntax(SyntaxConfig::builder().trim_blocks(true).lstrip_blocks(true).build().expect("syntax"));
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_unknown_method_callback(python_method);
    env.set_formatter(|out, _state, value| {
        out.write_str(&py_str_value(value)).map_err(|_| Error::new(ErrorKind::WriteFailure, "write"))
    });
    env.add_filter("tojson", tojson);
    env.add_filter("length", py_length);
    env.add_filter("count", py_length);
    env.add_filter("items", py_items);
    env.add_filter("join", py_join);
    env.add_filter("round", py_round);
    env.add_test("sequence", |value: JinjaValue| py_is_sequence(&value));
    env.add_filter("string", |value: JinjaValue| py_str_value(&value));
    env.add_filter("trim", |value: JinjaValue, chars: Option<String>| strip_chars(&py_str_value(&value), chars.as_deref(), true, true));
    env.add_function("raise_exception", |message: JinjaValue| -> Result<JinjaValue, Error> {
        Err(Error::new(ErrorKind::InvalidOperation, format!("{RAISED}{}", py_str_value(&message))))
    });
    env.add_function("strftime_now", |format: String| strftime_now(&format));
    env
}

/// A JSON object as a template map that keeps its key order, as Python dicts do.
#[derive(Debug)]
struct OrderedMap(Vec<(JinjaValue, JinjaValue)>);

impl Object for OrderedMap {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }
    fn get_value(self: &Arc<Self>, key: &JinjaValue) -> Option<JinjaValue> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }
    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Values(self.0.iter().map(|(k, _)| k.clone()).collect())
    }
    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(self.0.len())
    }
}

pub(crate) fn to_jinja(value: &Value) -> JinjaValue {
    match value {
        Value::Null => JinjaValue::from(()),
        Value::Bool(b) => JinjaValue::from(*b),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => JinjaValue::from(i),
            (None, Some(u)) => JinjaValue::from(u),
            _ => JinjaValue::from(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => JinjaValue::from(s.as_str()),
        Value::Array(items) => items.iter().map(to_jinja).collect(),
        Value::Object(map) => JinjaValue::from_object(OrderedMap(
            map.iter().map(|(k, v)| (JinjaValue::from(k.as_str()), to_jinja(v))).collect(),
        )),
    }
}

/// A template value as JSON; None for what JSON cannot hold (undefined, functions, non-finite floats).
pub(crate) fn from_jinja(value: &JinjaValue) -> Option<Value> {
    Some(match value.kind() {
        ValueKind::None => Value::Null,
        ValueKind::Bool => Value::Bool(value.is_true()),
        ValueKind::Number if value.is_integer() => match value.as_i64() {
            Some(i) => Value::from(i),
            None => Value::from(u64::try_from(value.clone()).ok()?),
        },
        ValueKind::Number => Value::from(serde_json::Number::from_f64(f64::try_from(value.clone()).ok()?)?),
        ValueKind::String => Value::String(value.as_str()?.to_owned()),
        ValueKind::Seq | ValueKind::Iterable => {
            Value::Array(value.try_iter().ok()?.map(|v| from_jinja(&v)).collect::<Option<_>>()?)
        }
        ValueKind::Map => {
            let mut map = Map::new();
            for key in value.try_iter().ok()? {
                let item = value.get_item(&key).ok()?;
                let key = match key.as_str() {
                    Some(k) => k.to_owned(),
                    None => py_str_value(&key),
                };
                map.insert(key, from_jinja(&item)?);
            }
            Value::Object(map)
        }
        _ => return None,
    })
}

/// Python's `str(value)` for a template value.
pub(crate) fn py_str_value(value: &JinjaValue) -> String {
    match value.kind() {
        ValueKind::Undefined => String::new(),
        ValueKind::String => value.as_str().unwrap_or_default().to_owned(),
        ValueKind::None => "None".into(),
        ValueKind::Bool => if value.is_true() { "True" } else { "False" }.into(),
        ValueKind::Number | ValueKind::Seq | ValueKind::Map => match from_jinja(value) {
            Some(json) => pyjson::py_str(&json),
            None => value.to_string(),
        },
        _ => value.to_string(),
    }
}

/// transformers' `tojson(x, ensure_ascii=False, indent=None, separators=None, sort_keys=False)`.
fn tojson(value: JinjaValue, args: Rest<JinjaValue>, kwargs: Kwargs) -> Result<JinjaValue, Error> {
    let positional = |i: usize, name: &str| -> Result<Option<JinjaValue>, Error> {
        let keyword: Option<JinjaValue> = kwargs.get(name)?;
        Ok(keyword.or_else(|| args.get(i).cloned()).filter(|v| !v.is_none() && !v.is_undefined()))
    };
    let mut dumps = Dumps::default();
    if let Some(v) = positional(0, "ensure_ascii")? {
        dumps.ensure_ascii = v.is_true();
    }
    if let Some(v) = positional(1, "indent")? {
        dumps.indent = Some(match v.as_i64() {
            Some(n) => " ".repeat(n.max(0) as usize).into(),
            None => py_str_value(&v).into(),
        });
        dumps.item_separator = ",".into();
    }
    if let Some(v) = positional(2, "separators")? {
        let parts: Vec<String> = v.try_iter()?.map(|p| py_str_value(&p)).collect();
        if let [item, key] = parts.as_slice() {
            dumps.item_separator = item.clone().into();
            dumps.key_separator = key.clone().into();
        }
    }
    if let Some(v) = positional(3, "sort_keys")? {
        dumps.sort_keys = v.is_true();
    }
    kwargs.assert_all_used()?;
    if value.is_undefined() {
        return Err(Error::new(ErrorKind::InvalidOperation, "Object of type Undefined is not JSON serializable"));
    }
    let json = from_jinja(&value)
        .ok_or_else(|| Error::new(ErrorKind::InvalidOperation, "value is not JSON serializable"))?;
    Ok(JinjaValue::from_safe_string(dumps.dumps(&json)))
}

/// `|length` of an undefined value is 0, as `len(Undefined)` is.
fn py_length(value: JinjaValue) -> Result<usize, Error> {
    if value.is_undefined() {
        return Ok(0);
    }
    minijinja::filters::length(&value)
}

/// `|items` of an undefined value iterates nothing.
fn py_items(value: JinjaValue) -> Result<JinjaValue, Error> {
    if value.is_undefined() {
        return Ok(JinjaValue::from(Vec::<JinjaValue>::new()));
    }
    minijinja::filters::items(&value)
}

/// Jinja2's `join(value, d='', attribute=None)` over each item's `str()`.
fn py_join(value: JinjaValue, separator: Option<String>, kwargs: Kwargs) -> Result<String, Error> {
    let attribute: Option<String> = kwargs.get("attribute")?;
    kwargs.assert_all_used()?;
    if value.is_undefined() || value.is_none() {
        return Ok(String::new());
    }
    if let Some(text) = value.as_str() {
        return Ok(text.chars().map(String::from).collect::<Vec<_>>().join(separator.as_deref().unwrap_or("")));
    }
    let mut parts = Vec::new();
    for item in value.try_iter()? {
        let item = match &attribute {
            // A dotted path, each part an attribute or, for digits, an index (Jinja2's make_attrgetter).
            Some(path) => path.split('.').try_fold(item, |v, part| match part.parse::<i64>() {
                Ok(index) => v.get_item(&JinjaValue::from(index)),
                Err(_) => v.get_attr(part),
            })?,
            None => item,
        };
        parts.push(py_str_value(&item));
    }
    Ok(parts.join(separator.as_deref().unwrap_or("")))
}

/// Jinja2's `round(value, precision=0, method='common')`: Python's `round` (half to even, on the exact decimal
/// value), or ceil or floor, as a float; an integer rounds to itself.
fn py_round(value: JinjaValue, precision: Option<i32>, method: Option<String>, kwargs: Kwargs) -> Result<JinjaValue, Error> {
    let precision = match precision {
        Some(p) => p,
        None => kwargs.get::<Option<i32>>("precision")?.unwrap_or(0),
    };
    let method = match method {
        Some(m) => m,
        None => kwargs.get::<Option<String>>("method")?.unwrap_or_else(|| "common".into()),
    };
    kwargs.assert_all_used()?;
    let scale = 10f64.powi(precision);
    match method.as_str() {
        "common" if value.is_integer() && precision >= 0 => Ok(value),
        "common" => {
            let x = f64::try_from(value)?;
            if precision >= 0 {
                // Rust formats the exact binary value with ties to even, as Python's round does.
                let text = format!("{x:.prec$}", prec = precision as usize);
                Ok(JinjaValue::from(text.parse::<f64>().unwrap_or(x)))
            } else {
                Ok(JinjaValue::from((x / 10f64.powi(-precision)).round_ties_even() * 10f64.powi(-precision)))
            }
        }
        "ceil" => Ok(JinjaValue::from((f64::try_from(value)? * scale).ceil() / scale)),
        "floor" => Ok(JinjaValue::from((f64::try_from(value)? * scale).floor() / scale)),
        _ => Err(Error::new(ErrorKind::InvalidOperation, "method must be common, ceil or floor")),
    }
}

/// Jinja2's `sequence` test: anything with a length and items, so strings, dicts and undefined too.
fn py_is_sequence(value: &JinjaValue) -> bool {
    matches!(value.kind(), ValueKind::String | ValueKind::Seq | ValueKind::Map | ValueKind::Undefined | ValueKind::Bytes)
}

fn strip_chars(text: &str, chars: Option<&str>, left: bool, right: bool) -> String {
    let matches = |c: char| match chars {
        Some(set) => set.contains(c),
        None => pyjson::is_py_space(c),
    };
    let mut out = text;
    if left {
        out = out.trim_start_matches(matches);
    }
    if right {
        out = out.trim_end_matches(matches);
    }
    out.to_owned()
}

/// pycompat's Python methods, with Python's whitespace for `strip`, `split` and `splitlines`.
fn python_method(state: &mut State, value: &JinjaValue, method: &str, args: &[JinjaValue]) -> Result<JinjaValue, Error> {
    if let Some(text) = value.as_str() {
        let arg = |i: usize| args.get(i).filter(|v| !v.is_none()).map(py_str_value);
        match method {
            "strip" | "lstrip" | "rstrip" if args.len() <= 1 => {
                let chars = arg(0);
                return Ok(strip_chars(text, chars.as_deref(), method != "rstrip", method != "lstrip").into());
            }
            "split" if args.first().is_none_or(|v| v.is_none()) => {
                let limit = args.get(1).and_then(JinjaValue::as_i64).filter(|n| *n >= 0);
                return Ok(split_whitespace(text, limit.map(|n| n as usize)));
            }
            "splitlines" => {
                let keep = args.first().is_some_and(|v| v.is_true());
                return Ok(splitlines(text, keep));
            }
            _ => {}
        }
    }
    minijinja_contrib::pycompat::unknown_method_callback(state, value, method, args)
}

fn split_whitespace(text: &str, limit: Option<usize>) -> JinjaValue {
    let mut parts = Vec::new();
    let mut rest = text.trim_start_matches(pyjson::is_py_space);
    while !rest.is_empty() {
        if limit.is_some_and(|l| parts.len() == l) {
            parts.push(JinjaValue::from(rest));
            break;
        }
        let end = rest.find(pyjson::is_py_space).unwrap_or(rest.len());
        parts.push(JinjaValue::from(&rest[..end]));
        rest = rest[end..].trim_start_matches(pyjson::is_py_space);
    }
    JinjaValue::from(parts)
}

fn splitlines(text: &str, keep: bool) -> JinjaValue {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let is_break = matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if !is_break {
            continue;
        }
        let mut end = i + c.len_utf8();
        if c == '\r' && chars.peek().is_some_and(|(_, n)| *n == '\n') {
            chars.next();
            end += 1;
        }
        parts.push(JinjaValue::from(if keep { &text[start..end] } else { &text[start..i] }));
        start = end;
    }
    if start < text.len() {
        parts.push(JinjaValue::from(&text[start..]));
    }
    JinjaValue::from(parts)
}

/// `datetime.now().strftime(format)`: a naive local time, so `%z` and `%Z` are empty and `%f` is the microseconds;
/// the rest is the C library's strftime with a buffer grown as Python's time.strftime grows it.
fn strftime_now(format: &str) -> Result<String, Error> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let mut expanded = String::with_capacity(format.len());
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            expanded.push(c);
            continue;
        }
        match chars.next() {
            Some('f') => expanded.push_str(&format!("{:06}", now.subsec_micros())),
            Some('z' | 'Z') => {}
            Some(':') if chars.clone().next() == Some('z') => {
                chars.next();
            }
            Some(other) => {
                expanded.push('%');
                expanded.push(other);
            }
            None => expanded.push('%'),
        }
    }
    if expanded.is_empty() {
        return Ok(String::new());
    }
    let format = std::ffi::CString::new(expanded).map_err(|_| Error::new(ErrorKind::InvalidOperation, "embedded null character"))?;
    let format_len = format.as_bytes().len();
    // SAFETY: localtime_r and strftime write only into the buffers given, within the lengths given.
    unsafe {
        let seconds = now.as_secs() as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&seconds, &mut tm);
        let mut size = 1024;
        loop {
            let mut buffer = vec![0u8; size];
            let n = libc::strftime(buffer.as_mut_ptr().cast(), size, format.as_ptr(), &tm);
            if n > 0 || size >= 256 * format_len {
                buffer.truncate(n);
                return Ok(String::from_utf8_lossy(&buffer).into_owned());
            }
            size *= 2;
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The probed template.

/// Generation prompts probed for options other than the efforts', at most this many and bytes (_PROBED_OPTIONS).
const PROBED_OPTIONS: usize = 64;
const PROBED_OPTIONS_BYTES: usize = 8 * 1024 * 1024;

/// One of the tokenizer's templates, probed and patched once at startup.
pub struct ChatTemplate {
    /// The source requests render: the tokenizer's, or its patch.
    pub source: String,
    pub later_system: LaterSystem,
    pub original: Original,
    compiled: Compiled,
    encode: Encoder,
    /// Generation prompts by `generation_key`; `None` where it is empty or the probes disagree.
    efforts: Vec<(String, Option<GenerationPrompt>)>,
    probed: Mutex<VecDeque<(String, Option<GenerationPrompt>, usize)>>,
    /// The source compiled with the image marker last asked for; a stable marker compiles once.
    image_marked: Mutex<Option<(String, Arc<Compiled>)>>,
}

impl ChatTemplate {
    pub fn prepare(source: &str, special_tokens: &Map<String, Value>, encode: Encoder) -> Result<Self, RenderError> {
        let original_compiled = Compiled::new(source, special_tokens)?;
        let original = later_system::original(&original_compiled);
        let mut later = LaterSystem::Unsupported;
        let mut effective = source.to_owned();
        let mut compiled = original_compiled;
        match original {
            Original::Renders => later = LaterSystem::Native,
            Original::Rejects | Original::Drops => {
                if let Some(patched) = later_system::patch(&compiled, source, original) {
                    if let Ok(patched_compiled) = Compiled::new(&patched, special_tokens) {
                        if later_system::verified(&compiled, &patched_compiled) {
                            later = LaterSystem::Patched;
                            effective = patched;
                            compiled = patched_compiled;
                        }
                    }
                }
            }
            Original::Misplaces => {}
        }
        let mut efforts = Vec::new();
        for effort in std::iter::once(None).chain(REASONING_EFFORTS.iter().map(|e| Some(*e))) {
            let options = template_options(effort, None, None, true, None);
            let key = generation_key(&options);
            let found = later_system::generation_prompt(&compiled, &options).map(|text| GenerationPrompt {
                tokens: encode(&text),
                text,
            });
            efforts.push((key, found));
        }
        Ok(ChatTemplate {
            source: effective,
            later_system: later,
            original,
            compiled,
            encode,
            efforts,
            probed: Mutex::new(VecDeque::new()),
            image_marked: Mutex::new(None),
        })
    }

    /// Whether requests with these normalized messages are served: a later system message needs a template that
    /// renders it.
    pub fn accepts(&self, messages: &[Value]) -> bool {
        self.later_system != LaterSystem::Unsupported
            || messages.iter().skip(1).all(|m| m.get("role").and_then(Value::as_str) != Some("system"))
    }

    /// Render the conversation under template options (alias retry included).
    pub fn render(&self, messages: &[Value], options: &Map<String, Value>) -> Result<String, RenderError> {
        self.compiled.render(&Value::Array(messages.to_vec()), options)
    }

    /// Render with the source's `<|image_pad|>` replaced by `marker`, as frontend._render_image_tokens does to
    /// find the template's own image placeholders.
    pub fn render_with_image_marker(
        &self,
        messages: &[Value],
        options: &Map<String, Value>,
        marker: &str,
    ) -> Result<String, RenderError> {
        let compiled = {
            let mut cached = self.image_marked.lock().expect("image template");
            match cached.as_ref().filter(|(m, _)| m == marker) {
                Some((_, compiled)) => compiled.clone(),
                None => {
                    let source = self.source.replace(super::IMAGE_PAD_TOKEN, marker);
                    let compiled = Arc::new(Compiled::new(&source, &self.compiled.special_tokens)?);
                    *cached = Some((marker.to_owned(), compiled.clone()));
                    compiled
                }
            }
        };
        compiled.render(&Value::Array(messages.to_vec()), options)
    }

    /// The generation prompt under these request options (the full template variables a request renders with);
    /// None where the probe found none.
    pub fn generation_prompt(&self, options: &Map<String, Value>) -> Option<GenerationPrompt> {
        let key = generation_key(options);
        if let Some((_, Some(found))) = self.efforts.iter().find(|(k, _)| *k == key) {
            return Some(found.clone());
        }
        let mut probed = self.probed.lock().expect("probe cache");
        if let Some(index) = probed.iter().position(|(k, _, _)| *k == key) {
            let entry = probed.remove(index).expect("entry");
            let found = entry.1.clone();
            probed.push_back(entry);
            return found;
        }
        drop(probed);
        let mut options: Map<String, Value> = serde_json::from_str(&key).expect("key");
        options.insert("add_generation_prompt".into(), true.into());
        let found = later_system::generation_prompt(&self.compiled, &options)
            .map(|text| GenerationPrompt { tokens: (self.encode)(&text), text });
        let size = key.len() + found.as_ref().map_or(0, |f| f.text.len() + 4 * f.tokens.len());
        let mut probed = self.probed.lock().expect("probe cache");
        if size <= PROBED_OPTIONS_BYTES && !probed.iter().any(|(k, _, _)| *k == key) {
            probed.push_back((key, found.clone(), size));
            while probed.len() > PROBED_OPTIONS
                || probed.iter().map(|(_, _, s)| s).sum::<usize>() > PROBED_OPTIONS_BYTES
            {
                probed.pop_front();
            }
        }
        found
    }

    /// The probed generation prompts for each reasoning effort, by options key (for the startup log and tests).
    pub fn effort_prompts(&self) -> impl Iterator<Item = (&str, Option<&GenerationPrompt>)> {
        self.efforts.iter().map(|(k, p)| (k.as_str(), p.as_ref()))
    }
}

/// The options that can shape the generation prompt, as a canonical key: all but tools, the history-only
/// preserve_thinking and apply_chat_template's own controls.
pub fn generation_key(options: &Map<String, Value>) -> String {
    let kept: std::collections::BTreeMap<&String, &Value> = options
        .iter()
        .filter(|(k, _)| !RESERVED_TEMPLATE_KWARGS.contains(&k.as_str()) && k.as_str() != "preserve_thinking")
        .collect();
    serde_json::to_string(&sorted(&serde_json::to_value(kept).expect("key"))).expect("key")
}

/// The value with every object's keys sorted, as `json.dumps(sort_keys=True)` writes it.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k.clone(), sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// The tokenizer's templates, probed and chosen once at startup (ChatTemplates).
pub struct ChatTemplates {
    templates: Vec<(Option<String>, Arc<ChatTemplate>)>,
}

impl ChatTemplates {
    pub fn new(files: &TemplateFiles, encode: Encoder) -> Result<Self, ChatTemplateError> {
        if files.templates.is_empty() || files.templates.iter().any(|(_, s)| s.is_empty()) {
            return Err(ChatTemplateError::Missing);
        }
        let named = files.templates.iter().any(|(n, _)| n.is_some());
        if named && !files.templates.iter().any(|(n, _)| n.as_deref() == Some("default")) {
            return Err(ChatTemplateError::NoDefault);
        }
        let mut prepared: Vec<(String, Arc<ChatTemplate>)> = Vec::new();
        let mut templates = Vec::new();
        for (name, source) in &files.templates {
            let template = match prepared.iter().find(|(s, _)| s == source) {
                Some((_, t)) => t.clone(),
                None => {
                    let t = Arc::new(
                        ChatTemplate::prepare(source, &files.special_tokens, encode.clone())
                            .map_err(|e| ChatTemplateError::Io(e.to_string()))?,
                    );
                    prepared.push((source.clone(), t.clone()));
                    t
                }
            };
            templates.push((name.clone(), template));
        }
        Ok(ChatTemplates { templates })
    }

    pub fn load(dir: &Path, encode: Encoder) -> Result<Self, ChatTemplateError> {
        Self::new(&TemplateFiles::read(dir)?, encode)
    }

    /// The template the tokenizer itself would choose for this request.
    pub fn select(&self, tools: Option<&[Value]>) -> &Arc<ChatTemplate> {
        let find = |name: Option<&str>| self.templates.iter().find(|(n, _)| n.as_deref() == name).map(|(_, t)| t);
        if let Some(t) = find(None) {
            return t;
        }
        if tools.is_some_and(|t| !t.is_empty()) {
            if let Some(t) = find(Some("tool_use")) {
                return t;
            }
        }
        find(Some("default")).expect("default template")
    }

    /// `{"later_system": ...}` for /status.
    pub fn status(&self) -> Value {
        if let [(None, t)] = self.templates.as_slice() {
            return json!({"later_system": t.later_system.as_str()});
        }
        let by_name: Map<String, Value> = self
            .templates
            .iter()
            .map(|(n, t)| (n.clone().unwrap_or_default(), t.later_system.as_str().into()))
            .collect();
        json!({"later_system": by_name})
    }

    /// One line for the startup log.
    pub fn describe(&self) -> String {
        self.templates
            .iter()
            .map(|(name, t)| {
                let what = match t.later_system {
                    LaterSystem::Native => "renders later system messages in place".to_owned(),
                    LaterSystem::Patched => format!(
                        "patched to render later system messages in place (the original template {} them)",
                        t.original.as_str()
                    ),
                    LaterSystem::Unsupported => format!(
                        "requests with later system messages are rejected (the original template {} them)",
                        t.original.as_str()
                    ),
                };
                let mut counts: Vec<usize> = t.efforts.iter().filter_map(|(_, p)| p.as_ref().map(|p| p.tokens.len())).collect();
                counts.sort_unstable();
                counts.dedup();
                let generation = match counts.as_slice() {
                    [] => "generation prompt unknown".to_owned(),
                    [one] => format!("generation prompt {one} {}", if *one == 1 { "token" } else { "tokens" }),
                    [first, .., last] => format!("generation prompt {first}-{last} tokens"),
                };
                format!("{}{what}, {generation}", name.as_ref().map(|n| format!("{n} ")).unwrap_or_default())
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

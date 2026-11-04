//! Reading the model's output as it arrives (output.py): its reasoning, its answer and its tool calls, as OpenAI
//! chat-completions deltas.
//!
//! Python slices text by code point; here text is UTF-8, so every length that Python measures in characters
//! (held partial tags, name limits, delta sizes) is counted in chars, and positions only ever compare or cut at
//! char boundaries.

use super::format::{ChatFormat, ToolCallSyntax, THINK_END, THINK_START, TOOL_CALL_CLOSE};
use super::pyjson::{self, codec_dumps, codec_dumps_str};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};

/// The most characters one argument delta carries, so SSE frames stay bounded for a large string argument.
pub const TOOL_ARGUMENT_DELTA_CHARS: usize = 16 * 1024;
/// The deepest a call's arguments nest as an object the server writes (json_codec.MAX_DEPTH - 5).
pub const MAX_ARGUMENTS_DEPTH: usize = pyjson::MAX_DEPTH - 5;
/// Tool and parameter names are 1 to this many characters (tool_schema.MAX_NAME_LENGTH).
pub const MAX_NAME_LENGTH: usize = 256;
/// The space names lose around them (tool_schema.NAME_SPACE).
const NAME_SPACE: &[char] = &[' ', '\t', '\n', '\r'];

/// The byte offset of the char `n` chars into `text` (or its end).
fn char_offset(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(i, _)| i)
}

/// `text` split before its longest end that begins one of `markers`, which more text may complete.
pub fn hold_partial<'a>(text: &'a str, markers: &[&str]) -> (&'a str, &'a str) {
    let longest = markers.iter().map(|m| m.chars().count()).max().unwrap_or(0);
    if longest < 2 || text.is_empty() {
        return (text, "");
    }
    // Byte offsets of the last `longest - 1` char starts, nearest the end first.
    let starts: Vec<usize> = text.char_indices().rev().take(longest - 1).map(|(i, _)| i).collect();
    for length in (1..=starts.len()).rev() {
        let suffix = &text[starts[length - 1]..];
        let held = markers.iter().any(|m| {
            let prefix_end = char_offset(m, length);
            length < m.chars().count() && &m[..prefix_end] == suffix
        });
        if held {
            return text.split_at(starts[length - 1]);
        }
    }
    (text, "")
}

/// The earliest of `tags` in `text`, ties broken as Python's `min` over (index, tag) does.
fn next_tag<'t>(text: &str, tags: &[&'t str]) -> Option<(usize, &'t str)> {
    tags.iter().filter_map(|tag| text.find(tag).map(|i| (i, *tag))).min()
}

/// One piece of a chat-completions stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delta {
    Reasoning(String),
    Content(String),
    /// A call's header: `{"index", "id", "type": "function", "function": {"name"}}`.
    ToolCallStart { index: usize, id: String, name: String },
    /// A fragment of a call's arguments JSON: `{"index", "function": {"arguments"}}`.
    ToolCallArguments { index: usize, arguments: String },
}

impl Delta {
    /// The delta as OpenAI's `choices[0].delta` object.
    pub fn to_openai(&self) -> Value {
        match self {
            Delta::Reasoning(text) => json!({"reasoning_content": text}),
            Delta::Content(text) => json!({"content": text}),
            Delta::ToolCallStart { index, id, name } => {
                json!({"tool_calls": [{"index": index, "id": id, "type": "function", "function": {"name": name}}]})
            }
            Delta::ToolCallArguments { index, arguments } => {
                json!({"tool_calls": [{"index": index, "function": {"arguments": arguments}}]})
            }
        }
    }
}

/// A complete call as the response's `message.tool_calls` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn to_openai(&self) -> Value {
        json!({"id": self.id, "type": "function", "function": {"name": self.name, "arguments": self.arguments}})
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Reasoning.

/// The output's reasoning and its answer, as the output arrives (ReasoningSplitter). Reasoning starts the output
/// where the generation prompt opened a think block, or follows a THINK_START that starts it where the model opens
/// its reasoning itself. It ends at THINK_END and, where a call may follow and a call's opening ends reasoning, at a
/// call's opening, which begins the answer.
#[derive(Clone, Debug)]
pub struct ReasoningSplitter {
    /// Whether the output is in its reasoning.
    pub reasoning: bool,
    /// Whether the output may yet open its reasoning: until it shows whether it starts with THINK_START.
    opening: bool,
    pending: String,
    /// Whether the newlines after `</think>`, which set the answer apart, are still to be dropped.
    separator: bool,
    ends: Vec<String>,
}

/// Which field a piece of text belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Reasoning,
    Content,
}

impl ReasoningSplitter {
    pub fn new(thinking: bool, tool_calls: bool, format: Option<&ChatFormat>) -> Self {
        let mut ends = vec![THINK_END.to_owned()];
        if let Some(format) = format.filter(|f| tool_calls && f.calls_end_reasoning()) {
            ends.push(format.call_syntax.call_open());
        }
        ReasoningSplitter {
            reasoning: thinking,
            opening: !thinking && format.is_some_and(|f| f.think_start_id.is_some()),
            pending: String::new(),
            separator: false,
            ends,
        }
    }

    pub fn put(&mut self, text: &str) -> Vec<(Field, String)> {
        let mut text = text.to_owned();
        if self.opening {
            self.pending.push_str(&text);
            if self.pending.chars().count() < THINK_START.chars().count() && THINK_START.starts_with(self.pending.as_str()) {
                return Vec::new();
            }
            self.opening = false;
            text = std::mem::take(&mut self.pending);
            if let Some(rest) = text.strip_prefix(THINK_START) {
                self.reasoning = true;
                text = rest.to_owned();
            }
        }
        if !self.reasoning {
            return self.content(text);
        }
        self.pending.push_str(&text);
        let ends: Vec<&str> = self.ends.iter().map(String::as_str).collect();
        if let Some((end, marker)) = next_tag(&self.pending, &ends) {
            let reasoning = self.pending[..end].to_owned();
            let mut content = &self.pending[end..];
            if marker == THINK_END {
                content = &content[THINK_END.len()..];
            }
            let content = content.to_owned();
            self.separator = marker == THINK_END;
            self.pending.clear();
            self.reasoning = false;
            let mut out = Vec::new();
            if !reasoning.is_empty() {
                out.push((Field::Reasoning, reasoning));
            }
            out.extend(self.content(content));
            return out;
        }
        let (ready, held) = hold_partial(&self.pending, &ends);
        let (ready, held) = (ready.to_owned(), held.to_owned());
        self.pending = held;
        if ready.is_empty() {
            Vec::new()
        } else {
            vec![(Field::Reasoning, ready)]
        }
    }

    fn content(&mut self, text: String) -> Vec<(Field, String)> {
        let mut text = text;
        if self.separator {
            text = text.trim_start_matches('\n').to_owned();
            if text.is_empty() {
                return Vec::new();
            }
            self.separator = false;
        }
        vec![(Field::Content, text)]
    }

    pub fn finish(&mut self) -> Vec<(Field, String)> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let field = if self.reasoning { Field::Reasoning } else { Field::Content };
        vec![(field, std::mem::take(&mut self.pending))]
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Argument values.

/// JSON type names (`"string"`, `"integer"`, `"number"`, `"boolean"`, `"null"`, `"array"`, `"object"`).
pub type TypeSet = BTreeSet<String>;

/// The JSON types a tool's parameter values convert by (tool_schema.ParameterTypes.of): those of each declared
/// parameter and those of any other; None where the schema constrains no type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolParameterTypes {
    pub parameters: HashMap<String, Option<TypeSet>>,
    pub other: Option<TypeSet>,
}

/// Where the projector reads a tool's parameter types when a call first names it. The server implements it over
/// the request's tool policy (the constrain port's ParameterTypes); a map serves fixed types.
pub trait ToolTypes: Send {
    fn of(&mut self, name: &str) -> ToolParameterTypes;
}

impl ToolTypes for HashMap<String, ToolParameterTypes> {
    fn of(&mut self, name: &str) -> ToolParameterTypes {
        self.get(name).cloned().unwrap_or_default()
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The JSON value `text` spells, if it can be written back in its call's arguments: finite numbers, Unicode
/// strings, containers nested at most MAX_ARGUMENTS_DEPTH deep in the arguments object.
fn json_value(text: &str) -> Option<Value> {
    let value = pyjson::codec_loads(text).ok()?;
    let mut pending = vec![(&value, 2usize)];
    while let Some((item, depth)) = pending.pop() {
        match item {
            Value::Array(items) => {
                if depth > MAX_ARGUMENTS_DEPTH {
                    return None;
                }
                pending.extend(items.iter().map(|c| (c, depth + 1)));
            }
            Value::Object(map) => {
                if depth > MAX_ARGUMENTS_DEPTH {
                    return None;
                }
                pending.extend(map.values().map(|c| (c, depth + 1)));
            }
            _ => {}
        }
    }
    Some(value)
}

/// A parameter's text as the JSON types its schema gives read it (convert_value): a string reads as the text, and
/// another value as the JSON the text spells when that is of a declared type or no declared type is a string. A
/// boolean or null declared without a string may also be spelled as Python spells it. Otherwise the text.
pub fn convert_value(text: &str, types: &TypeSet) -> Value {
    let as_text = || Value::String(text.to_owned());
    let Some(value) = json_value(text) else {
        let literal = match pyjson::py_strip(text) {
            "True" => Some(Value::Bool(true)),
            "False" => Some(Value::Bool(false)),
            "None" => Some(Value::Null),
            _ => None,
        };
        return match literal {
            Some(v) if !types.contains("string") && types.contains(json_type(&v)) => v,
            _ => as_text(),
        };
    };
    if types.contains("string") {
        let kind = json_type(&value);
        let declared = types.contains(kind) || (kind == "integer" && types.contains("number"));
        return if declared && kind != "string" { value } else { as_text() };
    }
    value
}

/// A fragment cut into deltas of at most TOOL_ARGUMENT_DELTA_CHARS characters.
fn argument_deltas(fragment: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = fragment;
    while !rest.is_empty() {
        let cut = char_offset(rest, TOOL_ARGUMENT_DELTA_CHARS);
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Tool calls.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Output,
    Json,
    Content,
    Name,
    Arguments,
    Parameter,
    Value,
    Closing,
}

/// The tool calls a model writes in its template's layout, parsed out of the answer as it arrives into OpenAI
/// argument deltas (StreamingToolCallProjector). Text outside calls streams as content, and a stray `</think>`
/// there is dropped. Without tool types (no tool policy) a call's opening is text.
pub struct ToolCallProjector {
    tool_types: Option<Box<dyn ToolTypes>>,
    request_id: String,
    syntax: ToolCallSyntax,
    text_tags: Vec<String>,
    pending: String,
    state: State,
    call_index: usize,
    call_id: Option<String>,
    function_name: Option<String>,
    parameter_types: HashMap<String, Option<TypeSet>>,
    other_types: Option<TypeSet>,
    parameter_names: BTreeSet<String>,
    argument_fragments: String,
    parameter_count: usize,
    parameter_name: Option<String>,
    value_types: Option<TypeSet>,
    value_streams: bool,
    value_started: bool,
    value_parts: String,
    content_fragments: Vec<String>,
    streamed_count: usize,
    text_visible: bool,
    closed_calls: Vec<ToolCall>,
}

impl ToolCallProjector {
    /// `tool_types` is None without a tool policy; `structured` for a response_format answer, whose JSON is left as
    /// it is.
    pub fn new(tool_types: Option<Box<dyn ToolTypes>>, request_id: &str, structured: bool, syntax: ToolCallSyntax) -> Self {
        let text_tags = if tool_types.is_some() {
            vec![syntax.call_open(), THINK_END.to_owned()]
        } else {
            vec![THINK_END.to_owned()]
        };
        let text_visible = tool_types.is_none();
        ToolCallProjector {
            tool_types,
            request_id: request_id.to_owned(),
            syntax,
            text_tags,
            pending: String::new(),
            state: if structured { State::Output } else { State::Content },
            call_index: 0,
            call_id: None,
            function_name: None,
            parameter_types: HashMap::new(),
            other_types: None,
            parameter_names: BTreeSet::new(),
            argument_fragments: String::new(),
            parameter_count: 0,
            parameter_name: None,
            value_types: None,
            value_streams: false,
            value_started: false,
            value_parts: String::new(),
            content_fragments: Vec::new(),
            streamed_count: 0,
            text_visible,
            closed_calls: Vec::new(),
        }
    }

    /// The id of the open call, None while the text names none.
    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    /// Whether the output so far ends inside a call, past its opening.
    pub fn in_call(&self) -> bool {
        matches!(self.state, State::Name | State::Arguments | State::Parameter | State::Value)
    }

    fn emit_content(&mut self, value: &str, events: &mut Vec<Delta>) {
        if value.is_empty() {
            return;
        }
        self.content_fragments.push(value.to_owned());
        // Whitespace that starts the output or follows a call waits for visible text: alone it only frames calls.
        if !self.text_visible {
            if pyjson::py_strip(value).is_empty() {
                return;
            }
            self.text_visible = true;
        }
        let unsent = self.content_fragments[self.streamed_count..].concat();
        self.streamed_count = self.content_fragments.len();
        events.push(Delta::Content(unsent));
    }

    fn streamed_content(&self) -> String {
        self.content_fragments[..self.streamed_count].concat()
    }

    fn emit_argument(&mut self, fragment: &str, events: &mut Vec<Delta>) {
        if self.call_id.is_none() {
            return;
        }
        self.argument_fragments.push_str(fragment);
        for chunk in argument_deltas(fragment) {
            events.push(Delta::ToolCallArguments { index: self.call_index, arguments: chunk.to_owned() });
        }
    }

    fn begin_call(&mut self, name: &str, events: &mut Vec<Delta>) {
        self.state = State::Arguments;
        self.argument_fragments.clear();
        self.parameter_count = 0;
        self.parameter_names.clear();
        let name = name.trim_matches(NAME_SPACE);
        if name.is_empty() {
            return;
        }
        if self.streamed_count == 0 {
            // Whitespace before the first text only framed the calls.
            self.content_fragments.clear();
        }
        self.function_name = Some(name.to_owned());
        let id = format!("call_{}_{}", self.request_id, self.call_index);
        self.call_id = Some(id.clone());
        let types = self.tool_types.as_mut().map(|t| t.of(name)).unwrap_or_default();
        self.parameter_types = types.parameters;
        self.other_types = types.other;
        events.push(Delta::ToolCallStart { index: self.call_index, id, name: name.to_owned() });
        self.emit_argument("{", events);
    }

    fn key(&mut self, name: &str) -> String {
        let separator = if self.parameter_count > 0 { "," } else { "" };
        self.parameter_count += 1;
        format!("{separator}{}:", codec_dumps_str(name))
    }

    fn begin_parameter(&mut self, name: &str, events: &mut Vec<Delta>) {
        let name = name.trim_matches(NAME_SPACE).to_owned();
        // A repeated parameter keeps its first value; one without a name, or of a call that names no function,
        // keeps none.
        let kept = !name.is_empty() && !self.parameter_names.contains(&name) && self.call_id.is_some();
        self.parameter_names.insert(name.clone());
        self.parameter_name = kept.then(|| name.clone());
        self.value_types = match self.parameter_types.get(&name) {
            Some(types) => types.clone(),
            None => self.other_types.clone(),
        };
        // A value of no type, or the type string alone, is its text and streams as written; any other converts at
        // its end.
        self.value_streams = match &self.value_types {
            None => true,
            Some(types) => types.is_empty() || (types.len() == 1 && types.contains("string")),
        };
        self.value_started = false;
        self.value_parts.clear();
        if self.value_streams && self.parameter_name.is_some() {
            let key = self.key(&name);
            self.emit_argument(&format!("{key}\""), events);
        }
        self.state = State::Value;
    }

    fn put_value(&mut self, text: &str, events: &mut Vec<Delta>) {
        if self.parameter_name.is_none() {
            return;
        }
        let mut text = text;
        if !self.value_started {
            if text.is_empty() {
                return;
            }
            self.value_started = true;
            text = text.strip_prefix(self.syntax.value_open).unwrap_or(text);
        }
        if text.is_empty() {
            return;
        }
        if self.value_streams {
            let quoted = codec_dumps_str(text);
            self.emit_argument(&quoted[1..quoted.len() - 1], events);
        } else {
            self.value_parts.push_str(text);
        }
    }

    fn end_parameter(&mut self, text: &str, events: &mut Vec<Delta>) {
        if let Some(name) = self.parameter_name.clone() {
            let mut text = text;
            if !self.value_started {
                self.value_started = true;
                text = text.strip_prefix(self.syntax.value_open).unwrap_or(text);
            }
            self.put_value(text, events);
            if self.value_streams {
                self.emit_argument("\"", events);
            } else {
                let types = self.value_types.clone().unwrap_or_default();
                let value = convert_value(&self.value_parts, &types);
                let key = self.key(&name);
                self.emit_argument(&format!("{key}{}", codec_dumps(&value)), events);
            }
        }
        self.parameter_name = None;
        self.value_parts.clear();
        self.state = State::Arguments;
    }

    fn finish_call(&mut self, events: &mut Vec<Delta>) {
        self.emit_argument("}", events);
        if let Some(id) = self.call_id.take() {
            self.closed_calls.push(ToolCall {
                id,
                name: self.function_name.take().unwrap_or_default(),
                arguments: std::mem::take(&mut self.argument_fragments),
            });
            self.call_index += 1;
        }
        self.function_name = None;
        self.argument_fragments.clear();
        self.text_visible = false;
        self.state = State::Closing;
    }

    /// The name the pending text spells up to `end`, taken off the text with `end` unless kept; "" for a longer
    /// one, and None while it may yet end.
    fn name(&mut self, end: &str, keep: bool) -> Option<String> {
        let window = MAX_NAME_LENGTH + end.chars().count();
        let limit = char_offset(&self.pending, window);
        match self.pending[..limit].find(end) {
            Some(index) => {
                let name = self.pending[..index].to_owned();
                let cut = if keep { index } else { index + end.len() };
                self.pending.drain(..cut);
                Some(name)
            }
            None if self.pending.chars().count() < window => None,
            None => Some(String::new()),
        }
    }

    pub fn put(&mut self, text: &str) -> Vec<Delta> {
        self.pending.push_str(text);
        let mut events = Vec::new();
        let syntax = self.syntax;
        while !self.pending.is_empty() {
            if self.state == State::Output {
                let first = self.pending.trim_start_matches(pyjson::is_py_space);
                if first.is_empty() {
                    break;
                }
                // A structured answer is one JSON value or tool calls; once JSON starts, tags in it are data.
                self.state = if first.starts_with('<') { State::Content } else { State::Json };
            }
            match self.state {
                State::Json => {
                    let text = std::mem::take(&mut self.pending);
                    self.emit_content(&text, &mut events);
                    break;
                }
                State::Content => {
                    let tags: Vec<String> = self.text_tags.clone();
                    let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
                    let Some((start, tag)) = next_tag(&self.pending, &tags) else {
                        let (ready, held) = hold_partial(&self.pending, &tags);
                        let (ready, held) = (ready.to_owned(), held.to_owned());
                        self.pending = held;
                        self.emit_content(&ready, &mut events);
                        break;
                    };
                    let before = self.pending[..start].to_owned();
                    self.emit_content(&before, &mut events);
                    self.pending.drain(..start + tag.len());
                    if tag != THINK_END {
                        self.state = State::Name;
                    }
                }
                State::Name => {
                    // The character that ends the name stays: the arguments drop it or read the tag it begins.
                    let end = syntax.name_end().to_string();
                    let Some(name) = self.name(&end, true) else { break };
                    self.begin_call(&name, &mut events);
                }
                State::Arguments => {
                    // Text between parameters is dropped, but for a tag it may begin.
                    let tags = syntax.argument_tags();
                    let Some((start, tag)) = next_tag(&self.pending, &tags) else {
                        let held = hold_partial(&self.pending, &tags).1.to_owned();
                        self.pending = held;
                        break;
                    };
                    self.pending.drain(..start + tag.len());
                    if tag == syntax.key_open {
                        self.state = State::Parameter;
                    } else {
                        self.finish_call(&mut events);
                        if tag == TOOL_CALL_CLOSE {
                            self.state = State::Content;
                        }
                    }
                }
                State::Parameter => {
                    let Some(name) = self.name(syntax.key_close, false) else { break };
                    self.begin_parameter(&name, &mut events);
                }
                State::Value => {
                    // Before the value starts, the start of the template's value_open waits for the rest.
                    if !self.value_started
                        && self.pending.chars().count() < syntax.value_open.chars().count()
                        && syntax.value_open.starts_with(self.pending.as_str())
                    {
                        break;
                    }
                    let ends = syntax.value_ends();
                    let ends: Vec<&str> = ends.iter().map(String::as_str).collect();
                    let Some((start, _)) = next_tag(&self.pending, &ends) else {
                        let held_len = hold_partial(&self.pending, &ends).1.len();
                        let ready = self.pending[..self.pending.len() - held_len].to_owned();
                        self.put_value(&ready, &mut events);
                        self.pending.drain(..self.pending.len() - held_len);
                        break;
                    };
                    let value = self.pending[..start].to_owned();
                    self.end_parameter(&value, &mut events);
                    // The tag after the close stays for the arguments to read.
                    self.pending.drain(..start + syntax.value_close.len());
                }
                State::Closing => {
                    // After </function>, the template closes the call's block.
                    let rest = self.pending.trim_start_matches(NAME_SPACE);
                    if let Some(after) = rest.strip_prefix(TOOL_CALL_CLOSE) {
                        self.pending = after.to_owned();
                    } else if TOOL_CALL_CLOSE.starts_with(rest) {
                        break;
                    }
                    self.state = State::Content;
                }
                State::Output => unreachable!("resolved above"),
            }
        }
        events
    }

    /// The answer's content and calls, and the deltas the stream still owes (StreamingToolCallProjector.finish).
    /// `incomplete` is output cut short by the token limit or by a stop sequence inside a call: it keeps an open
    /// call with the arguments it has.
    pub fn finish(&mut self, incomplete: bool) -> (String, Vec<ToolCall>, Vec<Delta>) {
        let mut events = Vec::new();
        if !incomplete {
            if self.state == State::Name {
                let pending = std::mem::take(&mut self.pending);
                let name = if pending.chars().count() <= MAX_NAME_LENGTH { pending } else { String::new() };
                self.begin_call(&name, &mut events);
            }
            if self.state == State::Value {
                self.pending.clear();
                self.end_parameter("", &mut events);
            }
            if matches!(self.state, State::Arguments | State::Parameter) {
                self.pending.clear();
                self.finish_call(&mut events);
            }
        }
        if self.state == State::Closing {
            self.pending.clear();
            self.state = State::Content;
        }
        let keeps_pending = matches!(self.state, State::Content | State::Output | State::Json)
            && !self.pending.is_empty()
            && !(incomplete && self.text_tags.iter().any(|t| t.starts_with(self.pending.as_str())));
        if keeps_pending {
            self.content_fragments.push(std::mem::take(&mut self.pending));
        } else if !self.closed_calls.is_empty() {
            // Whitespace held after the last text only framed the calls.
            self.content_fragments.truncate(self.streamed_count);
        }
        self.pending.clear();
        let mut calls = self.closed_calls.clone();
        if let Some(id) = &self.call_id {
            calls.push(ToolCall {
                id: id.clone(),
                name: self.function_name.clone().unwrap_or_default(),
                arguments: self.argument_fragments.clone(),
            });
        }
        let streamed = self.streamed_content();
        let content = if !calls.is_empty() && incomplete { streamed.clone() } else { self.content_fragments.concat() };
        let unsent = &content[streamed.len().min(content.len())..];
        if !unsent.is_empty() {
            events.push(Delta::Content(unsent.to_owned()));
        }
        (content, calls, events)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The whole answer.

/// A finished answer, as the server's `_collect` gathers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected {
    pub reasoning: String,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    /// The output ended inside its reasoning.
    pub reasoning_open: bool,
    /// The token limit or a stop sequence ended the output inside a call.
    pub cut_in_call: bool,
    /// The id of that call, whose arguments are unfinished.
    pub cut_call: Option<String>,
}

impl Collected {
    /// OpenAI's finish_reason: the output called tools when it has calls and none was cut, and the token limit did
    /// not end it.
    pub fn finish_reason<'a>(&self, engine_reason: &'a str) -> &'a str {
        if !self.tool_calls.is_empty() && !self.cut_in_call && engine_reason != "length" {
            "tool_calls"
        } else {
            engine_reason
        }
    }
}

/// A chat answer's text read into deltas: the reasoning splitter, then the call projector over the answer.
pub struct OutputReader {
    splitter: ReasoningSplitter,
    projector: ToolCallProjector,
    reasoning: String,
}

impl OutputReader {
    /// `thinking`: the generation prompt opened a think block. `tool_types`: the request's tool policy, None
    /// without one; `may_call_tools` whether it offers a tool to call. `structured`: a response_format answer.
    pub fn new(
        format: &ChatFormat,
        thinking: bool,
        may_call_tools: bool,
        tool_types: Option<Box<dyn ToolTypes>>,
        request_id: &str,
        structured: bool,
    ) -> Self {
        OutputReader {
            splitter: ReasoningSplitter::new(thinking, may_call_tools, Some(format)),
            projector: ToolCallProjector::new(tool_types, request_id, structured, format.call_syntax),
            reasoning: String::new(),
        }
    }

    fn append(&mut self, pieces: Vec<(Field, String)>, out: &mut Vec<Delta>) {
        for (field, text) in pieces {
            match field {
                Field::Content => out.extend(self.projector.put(&text)),
                Field::Reasoning => {
                    self.reasoning.push_str(&text);
                    out.push(Delta::Reasoning(text));
                }
            }
        }
    }

    /// Read decoded text; returns the deltas it completes.
    pub fn put(&mut self, text: &str) -> Vec<Delta> {
        let mut out = Vec::new();
        let pieces = self.splitter.put(text);
        self.append(pieces, &mut out);
        out
    }

    /// Whether the output so far ends inside a tool call.
    pub fn in_call(&self) -> bool {
        self.projector.in_call()
    }

    /// End the answer. `length` is the token limit ending it; `stopped` a stop sequence. Returns the deltas still
    /// owed and the finished answer.
    pub fn finish(mut self, length: bool, stopped: bool) -> (Vec<Delta>, Collected) {
        let mut out = Vec::new();
        let pieces = self.splitter.finish();
        self.append(pieces, &mut out);
        let cut_in_call = self.projector.in_call() && (length || stopped);
        let (content, tool_calls, events) = self.projector.finish(length || cut_in_call);
        out.extend(events);
        let collected = Collected {
            reasoning: self.reasoning,
            content,
            tool_calls,
            reasoning_open: self.splitter.reasoning,
            cut_in_call,
            cut_call: self.projector.call_id().map(str::to_owned),
        };
        (out, collected)
    }
}

/// A response schema's check: the reason a value fails it.
pub type ResponseValidator<'a> = &'a dyn Fn(&Value) -> Result<(), String>;

/// Check a structured answer against its response_format (output.validate_response_content). `validate` is the
/// schema validator the constrain port builds; it returns the reason a value fails. Python's messages quote its own
/// JSON and jsonschema errors, so only the prefix and code match it.
pub fn validate_response_content(content: &str, validate: Option<ResponseValidator>) -> Result<(), super::ChatError> {
    let Some(validate) = validate else { return Ok(()) };
    let invalid = |reason: String| super::ChatError {
        status: 500,
        message: format!("model returned invalid structured output: {reason}"),
        code: "invalid_model_output",
    };
    let value = pyjson::codec_loads(content).map_err(invalid)?;
    validate(&value).map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_partial_tags_by_char() {
        assert_eq!(hold_partial("abc</th", &["</think>"]), ("abc", "</th"));
        assert_eq!(hold_partial("x</think>", &["</think>"]), ("x</think>", ""));
        assert_eq!(hold_partial("żółćgę", &["gęś"]), ("żółć", "gę"));
    }
}

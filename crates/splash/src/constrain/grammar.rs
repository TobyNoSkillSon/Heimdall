//! The llguidance grammars of constrained requests (tool_schema.json_grammar, tool_grammar), byte for byte as Splash
//! writes them, so a grammar compiled here behaves as the Python server's did and shares its cache keys.
use super::lru::LruCache;
use super::pyjson::{dumps, dumps_str, J};
use super::schema::{json_objects, walk};
use super::tools::{
    argument_schemas, is_tag_name, literal_texts, schema_types, schema_with_root, Shape, ToolPolicy, SCHEMA_ANNOTATIONS,
    SCHEMA_IDENTIFIERS,
};
use super::ApiError;
use crate::chat::format::{ChatFormat, ToolCallSyntax, TOOL_CALL_CLOSE, TOOL_CALL_OPEN};
use llguidance::toktrie;
use serde_json::Value;
use std::sync::{Mutex, OnceLock};

/// The whitespace a model chooses between the tokens of constrained output is bounded: a model that prefers
/// whitespace to every token the grammar allows next would otherwise write it until max_tokens, as Qwen models do
/// under speculative decoding (vLLM #38696, #50989). Whitespace inside strings is content and unbounded.
pub const MAX_WHITESPACE: usize = 64;
const WHITESPACE: &str = r"[ \t\n\r]{0,64}";
const WHITESPACE_RULE: &str = r"WS: /[ \t\n\r]{0,64}/";
/// The grammar compiler expands these keywords into work proportional to their values; larger bounds are left to the
/// response check on the complete output.
const GRAMMAR_BOUND_KEYWORDS: [&str; 3] = ["minItems", "maxItems", "multipleOf"];
const MAX_GRAMMAR_BOUND: f64 = 64.0;
/// A client sends the same tools and output schema every turn, so pattern checks are kept.
const PATTERN_CHECK_CACHE_SIZE: usize = 1024;
const PATTERN_CHECK_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// What the grammars write reads back as written: a name neither starts nor ends with the space reading strips and is
/// no longer than reading takes, and raw text does not run past its value's close.
const NAME: &str = r"/[^<> \t\n\r]([^<>\n\r]{0,254}[^<> \t\n\r])?/";
const RAW_VALUE: &str = "/(?s:.*)/";

fn lark_regex(pattern: &str) -> String {
    format!("/{}/", pattern.replace('/', r"\/"))
}

/// Python's `re.escape`: only characters special in a regular expression.
fn re_escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\u{b}\u{c}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Validates a grammar the way `LLMatcher.validate_grammar` does; None when it compiles.
pub(crate) fn grammar_error(grammar: &str, tok_env: Option<toktrie::TokEnv>) -> Option<String> {
    use llguidance::api::{GrammarInit, ParserLimits, TopLevelGrammar};
    match TopLevelGrammar::from_lark_or_grammar_list(grammar) {
        Ok(parsed) => GrammarInit::Serialized(parsed).validate(tok_env, ParserLimits::default()).into_error(),
        Err(error) => Some(error.to_string()),
    }
}

/// Whether the grammar compiler takes a JSON Schema `pattern`: it rejects look-around, word boundaries and
/// backreferences, which a response format checks on the complete output.
fn grammar_takes_pattern(pattern: &str) -> bool {
    static CHECKS: OnceLock<Mutex<LruCache<String, bool>>> = OnceLock::new();
    let checks = CHECKS.get_or_init(|| Mutex::new(LruCache::new(PATTERN_CHECK_CACHE_BYTES, Some(PATTERN_CHECK_CACHE_SIZE))));
    if let Some(known) = checks.lock().unwrap().get(pattern) {
        return *known;
    }
    let string = dumps(&J::obj(vec![("type".into(), J::str("string")), ("pattern".into(), J::str(pattern))]), false);
    let takes = grammar_error(&format!("%llguidance {{}}\nstart: %json {string}\n"), None).is_none();
    // dumps uses ASCII escapes, so its length bounds the pattern's bytes.
    checks.lock().unwrap().put(pattern.to_string(), takes, string.len());
    takes
}

/// The constraints of `schema` the grammar compiler takes, with whitespace between its tokens bounded.
pub(crate) fn grammar_compatible_schema(schema: &J) -> J {
    let mut output = schema.deep_copy();
    walk(&output, &mut |node| {
        if let Some(object) = node.as_obj() {
            let mut object = object.borrow_mut();
            object.remove("propertyNames");
            if let Some(J::Str(pattern)) = object.remove("pattern") {
                if grammar_takes_pattern(&pattern) {
                    object.set("pattern", J::Str(pattern));
                }
            }
        }
        true
    });
    // A local reference can point anywhere in the document, so any object may be compiled as a schema.
    for node in json_objects(&output) {
        let mut object = node.as_obj().unwrap().borrow_mut();
        for key in GRAMMAR_BOUND_KEYWORDS {
            if object.get(key).and_then(J::number).is_some_and(|bound| bound > MAX_GRAMMAR_BOUND) {
                object.remove(key);
            }
        }
    }
    if output.is_bool(true) {
        // Any value, with the whitespace between its tokens bounded.
        output = J::empty_obj();
    }
    if let Some(object) = output.as_obj() {
        let guidance = J::obj(vec![("lenient".into(), J::Bool(true)), ("whitespace_pattern".into(), J::str(WHITESPACE))]);
        object.borrow_mut().set("x-guidance", guidance);
    }
    output
}

/// Tags of `syntax` in a row: each as its token where the family's tags are tokens (Flint's), so text cannot spell
/// them; else their text as one string.
fn tags(syntax: &ToolCallSyntax, parts: &[&str]) -> String {
    if syntax.tags_are_tokens {
        return parts.iter().filter(|t| !t.is_empty()).copied().collect::<Vec<_>>().join(" ");
    }
    dumps_str(&parts.concat())
}

fn function_opening(syntax: &ToolCallSyntax, name: &str) -> String {
    if syntax.tags_are_tokens {
        let quoted = dumps_str(name);
        return [syntax.name_open, quoted.as_str(), syntax.name_close].iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join(" ");
    }
    dumps_str(&syntax.function_opening(name))
}

/// The tags around a parameter's name, up to its value. The name is a lexeme of its own: one spanning
/// "<parameter=url>" would win over another name that starts like it, and lexing cannot back off.
fn key_tags(syntax: &ToolCallSyntax) -> (String, String) {
    (tags(syntax, &[syntax.key_open]), tags(syntax, &[syntax.key_close, syntax.value_open]))
}

fn raw_value(syntax: &ToolCallSyntax, rule: &str) -> Vec<String> {
    if syntax.tags_are_tokens {
        return vec![format!("{rule}: RAW_VALUE {}", syntax.value_close)];
    }
    vec![format!("{rule}[suffix={}]: {RAW_VALUE}", dumps_str(syntax.value_close))]
}

fn value_terminals(syntax: &ToolCallSyntax) -> Vec<String> {
    if !syntax.tags_are_tokens {
        return Vec::new();
    }
    let spelled = lark_regex(&format!("(?s:.*){}(?s:.*)", re_escape(syntax.value_close)));
    vec![format!("RAW_VALUE: {RAW_VALUE} & ~{spelled}")]
}

/// The arguments of a tool that is not strict: parameters in the template's tags, any name, raw text.
fn free_arguments(syntax: &ToolCallSyntax) -> String {
    let (key_open, key_close) = key_tags(syntax);
    let mut rules = vec!["%llguidance {}".to_string(), "start: parameter*".into(), format!("parameter: {key_open} NAME {key_close} value")];
    rules.extend(raw_value(syntax, "value"));
    rules.push(format!("NAME: {NAME}"));
    rules.extend(value_terminals(syntax));
    rules.join("\n") + "\n"
}

/// The members of a schema that is only a union of them, else the schema itself (flagged true).
fn union_members(schema: &J) -> Vec<(J, bool)> {
    if let Some(object) = schema.as_obj() {
        let members = schema.get("anyOf").or_else(|| schema.get("oneOf"));
        let only_union = object
            .borrow()
            .keys()
            .filter(|k| !SCHEMA_ANNOTATIONS.contains(k) && !SCHEMA_IDENTIFIERS.contains(k))
            .all(|k| k == "anyOf" || k == "oneOf");
        if let Some(members) = members.as_ref().and_then(J::as_arr) {
            let members = members.borrow();
            if !members.is_empty() && only_union {
                return members.iter().map(|m| (m.clone(), m.is(schema))).collect();
            }
        }
    }
    vec![(schema.clone(), true)]
}

/// The rules of a strict tool's parameter: raw text when it may be any string, else one of the values each union
/// member lists, or JSON of a member that lists none; None when no value is possible. Framing made the parameter
/// schema self-contained, so it is its own reference root.
fn parameter_rules(rule: &str, prefix: &str, value_schema: &J, syntax: &ToolCallSyntax) -> Result<Option<Vec<String>>, ApiError> {
    let closing = tags(syntax, &[syntax.value_close]);
    if value_schema.is_bool(false) {
        return Ok(None);
    }
    let mut rules: Vec<String> = Vec::new();
    let mut choices: Vec<String> = Vec::new();
    for (member, whole) in union_members(value_schema) {
        let Some(texts) = literal_texts(&member, value_schema)? else {
            let types = schema_types(&member, value_schema)?;
            if types.is_none_or(|t| t.contains("string")) {
                // Any text may be the value; the listed ones are among it.
                let mut out = vec![format!("{rule}: {prefix} {rule}_value")];
                out.extend(raw_value(syntax, &format!("{rule}_value")));
                return Ok(Some(out));
            }
            // A member refers to the parameter schema's definitions.
            let member = if whole { member } else { schema_with_root(&member, value_schema).0 };
            choices.push(format!("%json {}", dumps(&grammar_compatible_schema(&member), true)));
            continue;
        };
        for text in texts {
            if text.contains(syntax.value_close) {
                return Err(ApiError::bad_request("tool parameter enum contains XML framing"));
            }
            if text.is_empty() {
                rules.push(format!("{rule}_empty_{}:", rules.len()));
                choices.push(format!("{rule}_empty_{}", rules.len() - 1));
            } else {
                choices.push(dumps_str(&text));
            }
        }
    }
    if choices.is_empty() {
        return Ok(None);
    }
    let mut seen = std::collections::HashSet::new();
    choices.retain(|c| seen.insert(c.clone()));
    rules.push(format!("{rule}: {prefix} ({}) {closing}", choices.join(" | ")));
    Ok(Some(rules))
}

/// The arguments of a strict tool: declared parameters in schema order, required ones present, then any others.
fn argument_grammar(schema: &Shape, syntax: &ToolCallSyntax) -> Result<String, ApiError> {
    let (key_open, key_close) = key_tags(syntax);
    let mut rules = Vec::new();
    let mut sequence = Vec::new();
    for (index, (name, value_schema)) in schema.properties.iter().enumerate() {
        let rule = format!("parameter_{index}");
        let prefix = format!("{key_open} {} {key_close}", dumps_str(name));
        if !value_schema.is_bool(false) && !is_tag_name(name) {
            return Err(ApiError::bad_request("invalid tool parameter name"));
        }
        let required = schema.required.contains(name);
        let Some(parameter) = parameter_rules(&rule, &prefix, value_schema, syntax)? else {
            if required {
                return Err(ApiError::bad_request(format!("required tool parameter cannot have a value: {name}")));
            }
            continue;
        };
        rules.extend(parameter);
        sequence.push(if required { rule } else { format!("{rule}?") });
    }
    if let Some(extra) = parameter_rules("extra", &format!("{key_open} EXTRA_NAME {key_close}"), &schema.additional, syntax)? {
        // Other names are those the tool does not declare.
        let declared: Vec<String> = schema.properties.iter().map(|(n, _)| dumps_str(n)).collect();
        let exclusion = if declared.is_empty() { String::new() } else { format!(" & ~({})", declared.join(" | ")) };
        rules.push(format!("EXTRA_NAME: {NAME}{exclusion}"));
        rules.extend(extra);
        sequence.push("extra*".into());
    }
    rules.extend(value_terminals(syntax));
    let start: String = sequence.iter().map(|item| format!(" {item}")).collect();
    Ok(format!("%llguidance {{}}\nstart:{start}\n{}\n", rules.join("\n")))
}

/// The reasoning a grammar lets come before its answer: where the generation prompt opened a think block, its text up
/// to the close; where the model opens one itself, an optional block; otherwise none. Each tag is its token.
fn reasoning(thinking: bool, chat_format: &ChatFormat) -> (&'static str, Option<String>) {
    let close = format!("<[{}]>", chat_format.think_end_id);
    if thinking {
        return ("think ", Some(format!("think: TEXT {close}")));
    }
    if let Some(start) = chat_format.think_start_id {
        return ("think? ", Some(format!("think: <[{start}]> TEXT {close}")));
    }
    ("", None)
}

fn schema_json(schema: &Value) -> String {
    dumps(&grammar_compatible_schema(&J::from_value(schema)), true)
}

/// The output grammar of a request with a response schema and no constrained tools: the reasoning the format lets
/// come first, then the JSON.
pub fn json_grammar(schema: &Value, thinking: bool, chat_format: &ChatFormat) -> String {
    let (think, think_rule) = reasoning(thinking, chat_format);
    let mut grammar = vec!["%llguidance {}".to_string(), format!("start: {think}WS %json {}", schema_json(schema))];
    if let Some(rule) = think_rule {
        grammar.push(rule);
        grammar.push(r"TEXT: /(?s:.*)/ & ~/(?s:.*)<\/think>(?s:.*)/".into());
    }
    grammar.push(WHITESPACE_RULE.into());
    grammar.join("\n") + "\n"
}

/// The output grammar of a request whose tool calls are constrained, or whose tools stand beside a response schema:
/// after the reasoning, each call names an offered tool in the template's layout, a strict tool's arguments generated
/// to its schema and any other's free. A required choice starts with a call and a named one is that call; under auto,
/// text may come first. Parallel calls may be followed by text and further calls, a single call by whitespace only.
/// Beside an auto choice, a response schema admits its JSON answer instead of the calls.
pub fn tool_grammar(policy: &ToolPolicy, thinking: bool, response_schema: Option<&Value>, chat_format: &ChatFormat) -> Result<String, ApiError> {
    let syntax = &chat_format.call_syntax;
    let mut strict_arguments = Vec::new();
    for (name, shape) in argument_schemas(policy)? {
        strict_arguments.push((name, argument_grammar(&shape, syntax)?));
    }
    let mut side_grammars: Vec<(String, String)> = Vec::new();
    if strict_arguments.len() < policy.schemas.len() {
        side_grammars.push(("free_arguments".into(), free_arguments(syntax)));
    }
    let closing = if syntax.arguments_close.is_empty() { String::new() } else { format!(" {}", tags(syntax, &[syntax.arguments_close])) };
    let mut calls = Vec::new();
    for (index, (name, _)) in policy.schemas.iter().enumerate() {
        let mut arguments = "free_arguments".to_string();
        if let Some((_, grammar)) = strict_arguments.iter().find(|(n, _)| n == name) {
            arguments = format!("arguments_{index}");
            side_grammars.push((arguments.clone(), grammar.clone()));
        }
        calls.push(format!("call_{index}: {TOOL_CALL_OPEN} {} @{arguments}{closing} {TOOL_CALL_CLOSE}", function_opening(syntax, name)));
    }
    let call = format!("({})", (0..calls.len()).map(|i| format!("call_{i}")).collect::<Vec<_>>().join(" | "));
    let body = if calls.is_empty() {
        // tool_choice none: neither the text nor a JSON answer starts a call.
        if response_schema.is_none() { "TEXT".to_string() } else { "answer".to_string() }
    } else if policy.required || response_schema.is_some() {
        // A call comes first, set apart from the reasoning by whitespace.
        let body = format!("WS {call} {}", if policy.parallel { format!("(TEXT {call})* TEXT") } else { "WS".into() });
        if policy.required { body } else { format!("(({body}) | answer)") }
    } else if policy.parallel {
        format!("(TEXT {call})* TEXT")
    } else {
        format!("TEXT ({call} WS)?")
    };
    let (think, think_rule) = reasoning(thinking, chat_format);
    let mut main = vec!["%llguidance {}".to_string(), format!("start: {think}{body}")];
    if let Some(schema) = response_schema.filter(|_| !policy.required) {
        main.push(format!("answer: WS %json {} WS", schema_json(schema)));
    }
    main.extend(think_rule);
    main.extend(calls);
    main.push(WHITESPACE_RULE.into());
    main.push(r"TEXT: /(?s:.*)/ & ~/(?s:.*)(<tool_call>|<\/think>)(?s:.*)/".into());
    side_grammars.insert(0, ("tool_output".into(), main.join("\n") + "\n"));
    let grammars: Vec<J> = side_grammars
        .into_iter()
        .map(|(name, grammar)| J::obj(vec![("name".into(), J::str(&name)), ("lark_grammar".into(), J::str(&grammar))]))
        .collect();
    Ok(dumps(&J::obj(vec![("grammars".into(), J::arr(grammars))]), true))
}

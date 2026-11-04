//! Constrained output through llguidance (constraints.py, tool_schema.py, schema_validation.py). OWNER: constraint worker.
//!
//! A request's tools and response_format are normalized here (`normalize_tools`, `normalize_response_format`), which
//! refuses what Splash refuses with the same 400 messages; `request_constraint` then decides, as frontend._prepare
//! does, whether the output generates under a grammar and builds that grammar's `TokenConstraint`, the engine's
//! `MaskProvider`. The grammars are byte-identical to Splash 1.4.1's (golden tests in tests/constrain_grammar.rs).
mod grammar;
mod lru;
mod matcher;
mod pyjson;
mod schema;
mod tools;
mod validate;

pub use grammar::{json_grammar, tool_grammar, MAX_WHITESPACE};
pub use lru::LruCache;
pub use matcher::{ConstraintError, ConstraintFactory, GrammarCacheStats, Prefixes, TokenConstraint};
pub use tools::{
    is_tag_name, normalize_tools, JsonTypes, NormalizedTools, ParameterTypes, ToolPolicy, ToolTypes, JSON_TYPE_NAMES,
    MAX_FRAMED_SCHEMA_BYTES, MAX_NAME_LENGTH, TAG_NAME_RULE,
};
pub use validate::{normalize_response_format, validate_response_content, ResponseFormat};

use crate::chat::format::{ChatFormat, TOOL_CALL_OPEN};
use std::time::Duration;

/// A refusal the HTTP layer returns as is (errors.APIError).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub code: &'static str,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError { status: 400, message: message.into(), code: "invalid_request_error" }
    }
}

/// Whether a request generates under a grammar: constrained tool calls (including tool_choice none) or a response
/// schema, tools beside a response schema sharing one. The server refuses ignore_eos for these before rendering.
pub fn is_constrained(policy: Option<&ToolPolicy>, response: Option<&ResponseFormat>) -> bool {
    tool_constrained(policy, response) || response.is_some()
}

fn tool_constrained(policy: Option<&ToolPolicy>, response: Option<&ResponseFormat>) -> bool {
    policy.is_some_and(|p| p.constrained || response.is_some())
}

/// The grammar source of a request, None when it is not constrained; `tool_calls` says whether it is a tool grammar,
/// whose call openings are checked when it compiles.
pub fn output_grammar(
    policy: Option<&ToolPolicy>,
    response: Option<&ResponseFormat>,
    thinking: bool,
    chat_format: &ChatFormat,
) -> Result<Option<(String, bool)>, ApiError> {
    let schema = response.map(|r| &r.schema);
    if tool_constrained(policy, response) {
        let policy = policy.expect("tool constrained");
        return Ok(Some((tool_grammar(policy, thinking, schema, chat_format)?, true)));
    }
    Ok(schema.map(|schema| (json_grammar(schema, thinking, chat_format), false)))
}

/// The tokens that begin each strict tool's call, after the close of the reasoning the generation prompt opened, with
/// the error for a tool whose parameter names the parser cannot admit there (frontend._call_openings). `encode`
/// tokenizes text without special tokens, as the chat template's tokenizer does.
pub fn call_openings(
    policy: &ToolPolicy,
    thinking: bool,
    chat_format: &ChatFormat,
    encode: &dyn Fn(&str) -> Vec<u32>,
) -> Vec<(Vec<u32>, String)> {
    policy
        .strict_names()
        .map(|name| {
            let mut tokens = if thinking { vec![chat_format.think_end_id] } else { Vec::new() };
            tokens.extend(encode(&format!("{TOOL_CALL_OPEN}{}", chat_format.call_syntax.function_opening(name))));
            (tokens, format!("tool {name} has too many parameters to constrain"))
        })
        .collect()
}

impl ConstraintFactory {
    /// The constraint of a prepared request, or None when it generates freely (frontend._prepare). `thinking` is
    /// whether the rendered prompt opened a think block; `timeout` is what remains of the request's deadline.
    pub fn request_constraint(
        &self,
        policy: Option<&ToolPolicy>,
        response: Option<&ResponseFormat>,
        thinking: bool,
        timeout: Option<Duration>,
        encode: &dyn Fn(&str) -> Vec<u32>,
    ) -> Result<Option<TokenConstraint>, ApiError> {
        let chat_format = *self.chat_format();
        let Some((grammar, tool_calls)) = output_grammar(policy, response, thinking, &chat_format)? else {
            return Ok(None);
        };
        let openings = || call_openings(policy.expect("a tool grammar has a policy"), thinking, &chat_format, encode);
        let prefixes: Option<Prefixes> = if tool_calls { Some(&openings) } else { None };
        self.create(&grammar, timeout, prefixes).map(Some)
    }
}

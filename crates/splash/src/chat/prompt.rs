//! A chat-completions request's prompt: its template options and messages (frontend._prepare_prompt), rendered and
//! encoded with the generation prompt checked (frontend._render_prompt, _generation_prompt).

use super::format::{THINK_END, THINK_START};
use super::messages::{normalize_messages, template_messages, FileParts};
use super::template::{template_options, ChatTemplates, LATER_SYSTEM_UNSUPPORTED, REASONING_EFFORTS, RESERVED_TEMPLATE_KWARGS};
use super::tokens::PromptTokenizer;
use super::ChatError;
use serde_json::{Map, Value};

/// What a request asks of the template, before tools (tool_schema.normalize_tools is the constrain port's).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PromptRequest {
    pub messages: Vec<Value>,
    /// The effort the prompt renders with: the request's, else the server default.
    pub reasoning_effort: Option<String>,
    pub preserve_thinking: Option<bool>,
    /// chat_template_kwargs' enable_thinking, which outranks the effort.
    pub enable_thinking: Option<bool>,
    /// The request's other template variables, which outrank Splash's own.
    pub template_kwargs: Map<String, Value>,
    /// The effort the request sent; None where it sent none.
    pub sent_effort: Option<String>,
}

impl PromptRequest {
    /// Read and check a chat-completions body's prompt fields in Python's order: reasoning_effort,
    /// preserve_thinking, chat_template_kwargs, enable_thinking, then messages. `files` renders PDF parts.
    pub fn from_body(
        body: &Value,
        default_effort: Option<&str>,
        vision: bool,
        files: FileParts,
    ) -> Result<Self, ChatError> {
        let sent = body.get("reasoning_effort").filter(|v| !v.is_null());
        let effort = match sent {
            Some(Value::String(e)) if REASONING_EFFORTS.contains(&e.as_str()) => Some(e.clone()),
            Some(_) => return Err(ChatError::bad_request("invalid reasoning_effort")),
            None => default_effort.map(str::to_owned),
        };
        let preserve_thinking = match body.get("preserve_thinking") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(ChatError::bad_request("preserve_thinking must be a boolean")),
        };
        let mut template_kwargs = match body.get("chat_template_kwargs") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(kwargs)) => {
                let mut reserved: Vec<&str> =
                    RESERVED_TEMPLATE_KWARGS.iter().copied().filter(|k| kwargs.contains_key(*k)).collect();
                reserved.sort_unstable();
                if let Some(first) = reserved.first() {
                    return Err(ChatError::bad_request(format!("chat_template_kwargs cannot set {first}")));
                }
                kwargs.clone()
            }
            Some(_) => return Err(ChatError::bad_request("chat_template_kwargs must be an object")),
        };
        let enable_thinking = match template_kwargs.shift_remove("enable_thinking") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(b)) => Some(b),
            Some(_) => return Err(ChatError::bad_request("chat_template_kwargs enable_thinking must be a boolean")),
        };
        let messages = template_messages(normalize_messages(body.get("messages"), vision, files)?);
        Ok(PromptRequest {
            messages,
            reasoning_effort: effort,
            preserve_thinking,
            enable_thinking,
            template_kwargs,
            sent_effort: sent.and_then(Value::as_str).map(str::to_owned),
        })
    }

    /// The full template variables the prompt renders with (template_options, then the request's kwargs).
    pub fn template_variables(&self, tools: Option<&[Value]>, add_generation_prompt: bool) -> Map<String, Value> {
        let mut options = template_options(
            self.reasoning_effort.as_deref(),
            self.preserve_thinking,
            tools,
            add_generation_prompt,
            self.enable_thinking,
        );
        for (key, value) in &self.template_kwargs {
            options.insert(key.clone(), value.clone());
        }
        options
    }
}

/// The request's thinking_token_budget: None for no cap (-1), the server default when absent or null.
pub fn thinking_token_budget(body: &Value, default: Option<u64>) -> Result<Option<u64>, ChatError> {
    match body.get("thinking_token_budget") {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) if n.as_i64() == Some(-1) => Ok(None),
        Some(Value::Number(n)) if n.as_u64().is_some() => Ok(n.as_u64()),
        Some(_) => Err(ChatError::bad_request("thinking_token_budget must be a non-negative integer or -1 for no cap")),
    }
}

/// The budget the engine enforces: only for a request that thinks, and only below its output budget.
pub fn effective_thinking_budget(budget: Option<u64>, thinking: bool, max_new: u64) -> Option<u64> {
    budget.filter(|b| thinking && *b < max_new)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub text: String,
    pub tokens: Vec<u32>,
    /// Whether the generation prompt opens a think block.
    pub thinking: bool,
    /// How many of the prompt's last tokens are the generation prompt (0 where they differ from its tokens).
    pub generation_prompt_tokens: usize,
}

/// Render and encode a request's prompt (frontend._render_prompt without images: a prompt with image parts renders
/// the template's `<|image_pad|>` text, which the images port expands).
pub fn render_prompt(
    templates: &ChatTemplates,
    tokenizer: &PromptTokenizer,
    request: &PromptRequest,
    tools: Option<&[Value]>,
    add_generation_prompt: bool,
) -> Result<RenderedPrompt, ChatError> {
    let template = templates.select(tools);
    if !template.accepts(&request.messages) {
        return Err(ChatError::bad_request(LATER_SYSTEM_UNSUPPORTED));
    }
    let options = request.template_variables(tools, add_generation_prompt);
    let text = template.render(&request.messages, &options).map_err(|error| ChatError::bad_request(error.client_message()))?;
    let tokens = tokenizer.encode(&text);
    let (mut thinking, mut generation_prompt_tokens) = (false, 0);
    if add_generation_prompt {
        let probed = template.generation_prompt(&options);
        (thinking, generation_prompt_tokens) = generation_prompt(probed.as_ref().map(|p| (p.text.as_str(), p.tokens.as_slice())), &text, &tokens)?;
    }
    Ok(RenderedPrompt { text, tokens, thinking, generation_prompt_tokens })
}

/// Whether the generation prompt opens a think block, and how many of the prompt's last tokens it is.
pub fn generation_prompt(probed: Option<(&str, &[u32])>, rendered: &str, tokens: &[u32]) -> Result<(bool, usize), ChatError> {
    let Some((text, ids)) = probed.filter(|(text, _)| !text.is_empty() && rendered.ends_with(text)) else {
        return Err(ChatError::bad_request("chat template must end with an assistant generation prefix"));
    };
    let position = |tag: &str| text.rfind(tag).map_or(-1, |i| i as i64);
    let thinking = position(THINK_START) > position(THINK_END);
    let count = ids.len();
    let matches = count < tokens.len() && tokens[tokens.len() - count..] == *ids;
    Ok((thinking, if matches { count } else { 0 }))
}

//! Chat formatting, tokenization and output reading. `format` is shared; the chat worker owns the rest.
//!
//! A request's way in: `prompt::PromptRequest::from_body` (messages and template options), the tools from the
//! constrain port, then `prompt::render_prompt` over `template::ChatTemplates` and `tokens::PromptTokenizer`. Its
//! way out: `tokens::Streamer` turns engine tokens into text, and `output::OutputReader` turns text into reasoning,
//! content and tool-call deltas.
pub mod format;
mod later_system;
pub mod messages;
pub mod output;
pub mod prompt;
pub mod pyjson;
pub mod template;
pub mod tokens;

/// The template's placeholder for an image, which the images port expands.
pub const IMAGE_PAD_TOKEN: &str = "<|image_pad|>";

/// A request the chat layer refuses, as the API reports it (errors.APIError).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ChatError {
    pub status: u16,
    pub message: String,
    pub code: &'static str,
}

impl ChatError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        ChatError { status: 400, message: message.into(), code: "invalid_request_error" }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ChatError { status: 500, message: message.into(), code: "internal_error" }
    }
}

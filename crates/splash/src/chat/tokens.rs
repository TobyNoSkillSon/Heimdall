//! The tokenizer: prompt encoding (tokenization.py), the startup checks (constraints.validate_tokenizer,
//! backend.thinking_end_tokens) and the streaming detokenizer (backend.CallbackStreamer).
//!
//! Prompts are encoded whole. Python's PromptTokenizer reuses a cached prefix's tokens at the last message boundary
//! only where the tokenizer splits its input there, so its tokens always equal a whole encoding; the cache is a
//! speed-up this port leaves out (the golden tests compare whole-prompt tokens against it).

use super::format::{ChatFormat, THINK_END, THINK_START, TOOL_CALL_OPEN};
use super::output::hold_partial;
use super::template::Encoder;
use super::ChatError;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tokenizers::Tokenizer;

/// The most tokens a thinking budget's forced end may take (protocol.MAX_THINKING_END_TOKENS).
pub const MAX_THINKING_END_TOKENS: usize = 64;

pub struct PromptTokenizer {
    tokenizer: Tokenizer,
    eos_token: Option<String>,
}

impl PromptTokenizer {
    /// Load `tokenizer.json`, with the EOS token tokenizer_config.json names, from a prepared model's tokenizer
    /// directory.
    pub fn from_dir(dir: &Path) -> anyhow::Result<Self> {
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| anyhow::anyhow!("tokenizer.json: {e}"))?;
        let config: Value = match std::fs::read_to_string(dir.join("tokenizer_config.json")) {
            Ok(text) => serde_json::from_str(&text)?,
            Err(_) => Value::Null,
        };
        let eos_token = match config.get("eos_token") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Object(o)) => o.get("content").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        };
        Ok(PromptTokenizer { tokenizer, eos_token })
    }

    pub fn new(tokenizer: Tokenizer, eos_token: Option<String>) -> Self {
        PromptTokenizer { tokenizer, eos_token }
    }

    pub fn tokenizer(&self) -> &Tokenizer {
        &self.tokenizer
    }

    /// `tokenizer(tokenizable(text), add_special_tokens=False)`. Rust text holds no surrogates (request JSON goes
    /// through `pyjson::replace_lone_surrogates`), so tokenizable() has nothing left to do. A byte-level BPE encodes
    /// any text; a failure (a broken tokenizer) yields no tokens.
    pub fn encode(&self, text: &str) -> Vec<u32> {
        self.tokenizer.encode(text, false).map(|e| e.get_ids().to_vec()).unwrap_or_default()
    }

    /// Token ids with each token's byte span in `text` (for placing image pads).
    pub fn encode_with_offsets(&self, text: &str) -> (Vec<u32>, Vec<(usize, usize)>) {
        match self.tokenizer.encode(text, false) {
            Ok(e) => (e.get_ids().to_vec(), e.get_offsets().to_vec()),
            Err(_) => (Vec::new(), Vec::new()),
        }
    }

    pub fn decode(&self, ids: &[u32], skip_special_tokens: bool) -> String {
        self.tokenizer.decode(ids, skip_special_tokens).unwrap_or_default()
    }

    pub fn token_to_id(&self, token: &str) -> Option<u32> {
        self.tokenizer.token_to_id(token)
    }

    pub fn eos_token_id(&self) -> Option<u32> {
        self.eos_token.as_deref().and_then(|t| self.token_to_id(t))
    }

    /// An encoder for the chat templates' generation-prompt probe.
    pub fn encoder(self: &Arc<Self>) -> Encoder {
        let this = self.clone();
        Arc::new(move |text: &str| this.encode(text))
    }

    /// Refuse a tokenizer that does not encode the chat format's tokens as the engine has them
    /// (constraints.validate_tokenizer); the message is the startup failure's.
    pub fn validate(&self, format: &ChatFormat) -> Result<(), String> {
        let vocabulary = self.tokenizer.get_vocab(true);
        if vocabulary.is_empty() || vocabulary.values().any(|id| *id >= format.vocabulary) {
            return Err("tokenizer vocabulary does not fit the native model".into());
        }
        let mut expected: Vec<(&str, u32)> = format.stop_tokens.to_vec();
        expected.push((THINK_END, format.think_end_id));
        expected.push((TOOL_CALL_OPEN, format.call_open_id));
        if let Some(start) = format.think_start_id {
            expected.push((THINK_START, start));
        }
        for (token, id) in expected {
            if vocabulary.get(token) != Some(&id) || self.encode(token) != [id] {
                return Err(format!("tokenizer must encode {} as native token {id}", py_repr(token)));
            }
        }
        if !self.eos_token_id().is_some_and(|eos| format.stop_ids().contains(&eos)) {
            return Err("tokenizer EOS token does not match the native model".into());
        }
        Ok(())
    }

    /// The tokens the engine selects where a thinking budget runs out (backend.thinking_end_tokens): the text, which
    /// must encode as text then the close of thinking as its own token. None without a text.
    pub fn thinking_end_tokens(&self, text: Option<&str>, format: &ChatFormat) -> Result<Option<Vec<u32>>, String> {
        let Some(text) = text else { return Ok(None) };
        let tokens = self.encode(text);
        let added = self.tokenizer.get_added_tokens_decoder();
        let valid = tokens.last() == Some(&format.think_end_id)
            && !tokens[..tokens.len() - 1].iter().any(|t| added.contains_key(t));
        if !valid {
            return Err(format!("--thinking-end-text must end with {THINK_END} and hold no other special token"));
        }
        if tokens.len() > MAX_THINKING_END_TOKENS {
            return Err(format!(
                "--thinking-end-text takes {} tokens; the most is {MAX_THINKING_END_TOKENS}",
                tokens.len()
            ));
        }
        Ok(Some(tokens))
    }

    /// The tokens that begin a strict tool's call after the reasoning the generation prompt opened
    /// (frontend._call_openings), for the constraint's prefixes.
    pub fn call_opening(&self, format: &ChatFormat, name: &str, thinking: bool) -> Vec<u32> {
        let mut tokens = if thinking { vec![format.think_end_id] } else { Vec::new() };
        tokens.extend(self.encode(&format!("{TOOL_CALL_OPEN}{}", format.call_syntax.function_opening(name))));
        tokens
    }
}

/// Python's `repr()` of a tag, as validate_tokenizer's message quotes it.
fn py_repr(text: &str) -> String {
    super::pyjson::str_repr(text)
}

/// The model's output as text, one piece per decoded step, stopped at the first stop sequence (CallbackStreamer).
///
/// Pieces come back from `put_tokens` and `end` instead of through a callback. A piece that may begin a stop
/// sequence waits until the next token shows whether it does.
pub struct Streamer {
    tokenizer: Arc<PromptTokenizer>,
    stop_sequences: Vec<String>,
    /// tokenizers' DecodeStream state (skip_special_tokens); None once it cannot continue.
    decode: Option<(Vec<u32>, String, usize)>,
    token_ids: Vec<u32>,
    emitted: String,
    pending_text: String,
    stop_sequence: Option<String>,
}

impl Streamer {
    pub fn new(tokenizer: Arc<PromptTokenizer>, stop_sequences: Vec<String>) -> Self {
        Streamer {
            tokenizer,
            stop_sequences,
            decode: Some((Vec::new(), String::new(), 0)),
            token_ids: Vec::new(),
            emitted: String::new(),
            pending_text: String::new(),
            stop_sequence: None,
        }
    }

    /// The stop sequence that ended the output, once one has: the request should then be cancelled.
    pub fn stop_sequence(&self) -> Option<&str> {
        self.stop_sequence.as_deref()
    }

    pub fn token_ids(&self) -> &[u32] {
        &self.token_ids
    }

    fn send(&mut self, text: String, out: &mut Vec<String>) {
        if !text.is_empty() {
            self.emitted.push_str(&text);
            out.push(text);
        }
    }

    fn emit(&mut self, text: &str, out: &mut Vec<String>) {
        if text.is_empty() || self.stop_sequence.is_some() {
            return;
        }
        if self.stop_sequences.is_empty() {
            self.send(text.to_owned(), out);
            return;
        }
        self.pending_text.push_str(text);
        let earliest = self
            .stop_sequences
            .iter()
            .filter_map(|stop| self.pending_text.find(stop.as_str()).map(|at| (at, stop)))
            .min_by_key(|(at, _)| *at);
        if let Some((at, stop)) = earliest {
            let stop = stop.clone();
            let ready = self.pending_text[..at].to_owned();
            self.pending_text.clear();
            self.stop_sequence = Some(stop);
            self.send(ready, out);
            return;
        }
        let retained = self
            .stop_sequences
            .iter()
            .map(|stop| hold_partial(&self.pending_text, &[stop.as_str()]).1.len())
            .max()
            .unwrap_or(0);
        let split = self.pending_text.len() - retained;
        let ready = self.pending_text[..split].to_owned();
        self.pending_text.drain(..split);
        self.send(ready, out);
    }

    /// Decode generated tokens; returns the text pieces ready to publish.
    pub fn put_tokens(&mut self, ids: &[u32]) -> Result<Vec<String>, ChatError> {
        let mut out = Vec::new();
        if self.stop_sequence.is_some() {
            return Ok(out);
        }
        for &id in ids {
            self.token_ids.push(id);
            let Some((state_ids, prefix, prefix_index)) = self.decode.as_mut() else { continue };
            let step = tokenizers::tokenizer::step_decode_stream(
                &self.tokenizer.tokenizer,
                vec![id],
                true,
                state_ids,
                prefix,
                prefix_index,
            );
            match step {
                Ok(Some(text)) => {
                    self.emit(&text, &mut out);
                    if self.stop_sequence.is_some() {
                        break;
                    }
                }
                Ok(None) => {}
                // The incremental decode cannot continue; end() decodes the whole sequence.
                Err(e) if e.to_string().starts_with("Invalid prefix encountered") => self.decode = None,
                Err(e) => return Err(ChatError::internal(format!("detokenizer: {e}"))),
            }
        }
        Ok(out)
    }

    /// The rest of the output once generation is done: what the whole decode holds beyond what streamed, without
    /// a trailing U+FFFD (bytes of a character the output ends inside), then any held text.
    pub fn end(&mut self) -> Result<Vec<String>, ChatError> {
        let mut out = Vec::new();
        if self.stop_sequence.is_some() {
            return Ok(out);
        }
        let decoded = self.tokenizer.decode(&self.token_ids, true);
        let handled_len = self.emitted.len() + self.pending_text.len();
        if !(decoded.starts_with(self.emitted.as_str()) && decoded[self.emitted.len()..].starts_with(self.pending_text.as_str())) {
            return Err(ChatError::internal("incremental tokenizer output diverged"));
        }
        let rest = decoded[handled_len..].trim_end_matches('\u{fffd}').to_owned();
        self.emit(&rest, &mut out);
        if self.stop_sequence.is_none() {
            let pending = std::mem::take(&mut self.pending_text);
            self.send(pending, &mut out);
        }
        Ok(out)
    }

    /// The reasoning's tokens as ReasoningSplitter reads the text: from the start where the generation prompt opened
    /// the reasoning, or after the model's own opening, to its close or, where a call's opening ends it, a call's
    /// opening. Zero without a chat format.
    pub fn count_reasoning_tokens(&self, format: Option<&ChatFormat>, thinking: bool, tool_calls: bool) -> usize {
        let Some(format) = format else { return 0 };
        let start = if thinking {
            0
        } else if format.think_start_id.is_some() && self.token_ids.first().copied() == format.think_start_id {
            1
        } else {
            return 0;
        };
        let calls_end = tool_calls && format.calls_end_reasoning();
        let call_open = format.call_syntax.call_open();
        for index in start..self.token_ids.len() {
            let token = self.token_ids[index];
            if token == format.think_end_id
                || (calls_end && token == format.call_open_id && self.opens_call(index, &call_open))
            {
                return index - start;
            }
        }
        self.token_ids.len() - start
    }

    /// Whether the text from the call-open token at `index` on begins `call_open`, decoded from as few tokens as
    /// decide it.
    fn opens_call(&self, index: usize, call_open: &str) -> bool {
        let mut text = String::new();
        let open_chars = call_open.chars().count();
        for end in index + 1..=self.token_ids.len() {
            text = self.tokenizer.decode(&self.token_ids[index..end], false);
            if text.chars().count() >= open_chars || !call_open.starts_with(text.as_str()) {
                break;
            }
        }
        text.starts_with(call_open)
    }
}

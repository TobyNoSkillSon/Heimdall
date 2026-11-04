//! A chat-completions request from body to engine submission, and the engine's events back to the answer
//! (frontend.Frontend.prepare/_prepare, backend.NativeBackend, the collect loop of server.FrontendHandler).
use super::request::{generation_options, output_budget, request_timeout};
use super::shapes::{ApiError, CacheInfo, NativeResult};
use crate::chat::format::ChatFormat;
use crate::chat::output::{Delta, OutputReader, ToolCall, ToolParameterTypes, ToolTypes as ChatToolTypes};
use crate::chat::prompt::{generation_prompt, render_prompt, PromptRequest};
use crate::chat::template::{ChatTemplates, LATER_SYSTEM_UNSUPPORTED};
use crate::chat::tokens::{PromptTokenizer, Streamer};
use crate::chat::{ChatError, IMAGE_PAD_TOKEN};
use crate::constrain::{self, ConstraintFactory, ParameterTypes, ResponseFormat, ToolPolicy};
use crate::engine::{Engine, EngineError, FailureKind, Request, RequestEvent, Submission};
use crate::images::{self, ImageCache, ImageError};
use crate::protocol::{FinishReason, ImageSpan, ReadyEvent, ThinkingBudget};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

impl From<ChatError> for ApiError {
    fn from(e: ChatError) -> Self {
        ApiError::new(e.status, e.message, e.code)
    }
}
impl From<constrain::ApiError> for ApiError {
    fn from(e: constrain::ApiError) -> Self {
        ApiError::new(e.status, e.message, e.code)
    }
}
impl From<ImageError> for ApiError {
    fn from(e: ImageError) -> Self {
        match e {
            ImageError::Invalid(m) => ApiError::bad(m),
            ImageError::Capacity(m) => ApiError::new(503, m, "image_capacity_exhausted"),
            ImageError::ContextLength { .. } => ApiError::context_length(),
            ImageError::Timeout => ApiError::timeout(),
        }
    }
}

/// The engine serving now: a relaunch after a crash replaces it while requests keep the one they started on.
pub struct EngineSlot(std::sync::RwLock<Arc<Engine>>);

impl EngineSlot {
    pub fn new(engine: Engine) -> Self {
        EngineSlot(std::sync::RwLock::new(Arc::new(engine)))
    }
    pub fn get(&self) -> Arc<Engine> {
        self.0.read().expect("engine slot").clone()
    }
    /// Serve `engine` from now on; returns the one it replaces.
    pub fn replace(&self, engine: Engine) -> Arc<Engine> {
        std::mem::replace(&mut *self.0.write().expect("engine slot"), Arc::new(engine))
    }
}

/// What the server serves and how (frontend.Frontend's fields).
pub struct App {
    pub engine: EngineSlot,
    pub tokenizer: Arc<PromptTokenizer>,
    pub templates: ChatTemplates,
    pub format: ChatFormat,
    pub constraints: ConstraintFactory,
    pub images: ImageCache,
    pub ready: ReadyEvent,
    /// The model id the engine loaded (the selector); /status reports it.
    pub model: String,
    /// The name responses report, and every name a request may use, that one first.
    pub response_model: String,
    pub model_names: Vec<String>,
    pub default_reasoning_effort: Option<String>,
    pub default_thinking_budget: Option<u32>,
    pub request_timeout: Option<Duration>,
    pub max_image_pixels: u32,
    pub started_at: f64,
}

/// A rendered prompt: its tokens, whether its generation prompt opens thinking, that prompt's token count, and the
/// image spans with their pixels.
type Rendered = (Vec<u32>, bool, usize, Vec<ImageSpan>, Vec<u8>);

/// A prepared generation (backend.Job), ready to submit.
pub struct Job {
    pub submission: Submission,
    pub public_id: String,
    pub created: u64,
    pub thinking: bool,
    pub may_call_tools: bool,
    pub policy: Option<Arc<ToolPolicy>>,
    pub response: Option<ResponseFormat>,
    pub stop_sequences: Vec<String>,
    pub deadline: Option<Instant>,
    pub prompt_len: usize,
}

/// What a running generation reports to whoever answers the client.
pub enum Event {
    /// The engine admitted the request.
    Started,
    Progress(Value),
    Delta(Delta),
}

/// A finished chat answer.
pub struct Answer {
    pub reasoning: String,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub result: NativeResult,
    pub reasoning_tokens: u32,
    pub called: bool,
}

/// The output projector's view of a request's tool types, read through the constrain port's ParameterTypes.
struct PolicyTypes(ParameterTypes);
impl ChatToolTypes for PolicyTypes {
    fn of(&mut self, name: &str) -> ToolParameterTypes {
        let types = self.0.of(name);
        let set = |t: Option<constrain::JsonTypes>| t.map(|t| t.names().into_iter().map(str::to_owned).collect());
        ToolParameterTypes {
            parameters: types.parameters.iter().map(|(k, v)| (k.clone(), set(*v))).collect(),
            other: set(types.other),
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn remaining(deadline: Option<Instant>) -> Result<Option<Duration>, ApiError> {
    match deadline {
        None => Ok(None),
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                Err(ApiError::timeout())
            } else {
                Ok(Some(left))
            }
        }
    }
}

impl App {
    pub fn max_context(&self) -> u32 {
        self.ready.max_context_tokens
    }

    pub fn accepts_model(&self, model: Option<&Value>) -> bool {
        match model {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.is_empty() || self.model_names.iter().any(|n| n == s),
            _ => false,
        }
    }

    pub fn input_modalities(&self) -> Vec<&'static str> {
        // PDF input is not ported (docs/splash-native.md); images are.
        if self.ready.vision { vec!["text", "image"] } else { vec!["text"] }
    }

    fn encode_plain(&self) -> impl Fn(&str) -> Vec<u32> + '_ {
        move |text| self.tokenizer.encode(text)
    }

    /// The prompt's tokens, with each image the template placed widened to its pads (frontend._render_prompt and
    /// _render_image_tokens, _expand_image_pads).
    fn render(&self, request: &PromptRequest, tools: Option<&[Value]>, deadline: Option<Instant>) -> Result<Rendered, ApiError> {
        let has_images = request.messages.iter().any(|m| {
            m.get("content").and_then(Value::as_array).is_some_and(|parts| parts.iter().any(|p| p.get("type").and_then(Value::as_str) == Some("image_url")))
        });
        if !has_images {
            let rendered = render_prompt(&self.templates, &self.tokenizer, request, tools, true)?;
            return Ok((rendered.tokens, rendered.thinking, rendered.generation_prompt_tokens, vec![], vec![]));
        }
        let template = self.templates.select(tools);
        if !template.accepts(&request.messages) {
            return Err(ApiError::bad(LATER_SYSTEM_UNSUPPORTED));
        }
        let options = request.template_variables(tools, true);
        let prepared = images::prepare_images(&request.messages, &self.images, self.max_image_pixels, Some(self.max_context() as usize), deadline)?;
        remaining(deadline)?;
        let pad = self.tokenizer.token_to_id(IMAGE_PAD_TOKEN).ok_or_else(|| ApiError::bad("the tokenizer does not define the image pad token"))?;
        // A marker no request text can hold ties each image to the placeholder the template itself emitted; one per
        // process, as Python's, so the marked template compiles once.
        static MARKER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let marker = MARKER.get_or_init(|| format!("__splash_image_{:032x}__", rand::random::<u128>())).clone();
        let marked = template.render_with_image_marker(&request.messages, &options, &marker).map_err(|e| ApiError::bad(e.client_message()))?;
        let parts: Vec<&str> = marked.split(marker.as_str()).collect();
        let mut offsets = std::collections::HashSet::new();
        let mut offset = 0;
        for part in &parts[..parts.len() - 1] {
            offset += part.len();
            offsets.insert((offset, offset + IMAGE_PAD_TOKEN.len()));
            offset += IMAGE_PAD_TOKEN.len();
        }
        let rendered = parts.join(IMAGE_PAD_TOKEN);
        let (tokens, spans) = self.tokenizer.encode_with_offsets(&rendered);
        let positions: Vec<usize> = spans.iter().enumerate().filter(|(_, s)| offsets.contains(s)).map(|(i, _)| i).collect();
        let probed = template.generation_prompt(&options);
        let (thinking, generation) = generation_prompt(probed.as_ref().map(|p| (p.text.as_str(), p.tokens.as_slice())), &rendered, &tokens)?;
        let expanded = images::expand_image_pads(&tokens, &prepared, &positions, pad, self.max_context() as usize)?;
        let spans = expanded
            .spans
            .iter()
            .map(|s| ImageSpan { offset: s.offset, tokens: s.tokens, grid_height: s.grid_height, grid_width: s.grid_width, digest_lo: s.digest_lo, digest_hi: s.digest_hi })
            .collect();
        Ok((expanded.tokens, thinking, generation, spans, expanded.pixels))
    }

    /// A chat-completions body as a job (frontend._prepare). Fields checked without the prompt fail first.
    pub fn prepare(&self, body: &Map<String, Value>) -> Result<Job, ApiError> {
        let deadline = request_timeout(body, self.request_timeout)?.map(|t| Instant::now() + t);
        let options = generation_options(body, self.default_thinking_budget)?;
        let n_ok = body.get("n").is_none_or(|n| n.as_u64() == Some(1) && !n.is_f64());
        let logprobs_ok = body.get("logprobs").is_none_or(|l| l == &Value::Bool(false));
        if !n_ok || !logprobs_ok {
            return Err(ApiError::bad("n and logprobs are not currently supported"));
        }
        if !self.accepts_model(body.get("model")) {
            let name = body.get("model").and_then(Value::as_str).unwrap_or_default();
            return Err(ApiError::new(404, format!("model {name} not found"), "model_not_found"));
        }
        let value = Value::Object(body.clone());
        let mut no_files = |_: &Value| -> Result<Vec<Value>, ChatError> { Err(ChatError::bad_request("file and PDF inputs are not supported by this server")) };
        let request = PromptRequest::from_body(&value, self.default_reasoning_effort.as_deref(), self.ready.vision, &mut no_files)?;
        let tools = constrain::normalize_tools(body.get("tools"), body.get("tool_choice"), body.get("parallel_tool_calls"), HashMap::new())?;
        let (tool_list, policy) = match tools {
            Some(t) => (Some(t.tools), Some(Arc::new(t.policy))),
            None => (None, None),
        };
        let response = constrain::normalize_response_format(body.get("response_format"))?;
        if options.ignore_eos && constrain::is_constrained(policy.as_deref(), response.as_ref()) {
            return Err(ApiError::bad("ignore_eos cannot be combined with constrained tool calls, tool_choice none or structured output"));
        }
        let (tokens, thinking, generation_prompt_tokens, image_spans, image_pixels) = self.render(&request, tool_list.as_deref(), deadline)?;
        let encode = self.encode_plain();
        let constraint = self.constraints.request_constraint(policy.as_deref(), response.as_ref(), thinking, remaining(deadline)?, &encode)?;
        remaining(deadline)?;
        let field = if body.contains_key("max_completion_tokens") { "max_completion_tokens" } else { "max_tokens" };
        let requested = body.get("max_completion_tokens").or_else(|| body.get("max_tokens"));
        let max_new = output_budget(requested, tokens.len(), self.max_context(), field)?;
        let may_call_tools = policy.as_ref().is_some_and(|p| !p.schemas.is_empty());
        let thinking_budget = options.thinking_token_budget.filter(|b| thinking && *b < max_new).map(|tokens| ThinkingBudget {
            tokens,
            end: vec![self.format.think_end_id],
            also_ends: if may_call_tools && self.format.calls_end_reasoning() { self.format.call_open_id } else { self.format.think_end_id },
        });
        let prompt_len = tokens.len();
        Ok(Job {
            submission: Submission {
                prompt_tokens: tokens,
                max_output_tokens: max_new,
                sampling: options.sampling,
                seed: options.seed,
                priority: options.priority,
                timeout: remaining(deadline)?,
                constraint: constraint.map(|c| Arc::new(c) as Arc<dyn crate::engine::MaskProvider>),
                image_spans,
                image_pixels,
                score_tokens: vec![],
                generation_prompt_tokens: generation_prompt_tokens as u32,
                ignore_eos: options.ignore_eos,
                return_progress: false,
                cache_salt: options.cache_salt,
                thinking_budget,
            },
            public_id: format!("{:032x}", rand::random::<u128>()),
            created: now_unix(),
            thinking,
            may_call_tools,
            policy,
            response,
            stop_sequences: options.stop_sequences,
            deadline,
            prompt_len,
        })
    }

    /// The error a request not yet admitted gets now (backend.refusal).
    pub fn refusal(&self) -> Option<ApiError> {
        self.engine.get().failure().map(|e| ApiError::new(503, format!("engine is recovering; retry shortly (last failure: {e})"), "engine_recovering"))
    }

    pub fn submit(&self, job: &mut Job) -> Result<Request, ApiError> {
        remaining(job.deadline)?;
        let submission = std::mem::take(&mut job.submission);
        self.engine.get().submit(submission).map_err(engine_error)
    }

    /// Read a submitted job to its end, reporting each delta through `emit` (server._collect). `emit` returning
    /// false means the client left: the request is cancelled.
    pub fn collect(&self, job: &Job, request: Request, emit: &mut dyn FnMut(Event) -> bool) -> Result<Answer, ApiError> {
        let tool_types: Option<Box<dyn ChatToolTypes>> = job.policy.clone().map(|p| Box::new(PolicyTypes(ParameterTypes::new(p))) as Box<dyn ChatToolTypes>);
        let mut reader = OutputReader::new(&self.format, job.thinking, job.may_call_tools, tool_types, &job.public_id, job.response.is_some());
        let mut streamer = Streamer::new(self.tokenizer.clone(), job.stop_sequences.clone());
        let mut cache = CacheInfo { status: "unknown", ..Default::default() };
        let mut first_batch = 0u32;
        let mut seen_tokens = false;
        let publish = |deltas: Vec<Delta>, emit: &mut dyn FnMut(Event) -> bool| -> bool { deltas.into_iter().all(|d| emit(Event::Delta(d))) };
        let done = loop {
            let wait = remaining(job.deadline).inspect_err(|_| {
                request.cancel();
            })?;
            let left = wait.map_or(Duration::from_millis(500), |l| l.min(Duration::from_millis(500)));
            let event = match request.events.recv_timeout(left) {
                Ok(e) => e,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // A keepalive opportunity; a client that left cancels.
                    if !emit(Event::Progress(Value::Null)) {
                        request.cancel();
                        return Err(ApiError::new(499, "client disconnected", "client_disconnected"));
                    }
                    continue;
                }
                Err(_) => return Err(ApiError::new(503, "the inference engine stopped unexpectedly and is restarting; retry the request", "runtime_unavailable")),
            };
            match event {
                RequestEvent::Started { lane, matched_prompt_tokens } => {
                    cache = CacheInfo { status: if matched_prompt_tokens > 0 { "hit" } else { "miss" }, matched_tokens: matched_prompt_tokens, lane: Some(lane), deduplicated_tokens: 0 };
                    if !emit(Event::Started) {
                        request.cancel();
                        return Err(ApiError::new(499, "client disconnected", "client_disconnected"));
                    }
                }
                RequestEvent::Progress { processed_tokens, elapsed_micros } => {
                    let progress = json!({"total": job.prompt_len, "cache": cache.matched_tokens, "processed": processed_tokens, "time_ms": elapsed_micros as f64 / 1000.0});
                    if !emit(Event::Progress(progress)) {
                        request.cancel();
                        return Err(ApiError::new(499, "client disconnected", "client_disconnected"));
                    }
                }
                RequestEvent::Tokens(tokens) => {
                    if !seen_tokens {
                        first_batch = tokens.len() as u32;
                        seen_tokens = true;
                    }
                    if streamer.stop_sequence().is_some() {
                        continue;
                    }
                    for text in streamer.put_tokens(&tokens)? {
                        if !publish(reader.put(&text), emit) {
                            request.cancel();
                            return Err(ApiError::new(499, "client disconnected", "client_disconnected"));
                        }
                    }
                    if streamer.stop_sequence().is_some() {
                        // The output ends at its stop sequence; the engine need not generate past it.
                        request.cancel();
                    }
                    // Tokens can arrive for minutes without a delta (a tool argument held until it ends): the stream
                    // still owes its client a keepalive every couple of seconds.
                    if !emit(Event::Progress(Value::Null)) {
                        request.cancel();
                        return Err(ApiError::new(499, "client disconnected", "client_disconnected"));
                    }
                }
                RequestEvent::Done(done) => break done,
                RequestEvent::Failed { message, retryable, retry_after_seconds, error, kind } => {
                    return Err(failure(kind, message, retryable, retry_after_seconds, error.as_ref()));
                }
            }
        };
        for text in streamer.end()? {
            publish(reader.put(&text), emit);
        }
        // Read after end(): a stop sequence only the whole decode shows still ends the answer (backend._finalize).
        let stop_sequence = streamer.stop_sequence().map(str::to_owned);
        let reason = if stop_sequence.is_some() {
            "stop"
        } else {
            match done.reason {
                FinishReason::Stop => "stop",
                FinishReason::Length => "length",
                FinishReason::Cancelled => "cancelled",
            }
        };
        let completion_tokens = if stop_sequence.is_some() { streamer.token_ids().len() as u32 } else { done.completion_tokens };
        let reasoning_tokens = streamer.count_reasoning_tokens(Some(&self.format), job.thinking, job.may_call_tools) as u32;
        let (deltas, collected) = reader.finish(reason == "length", stop_sequence.is_some());
        publish(deltas, emit);
        let ended = reason != "length" && stop_sequence.is_none();
        if ended && collected.tool_calls.is_empty() {
            constrain::validate_response_content(&collected.content, job.response.as_ref())?;
        }
        let result = NativeResult {
            reason,
            prompt_tokens: done.prompt_tokens,
            completion_tokens,
            start_to_first_token_ms: done.prefill_micros as f64 / 1000.0,
            first_token_to_done_ms: done.decode_micros as f64 / 1000.0,
            request_wall_ms: done.wall_micros as f64 / 1000.0,
            prefill_tokens: done.prompt_tokens.saturating_sub(cache.matched_tokens),
            cache: CacheInfo { deduplicated_tokens: done.deduplicated_tokens as u64, ..cache },
            stop_sequence,
            first_token_batch_tokens: first_batch,
            drafted_tokens: done.drafted_tokens,
            accepted_draft_tokens: done.accepted_draft_tokens,
            draft_cycles: done.draft_cycles,
            bounded: done.bounded,
        };
        // A call the limit or a stop sequence cut off is no call (server._collect).
        let called = !collected.tool_calls.is_empty() && !collected.cut_in_call;
        Ok(Answer { reasoning: collected.reasoning, content: collected.content, tool_calls: collected.tool_calls, result, reasoning_tokens, called })
    }
}

fn engine_error(e: EngineError) -> ApiError {
    match e {
        EngineError::PendingLimit(_) => ApiError::new(503, "frontend request capacity is exhausted", "frontend_overloaded"),
        EngineError::Request { code, message } => ApiError::new(400, message, code),
        EngineError::Timeout(_) => ApiError::timeout(),
        other => ApiError::new(503, format!("the inference engine stopped unexpectedly and is restarting; retry the request ({other})"), "runtime_unavailable"),
    }
}

/// A request's failure as the API answers it (backend._api_error).
fn failure(kind: FailureKind, message: String, retryable: bool, retry_after: u32, error: Option<&crate::protocol::ErrorEvent>) -> ApiError {
    match kind {
        FailureKind::Constraint if retryable => ApiError::new(503, message, "runtime_busy"),
        FailureKind::Constraint => ApiError::new(400, message, "constraint_error"),
        FailureKind::Engine => ApiError::new(503, "the inference engine stopped unexpectedly and is restarting; retry the request", "runtime_unavailable"),
        FailureKind::Request => {
            let code = error.map(|e| String::from_utf8_lossy(&e.code).into_owned()).unwrap_or_else(|| "request_failed".into());
            let message = error.map(|e| String::from_utf8_lossy(&e.message).into_owned()).unwrap_or(message);
            match code.as_str() {
                "deadline_exceeded" => ApiError::new(504, message, "request_timeout"),
                "capacity_exhausted" => ApiError::new(
                    400,
                    format!("the request does not fit in the memory this server may use, even after every cached prefix was evicted; restart the server with a larger --max-memory or a smaller --max-context ({message})"),
                    code,
                ),
                "kv_disk_quota" => ApiError::new(400, message, code),
                _ => {
                    let request_codes = ["invalid_constraint", "invalid_count", "invalid_deadline", "invalid_enum_value", "invalid_request", "invalid_sampling", "limit_exceeded"];
                    let status = if retryable { 503 } else if request_codes.contains(&code.as_str()) { 400 } else { 500 };
                    let mut e = ApiError::new(status, message, code);
                    e.retry_after = Some(retry_after.max(1));
                    e
                }
            }
        }
    }
}

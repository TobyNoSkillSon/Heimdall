//! The native wire protocol, version 11 (server/protocol.py). OWNER: engine worker.
//!
//! The server encodes client frames and decodes engine events; the engine does the reverse. Every frame is a 16-byte
//! header (`SPLH`, u16 version, u16 type, u64 payload length) and a little-endian payload. Validation keeps Python's
//! rules, order, issue codes and messages, so an engine sees the same refusals from either server. Rust's types make
//! Python's integer-range and type checks vacuous; those rules are the ones not repeated here.
//!
//! The engine side (`encode_event`, `decode_client_frame`, `FrameParser::client_frames`) exists for the fake engine
//! and tests; the real engine is `runtime/engine/Protocol.*` in Splash.
use std::fmt;

pub const PROTOCOL_VERSION: u16 = 11;
pub const FRAME_HEADER_BYTES: usize = 16;
pub const STATUS_SCHEMA_VERSION: u64 = 6;
/// Score-only requests carry 2..255 distinct option token ids and produce no generated tokens; a successful score
/// DoneEvent returns one raw final-position logit per requested token, in request order.
pub const MIN_SCORE_TOKENS: usize = 2;
pub const MAX_SCORE_TOKENS: usize = 255;
pub const MAX_THINKING_END_TOKENS: usize = 64;
/// Patches per image, the most the vision encoder takes.
pub const MAX_IMAGE_PATCHES: u64 = 16384;
/// Image pixels travel inside the request frame; a multi-image turn can carry well over 64 MiB.
pub const MAX_FRAME_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_STATUS_JSON_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_ERROR_STRING_BYTES: u64 = 1024 * 1024;
pub const MAX_PROMPT_TOKENS: usize = 1 << 20;
pub const MAX_LOGICAL_OUTPUT_TOKENS: u32 = 1 << 20;
pub const MAX_TOKEN_BATCH: usize = 4096;
pub const MAX_SIMULATION_TOKENS: usize = 32;
pub const MAX_IMAGE_SPANS: usize = 64;
pub const MAX_MASK_WORDS: u64 = 1 << 20;

const MAGIC: &[u8; 4] = b"SPLH";
/// `<QBBQQ`: the integer head a deadline refresh rewrites without touching the sampling floats.
const REQUEST_HEAD_BYTES: usize = 26;
/// `<QBBQQ IIIffIffffQIIIQQIII`.
pub const REQUEST_FIXED_BYTES: usize = 114;
/// `<IIIIQQ`.
pub const IMAGE_SPAN_BYTES: usize = 32;
const CANCEL_BYTES: usize = 8;
const MASK_RESPONSE_BYTES: usize = 20;
const STATUS_REQUEST_BYTES: usize = 8;
const RELEASE_REQUEST_BYTES: usize = 8;
const READY_BYTES: usize = 9;
const START_BYTES: usize = 16;
const PROMPT_PROGRESS_BYTES: usize = 20;
const TOKENS_BYTES: usize = 16;
const MASK_REQUEST_BYTES: usize = 24;
/// `<QBBIIQQQIIIII`.
pub const DONE_FIXED_BYTES: usize = 62;
/// `<BBIQII`.
pub const ERROR_FIXED_BYTES: usize = 22;
const STATUS_JSON_BYTES: usize = 8;
/// The request frame's thinking budget tokens without one.
const NO_THINKING_BUDGET: u32 = u32::MAX;
/// A sampling temperature is 0 or a normal float32: the kernels divide by it, and Metal flushes a subnormal to zero.
const FLOAT32_MIN: f32 = f32::MIN_POSITIVE;

#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FrameType {
    Request = 0x0001,
    Cancel = 0x0002,
    MaskResponse = 0x0003,
    StatusRequest = 0x0004,
    ReleaseRequest = 0x0005,
    Ready = 0x0100,
    Start = 0x0101,
    Tokens = 0x0102,
    MaskRequest = 0x0103,
    Done = 0x0104,
    Error = 0x0105,
    StatusJson = 0x0107,
    PromptProgress = 0x0108,
}

impl FrameType {
    pub fn from_u16(value: u16) -> Option<Self> {
        use FrameType::*;
        Some(match value {
            0x0001 => Request,
            0x0002 => Cancel,
            0x0003 => MaskResponse,
            0x0004 => StatusRequest,
            0x0005 => ReleaseRequest,
            0x0100 => Ready,
            0x0101 => Start,
            0x0102 => Tokens,
            0x0103 => MaskRequest,
            0x0104 => Done,
            0x0105 => Error,
            0x0107 => StatusJson,
            0x0108 => PromptProgress,
            _ => return None,
        })
    }

    /// The frame types the engine sends; the others are the server's.
    pub fn is_event(self) -> bool {
        (self as u16) >= 0x0100
    }

    pub fn name(self) -> &'static str {
        use FrameType::*;
        match self {
            Request => "request",
            Cancel => "cancel",
            MaskResponse => "mask_response",
            StatusRequest => "status_request",
            ReleaseRequest => "release_request",
            Ready => "ready",
            Start => "start",
            Tokens => "tokens",
            MaskRequest => "mask_request",
            Done => "done",
            Error => "error",
            StatusJson => "status_json",
            PromptProgress => "prompt_progress",
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailureClass {
    RequestError = 1,
    EngineUnhealthy = 2,
    ProtocolFatal = 3,
}

impl FailureClass {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::RequestError),
            2 => Some(Self::EngineUnhealthy),
            3 => Some(Self::ProtocolFatal),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::RequestError => "request_error",
            Self::EngineUnhealthy => "engine_unhealthy",
            Self::ProtocolFatal => "protocol_fatal",
        }
    }
}

/// Issue codes travel by name; the values are runtime/engine/Protocol.hpp's.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IssueCode {
    None = 0,
    BadMagic = 1,
    UnsupportedVersion = 2,
    UnknownFrameType = 3,
    FrameTooLarge = 4,
    InvalidPayloadLength = 5,
    TruncatedFrame = 6,
    InvalidRequestId = 7,
    InvalidEnumValue = 8,
    InvalidDeadline = 9,
    InvalidSampling = 10,
    InvalidCount = 11,
    InvalidConstraint = 12,
    InvalidErrorClassification = 13,
    LimitExceeded = 14,
    IntegerOverflow = 15,
    AllocationFailure = 16,
}

impl IssueCode {
    pub fn name(self) -> &'static str {
        use IssueCode::*;
        match self {
            None => "none",
            BadMagic => "bad_magic",
            UnsupportedVersion => "unsupported_version",
            UnknownFrameType => "unknown_frame_type",
            FrameTooLarge => "frame_too_large",
            InvalidPayloadLength => "invalid_payload_length",
            TruncatedFrame => "truncated_frame",
            InvalidRequestId => "invalid_request_id",
            InvalidEnumValue => "invalid_enum_value",
            InvalidDeadline => "invalid_deadline",
            InvalidSampling => "invalid_sampling",
            InvalidCount => "invalid_count",
            InvalidConstraint => "invalid_constraint",
            InvalidErrorClassification => "invalid_error_classification",
            LimitExceeded => "limit_exceeded",
            IntegerOverflow => "integer_overflow",
            AllocationFailure => "allocation_failure",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolIssue {
    pub failure_class: FailureClass,
    pub code: IssueCode,
    pub request_id: u64,
    pub message: String,
}

impl ProtocolIssue {
    pub fn describe(&self) -> String {
        let mut result = format!("{}:{}", self.failure_class.name(), self.code.name());
        if self.request_id != 0 {
            result += &format!(" request={}", self.request_id);
        }
        if !self.message.is_empty() {
            result += &format!(": {}", self.message);
        }
        result
    }
}

impl fmt::Display for ProtocolIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{issue}")]
pub struct ProtocolError {
    pub issue: ProtocolIssue,
}

fn issue(failure_class: FailureClass, code: IssueCode, message: impl Into<String>, request_id: u64) -> ProtocolIssue {
    ProtocolIssue { failure_class, code, request_id, message: message.into() }
}

/// An engine event that breaks the protocol: the stream is not trusted.
fn fatal(code: IssueCode, message: impl Into<String>, request_id: u64) -> ProtocolError {
    ProtocolError { issue: issue(FailureClass::ProtocolFatal, code, message, request_id) }
}

/// A client frame's failure fails the request it names. One that names none cannot be answered as a request's error
/// and fails the stream, as the engine classifies it (Protocol.cpp requestIssue).
fn fail_request(code: IssueCode, message: impl Into<String>, request_id: u64) -> ProtocolError {
    let class = if request_id != 0 { FailureClass::RequestError } else { FailureClass::ProtocolFatal };
    ProtocolError { issue: issue(class, code, message, request_id) }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RequestPriority {
    Foreground = 0,
    #[default]
    Normal = 1,
    Background = 2,
}

impl RequestPriority {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Foreground),
            1 => Some(Self::Normal),
            2 => Some(Self::Background),
            _ => None,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ConstraintMode {
    #[default]
    None = 0,
    TokenMask = 1,
}

impl ConstraintMode {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::TokenMask),
            _ => None,
        }
    }
}

/// Request flag bits. A request with any other bit set is a request error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RequestFlags(pub u32);

impl RequestFlags {
    pub const NONE: Self = Self(0);
    /// Never select the model's stop tokens, so generation runs to its output limit. Only unconstrained generation
    /// can carry it.
    pub const IGNORE_END_OF_SEQUENCE: Self = Self(1 << 0);
    /// Report the prompt's progress while it prefills (PromptProgressEvent).
    pub const RETURN_PROGRESS: Self = Self(1 << 1);
    const DEFINED: u32 = Self::IGNORE_END_OF_SEQUENCE.0 | Self::RETURN_PROGRESS.0;

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }
}

impl std::ops::BitOr for RequestFlags {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The defaults are greedy selection with nothing changing the logits, which score requests require. A top_k of 0,
/// or one past the vocabulary, keeps every token; the default penalties change nothing, and a min_p of 0 drops none.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SamplingParameters {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub presence_penalty: f32,
    pub frequency_penalty: f32,
    pub repetition_penalty: f32,
    pub min_p: f32,
}

impl Default for SamplingParameters {
    fn default() -> Self {
        Self {
            temperature: 0.0,
            top_p: 1.0,
            top_k: 0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            repetition_penalty: 1.0,
            min_p: 0.0,
        }
    }
}

/// One image in the prompt: its placeholder token run, the patch grid of the resized pixels, and a 128-bit digest of
/// that content. Placeholder token ids are identical for every image, so cache identity keys on spans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ImageSpan {
    pub offset: u32,
    pub tokens: u32,
    pub grid_height: u32,
    pub grid_width: u32,
    pub digest_lo: u64,
    pub digest_hi: u64,
}

impl ImageSpan {
    /// The span's resized uint8 RGB bytes: 16x16 pixels per patch.
    pub fn pixel_bytes(&self) -> u64 {
        self.grid_height as u64 * 16 * self.grid_width as u64 * 16 * 3
    }
}

/// A request that generates inside a thinking block may select at most `tokens` thinking tokens; the engine then
/// selects the `end` tokens in order, the last of which closes the block. Thinking also ends where the request
/// selects that close or `also_ends` first (a tool call's opening that follows thinking directly).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ThinkingBudget {
    pub tokens: u32,
    pub end: Vec<u32>,
    pub also_ends: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestFrame {
    pub request_id: u64,
    pub priority: RequestPriority,
    pub absolute_deadline_unix_micros: u64,
    pub remaining_deadline_micros: u64,
    pub logical_max_output_tokens: u32,
    pub prompt_tokens: Vec<u32>,
    pub sampling: SamplingParameters,
    pub seed: u64,
    pub constraint: ConstraintMode,
    /// Sorted, non-overlapping image spans; their resized uint8 RGB pixels concatenated in span order.
    pub image_spans: Vec<ImageSpan>,
    pub image_pixels: Vec<u8>,
    /// Option token ids for score-only requests; empty means ordinary generation.
    pub score_tokens: Vec<u32>,
    /// Trailing prompt tokens of the chat template's generation prompt; zero when unknown. It must leave at least one
    /// prompt token.
    pub generation_prompt_tokens: u32,
    pub flags: RequestFlags,
    /// The request's cache_salt hashed to 128 bits; zero without one.
    pub cache_salt: u128,
    /// Only a request that generates inside a thinking block has one; a score request never does.
    pub thinking_budget: Option<ThinkingBudget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CancelFrame {
    pub request_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskResponseFrame {
    pub request_id: u64,
    pub mask_request_id: u64,
    /// `words_per_mask` words per mask row; bit t of a row allows token t.
    pub mask_words: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusRequestFrame {
    pub correlation_id: u64,
}

/// Give the weights back now, as the idle release does, when the engine holds no request and restores none;
/// answered by a status JSON with the same correlation id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseRequestFrame {
    pub correlation_id: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    Request(RequestFrame),
    Cancel(CancelFrame),
    MaskResponse(MaskResponseFrame),
    StatusRequest(StatusRequestFrame),
    ReleaseRequest(ReleaseRequestFrame),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadyEvent {
    pub max_concurrent_requests: u32,
    pub max_context_tokens: u32,
    /// Requests may carry image spans; false for a model serving without vision.
    pub vision: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartEvent {
    pub request_id: u64,
    pub lane: u32,
    /// The cached prefix the request starts from; zero for a cold start.
    pub matched_prompt_tokens: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptProgressEvent {
    pub request_id: u64,
    pub processed_tokens: u32,
    pub elapsed_micros: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokensEvent {
    pub request_id: u64,
    pub sequence_offset: u32,
    pub tokens: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskRequestEvent {
    pub request_id: u64,
    pub mask_request_id: u64,
    pub words_per_mask: u32,
    pub simulation_tokens: Vec<u32>,
}

impl MaskRequestEvent {
    /// Rows before and after each simulated context token: the initial request carries no tokens and has one row; a
    /// verify request carries `(pending_anchor, *draft_proposals)`.
    pub fn mask_rows(&self) -> usize {
        self.simulation_tokens.len() + 1
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FinishReason {
    Stop = 0,
    Length = 1,
    Cancelled = 2,
}

impl FinishReason {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Stop),
            1 => Some(Self::Length),
            2 => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DoneEvent {
    pub request_id: u64,
    pub reason: FinishReason,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub prefill_micros: u64,
    pub decode_micros: u64,
    pub wall_micros: u64,
    /// Raw final-position logits for a score-only request, in requested token order; empty otherwise.
    pub option_logits: Vec<f32>,
    /// Tokens whose KV the request read from cached blocks another request computed, past its cached prefix.
    pub deduplicated_tokens: u32,
    pub drafted_tokens: u32,
    pub accepted_draft_tokens: u32,
    pub draft_cycles: u32,
    /// A LENGTH end short of the logical maximum, at a bound of the engine's own (SSD quota, lone-lane memory).
    pub bounded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorEvent {
    pub failure_class: FailureClass,
    pub request_id: u64,
    pub retryable: bool,
    pub code: Vec<u8>,
    pub message: Vec<u8>,
    /// For a retryable request error, the seconds after which a retry may succeed; 0 for at once.
    pub retry_after_seconds: u32,
}

impl ErrorEvent {
    pub fn code_text(&self) -> String {
        String::from_utf8_lossy(&self.code).into_owned()
    }

    pub fn message_text(&self) -> String {
        String::from_utf8_lossy(&self.message).into_owned()
    }
}

/// The JSON is opaque to the transport: the document carries its own schema_version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusJsonEvent {
    pub correlation_id: u64,
    pub json: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    Ready(ReadyEvent),
    Start(StartEvent),
    PromptProgress(PromptProgressEvent),
    Tokens(TokensEvent),
    MaskRequest(MaskRequestEvent),
    Done(DoneEvent),
    Error(ErrorEvent),
    StatusJson(StatusJsonEvent),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub frame_type: FrameType,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParseStep {
    pub consumed: usize,
    pub frame: Option<Frame>,
    pub issue: Option<ProtocolIssue>,
}

// ---- bounds ----

fn payload_bounds(frame_type: FrameType) -> (u64, u64) {
    use FrameType::*;
    let size = |n: usize| n as u64;
    let (min, max) = match frame_type {
        // Image pixels dominate prompt tokens; the frame limit is the bound.
        Request => (size(REQUEST_FIXED_BYTES), MAX_FRAME_PAYLOAD_BYTES),
        Cancel => (size(CANCEL_BYTES), size(CANCEL_BYTES)),
        MaskResponse => (size(MASK_RESPONSE_BYTES), size(MASK_RESPONSE_BYTES) + MAX_MASK_WORDS * 4),
        StatusRequest => (size(STATUS_REQUEST_BYTES), size(STATUS_REQUEST_BYTES)),
        ReleaseRequest => (size(RELEASE_REQUEST_BYTES), size(RELEASE_REQUEST_BYTES)),
        Ready => (size(READY_BYTES), size(READY_BYTES)),
        PromptProgress => (size(PROMPT_PROGRESS_BYTES), size(PROMPT_PROGRESS_BYTES)),
        Start => (size(START_BYTES), size(START_BYTES)),
        Tokens => (size(TOKENS_BYTES), size(TOKENS_BYTES + MAX_TOKEN_BATCH * 4)),
        MaskRequest => (size(MASK_REQUEST_BYTES), size(MASK_REQUEST_BYTES + MAX_SIMULATION_TOKENS * 4)),
        Done => (size(DONE_FIXED_BYTES), size(DONE_FIXED_BYTES + MAX_SCORE_TOKENS * 4)),
        Error => (size(ERROR_FIXED_BYTES), size(ERROR_FIXED_BYTES) + MAX_ERROR_STRING_BYTES * 2),
        StatusJson => (size(STATUS_JSON_BYTES), size(STATUS_JSON_BYTES) + MAX_STATUS_JSON_BYTES),
    };
    (min, max.min(MAX_FRAME_PAYLOAD_BYTES))
}

fn payload_length_issue(frame_type: FrameType, payload_bytes: u64) -> Option<ProtocolIssue> {
    let (minimum, maximum) = payload_bounds(frame_type);
    let name = frame_type.name();
    if payload_bytes > maximum {
        return Some(issue(
            FailureClass::ProtocolFatal,
            IssueCode::FrameTooLarge,
            format!("{name} payload length {payload_bytes} exceeds its safe limit {maximum}"),
            0,
        ));
    }
    if payload_bytes < minimum {
        return Some(issue(
            FailureClass::ProtocolFatal,
            IssueCode::InvalidPayloadLength,
            format!("{name} payload length {payload_bytes} is below its required minimum {minimum}"),
            0,
        ));
    }
    None
}

// ---- request validation ----

fn image_spans_check(request: &RequestFrame) -> Result<(), &'static str> {
    let prompt_tokens = request.prompt_tokens.len() as u64;
    let mut previous_end = 0u64;
    let mut pixel_bytes = 0u64;
    for span in &request.image_spans {
        let (height, width) = (span.grid_height as u64, span.grid_width as u64);
        if height < 2 || width < 2 || height % 2 != 0 || width % 2 != 0 || height * width > MAX_IMAGE_PATCHES {
            return Err("image grid must be even-sided and within the patch limit");
        }
        if span.tokens as u64 != (height / 2) * (width / 2) {
            return Err("image span tokens must equal the merged grid size");
        }
        let (offset, end) = (span.offset as u64, span.offset as u64 + span.tokens as u64);
        if offset < previous_end || end > prompt_tokens {
            return Err("image spans must be sorted, non-overlapping runs inside the prompt");
        }
        previous_end = end;
        pixel_bytes += span.pixel_bytes();
    }
    if request.image_pixels.len() as u64 != pixel_bytes {
        return Err("image pixels do not match the image grids");
    }
    Ok(())
}

fn sampling_check(sampling: &SamplingParameters, scores: bool) -> Result<(), &'static str> {
    let SamplingParameters {
        temperature, top_p, presence_penalty: presence, frequency_penalty: frequency, repetition_penalty: repetition,
        min_p, ..
    } = *sampling;
    // Written as Python's `not (...)` so NaN fails each rule.
    #[allow(clippy::neg_cmp_op_on_partial_ord, clippy::manual_range_contains)]
    {
        if !(temperature == 0.0 || (FLOAT32_MIN <= temperature && temperature < f32::INFINITY))
            || !top_p.is_finite()
            || !(0.0 < top_p && top_p <= 1.0)
            || !(0.0 <= min_p && min_p <= 1.0)
        {
            return Err("sampling requires temperature 0 or at least FLT_MIN, top_p in (0,1] and min_p in [0,1]");
        }
        if !(presence.abs() <= 2.0) || !(frequency.abs() <= 2.0) || !repetition.is_finite() || repetition <= 0.0 {
            return Err(
                "sampling requires presence and frequency penalties in [-2,2] and a positive repetition penalty",
            );
        }
    }
    if scores && *sampling != SamplingParameters::default() {
        return Err("score requests require default greedy sampling");
    }
    Ok(())
}

fn thinking_check(thinking: &ThinkingBudget, scores: bool) -> Result<(), String> {
    if scores {
        return Err("score requests take no thinking budget".into());
    }
    if thinking.tokens == NO_THINKING_BUDGET {
        return Err(format!("thinking budget must be an integer in [0, {}]", NO_THINKING_BUDGET - 1));
    }
    let end = &thinking.end;
    if end.is_empty() || end.len() > MAX_THINKING_END_TOKENS {
        return Err(format!("thinking end token count must be in [1, {MAX_THINKING_END_TOKENS}]"));
    }
    // Thinking ends where its end does, and nowhere inside it.
    let inner = &end[..end.len() - 1];
    if inner.contains(&end[end.len() - 1]) || inner.contains(&thinking.also_ends) {
        return Err("only the last thinking end token may end thinking".into());
    }
    Ok(())
}

/// Checks a request as `_validated_request` does and fails with the first rule it breaks.
pub fn validate_request(request: &RequestFrame) -> Result<(), ProtocolError> {
    use IssueCode::*;
    if request.request_id == 0 {
        return Err(fail_request(InvalidRequestId, "request id must be non-zero", 0));
    }
    let id = request.request_id;
    if request.flags.0 & !RequestFlags::DEFINED != 0 {
        return Err(fail_request(InvalidEnumValue, "request flags are not defined by native protocol", id));
    }
    if request.absolute_deadline_unix_micros == 0 || request.remaining_deadline_micros == 0 {
        return Err(fail_request(InvalidDeadline, "absolute and remaining deadlines must be non-zero", id));
    }
    let scores = &request.score_tokens;
    let output = request.logical_max_output_tokens;
    if !scores.is_empty() {
        if output != 0 {
            return Err(fail_request(InvalidCount, "score requests must not generate output tokens", id));
        }
    } else if output == 0 || output > MAX_LOGICAL_OUTPUT_TOKENS {
        return Err(fail_request(LimitExceeded, "logical max output token count exceeds its limit", id));
    }
    let prompt = request.prompt_tokens.len();
    if prompt == 0 || prompt > MAX_PROMPT_TOKENS {
        return Err(fail_request(LimitExceeded, "prompt token count exceeds its limit", id));
    }
    if request.image_spans.len() > MAX_IMAGE_SPANS {
        return Err(fail_request(LimitExceeded, "image span count exceeds its limit", id));
    }
    image_spans_check(request).map_err(|message| fail_request(InvalidCount, message, id))?;
    if request.generation_prompt_tokens as usize >= prompt {
        return Err(fail_request(InvalidCount, "generation prompt must leave a prompt token", id));
    }
    if !scores.is_empty() && (!request.image_spans.is_empty() || !request.image_pixels.is_empty()) {
        return Err(fail_request(InvalidCount, "score requests are text-only", id));
    }
    if !scores.is_empty() {
        let mut distinct = scores.clone();
        distinct.sort_unstable();
        distinct.dedup();
        if scores.len() < MIN_SCORE_TOKENS || scores.len() > MAX_SCORE_TOKENS || distinct.len() != scores.len() {
            return Err(fail_request(
                InvalidCount,
                format!("score requests need {MIN_SCORE_TOKENS}..{MAX_SCORE_TOKENS} distinct option tokens"),
                id,
            ));
        }
    }
    sampling_check(&request.sampling, !scores.is_empty()).map_err(|m| fail_request(InvalidSampling, m, id))?;
    if let Some(thinking) = &request.thinking_budget {
        thinking_check(thinking, !scores.is_empty()).map_err(|m| fail_request(InvalidCount, m, id))?;
    }
    let constrained = request.constraint != ConstraintMode::None;
    if !scores.is_empty() && constrained {
        return Err(fail_request(InvalidConstraint, "score requests do not accept output constraints", id));
    }
    // A grammar decides where constrained output ends.
    if request.flags.contains(RequestFlags::IGNORE_END_OF_SEQUENCE) && (!scores.is_empty() || constrained) {
        return Err(fail_request(InvalidConstraint, "only unconstrained generation can ignore end-of-sequence", id));
    }
    Ok(())
}

// ---- encoding ----

fn frame_buffer(frame_type: FrameType, payload_bytes: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload_bytes);
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    frame.extend_from_slice(&(frame_type as u16).to_le_bytes());
    frame.extend_from_slice(&(payload_bytes as u64).to_le_bytes());
    frame
}

trait Put {
    fn u8(&mut self, v: u8);
    fn u32(&mut self, v: u32);
    fn u64(&mut self, v: u64);
    fn f32(&mut self, v: f32);
    fn words(&mut self, v: &[u32]);
}

impl Put for Vec<u8> {
    fn u8(&mut self, v: u8) {
        self.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn words(&mut self, v: &[u32]) {
        for w in v {
            self.extend_from_slice(&w.to_le_bytes());
        }
    }
}

/// The whole request frame, header included, in one buffer each part is copied into once: image pixels run to
/// hundreds of MiB.
fn request_frame(request: &RequestFrame) -> Result<Vec<u8>, ProtocolError> {
    validate_request(request)?;
    let end: &[u32] = request.thinking_budget.as_ref().map_or(&[], |t| &t.end);
    let payload_bytes = REQUEST_FIXED_BYTES as u64
        + 4 * request.prompt_tokens.len() as u64
        + IMAGE_SPAN_BYTES as u64 * request.image_spans.len() as u64
        + request.image_pixels.len() as u64
        + 4 * request.score_tokens.len() as u64
        + 4 * end.len() as u64;
    if let Some(found) = payload_length_issue(FrameType::Request, payload_bytes) {
        return Err(fail_request(found.code, found.message, request.request_id));
    }
    let mut f = frame_buffer(FrameType::Request, payload_bytes as usize);
    let s = &request.sampling;
    f.u64(request.request_id);
    f.u8(request.priority as u8);
    f.u8(request.constraint as u8);
    f.u64(request.absolute_deadline_unix_micros);
    f.u64(request.remaining_deadline_micros);
    f.u32(request.logical_max_output_tokens);
    f.u32(request.prompt_tokens.len() as u32);
    f.u32(request.image_spans.len() as u32);
    f.f32(s.temperature);
    f.f32(s.top_p);
    f.u32(s.top_k);
    f.f32(s.presence_penalty);
    f.f32(s.frequency_penalty);
    f.f32(s.repetition_penalty);
    f.f32(s.min_p);
    f.u64(request.seed);
    f.u32(request.score_tokens.len() as u32);
    f.u32(request.generation_prompt_tokens);
    f.u32(request.flags.0);
    f.u64(request.cache_salt as u64);
    f.u64((request.cache_salt >> 64) as u64);
    match &request.thinking_budget {
        None => {
            f.u32(NO_THINKING_BUDGET);
            f.u32(0);
            f.u32(0);
        }
        Some(t) => {
            f.u32(t.tokens);
            f.u32(t.end.len() as u32);
            f.u32(t.also_ends);
        }
    }
    debug_assert_eq!(f.len(), FRAME_HEADER_BYTES + REQUEST_FIXED_BYTES);
    f.words(&request.prompt_tokens);
    for span in &request.image_spans {
        f.u32(span.offset);
        f.u32(span.tokens);
        f.u32(span.grid_height);
        f.u32(span.grid_width);
        f.u64(span.digest_lo);
        f.u64(span.digest_hi);
    }
    f.extend_from_slice(&request.image_pixels);
    f.words(&request.score_tokens);
    f.words(end);
    debug_assert_eq!(f.len() as u64, FRAME_HEADER_BYTES as u64 + payload_bytes);
    Ok(f)
}

/// Validate and serialize one client message as a whole frame in one buffer.
pub fn serialize_message(message: &ClientMessage) -> Result<Vec<u8>, ProtocolError> {
    use IssueCode::*;
    match message {
        ClientMessage::Request(request) => request_frame(request),
        ClientMessage::Cancel(cancel) => {
            if cancel.request_id == 0 {
                return Err(fail_request(InvalidRequestId, "cancel request id must be non-zero", 0));
            }
            let mut f = frame_buffer(FrameType::Cancel, CANCEL_BYTES);
            f.u64(cancel.request_id);
            Ok(f)
        }
        ClientMessage::MaskResponse(response) => {
            let id = response.request_id;
            if id == 0 || response.mask_request_id == 0 {
                return Err(fail_request(InvalidRequestId, "mask response request ids must be non-zero", id));
            }
            let words = response.mask_words.len();
            if words == 0 || words as u64 > MAX_MASK_WORDS {
                return Err(fail_request(LimitExceeded, "mask response word count exceeds its limit", id));
            }
            let mut f = frame_buffer(FrameType::MaskResponse, MASK_RESPONSE_BYTES + 4 * words);
            f.u64(id);
            f.u64(response.mask_request_id);
            f.u32(words as u32);
            f.words(&response.mask_words);
            Ok(f)
        }
        ClientMessage::StatusRequest(request) => {
            let mut f = frame_buffer(FrameType::StatusRequest, STATUS_REQUEST_BYTES);
            f.u64(request.correlation_id);
            Ok(f)
        }
        ClientMessage::ReleaseRequest(request) => {
            let mut f = frame_buffer(FrameType::ReleaseRequest, RELEASE_REQUEST_BYTES);
            f.u64(request.correlation_id);
            Ok(f)
        }
    }
}

/// Serialize a low-level frame whose payload length is within its type's bounds.
pub fn serialize_frame(frame: &Frame) -> Result<Vec<u8>, ProtocolError> {
    if let Some(found) = payload_length_issue(frame.frame_type, frame.payload.len() as u64) {
        return Err(ProtocolError { issue: found });
    }
    let mut f = frame_buffer(frame.frame_type, frame.payload.len());
    f.extend_from_slice(&frame.payload);
    Ok(f)
}

/// Re-stamp a REQUEST frame's absolute deadline as `now` plus its budget, leaving every other byte (and every other
/// frame) as it is. Only the integer head is touched, so a signalling NaN keeps its bits; no validator runs.
pub fn refresh_request_deadline(frame: &[u8], now_unix_micros: u64) -> Vec<u8> {
    let mut out = frame.to_vec();
    if frame.len() < FRAME_HEADER_BYTES + REQUEST_FIXED_BYTES
        || u16::from_le_bytes([frame[6], frame[7]]) != FrameType::Request as u16
    {
        return out;
    }
    let head = FRAME_HEADER_BYTES;
    let remaining = u64::from_le_bytes(frame[head + 18..head + REQUEST_HEAD_BYTES].try_into().unwrap());
    let absolute = now_unix_micros.saturating_add(remaining);
    out[head + 10..head + 18].copy_from_slice(&absolute.to_le_bytes());
    out
}

/// Encode one engine event as the engine does, without validation (the fake engine and tests send invalid events on
/// purpose).
pub fn encode_event(event: &EngineEvent) -> Vec<u8> {
    let mut p = Vec::new();
    let frame_type = match event {
        EngineEvent::Ready(e) => {
            p.u32(e.max_concurrent_requests);
            p.u32(e.max_context_tokens);
            p.u8(e.vision as u8);
            FrameType::Ready
        }
        EngineEvent::Start(e) => {
            p.u64(e.request_id);
            p.u32(e.lane);
            p.u32(e.matched_prompt_tokens);
            FrameType::Start
        }
        EngineEvent::PromptProgress(e) => {
            p.u64(e.request_id);
            p.u32(e.processed_tokens);
            p.u64(e.elapsed_micros);
            FrameType::PromptProgress
        }
        EngineEvent::Tokens(e) => {
            p.u64(e.request_id);
            p.u32(e.sequence_offset);
            p.u32(e.tokens.len() as u32);
            p.words(&e.tokens);
            FrameType::Tokens
        }
        EngineEvent::MaskRequest(e) => {
            p.u64(e.request_id);
            p.u64(e.mask_request_id);
            p.u32(e.words_per_mask);
            p.u32(e.simulation_tokens.len() as u32);
            p.words(&e.simulation_tokens);
            FrameType::MaskRequest
        }
        EngineEvent::Done(e) => {
            p.u64(e.request_id);
            p.u8(e.reason as u8);
            p.u8(e.bounded as u8);
            p.u32(e.prompt_tokens);
            p.u32(e.completion_tokens);
            p.u64(e.prefill_micros);
            p.u64(e.decode_micros);
            p.u64(e.wall_micros);
            p.u32(e.deduplicated_tokens);
            p.u32(e.drafted_tokens);
            p.u32(e.accepted_draft_tokens);
            p.u32(e.draft_cycles);
            p.u32(e.option_logits.len() as u32);
            for v in &e.option_logits {
                p.f32(*v);
            }
            FrameType::Done
        }
        EngineEvent::Error(e) => {
            p.u8(e.failure_class as u8);
            p.u8(e.retryable as u8);
            p.u32(e.retry_after_seconds);
            p.u64(e.request_id);
            p.u32(e.code.len() as u32);
            p.u32(e.message.len() as u32);
            p.extend_from_slice(&e.code);
            p.extend_from_slice(&e.message);
            FrameType::Error
        }
        EngineEvent::StatusJson(e) => {
            p.u64(e.correlation_id);
            p.extend_from_slice(&e.json);
            FrameType::StatusJson
        }
    };
    let mut f = frame_buffer(frame_type, p.len());
    f.extend_from_slice(&p);
    f
}

// ---- decoding ----

/// Little-endian reads at a cursor; callers check the payload is long enough first.
struct Get<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Get<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let out = self.bytes[self.at..self.at + N].try_into().unwrap();
        self.at += N;
        out
    }
    fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }
    fn f32(&mut self) -> f32 {
        f32::from_le_bytes(self.take())
    }
    fn words(&mut self, count: usize) -> Vec<u32> {
        (0..count).map(|_| self.u32()).collect()
    }
    fn bytes(&mut self, count: usize) -> &'a [u8] {
        let out = &self.bytes[self.at..self.at + count];
        self.at += count;
        out
    }
    fn rest(&self) -> usize {
        self.bytes.len() - self.at
    }
}

fn checked_frame(frame: &Frame) -> Result<(), ProtocolError> {
    match payload_length_issue(frame.frame_type, frame.payload.len() as u64) {
        Some(found) => Err(ProtocolError { issue: found }),
        None => Ok(()),
    }
}

/// The `count` u32 words or f32 values that end an event payload must be exactly what is left.
fn tail_check(get: &Get, count: u32, label: &str, request_id: u64) -> Result<(), ProtocolError> {
    if get.rest() as u64 != count as u64 * 4 {
        return Err(fatal(
            IssueCode::InvalidPayloadLength,
            format!("{label} count does not match the binary event payload"),
            request_id,
        ));
    }
    Ok(())
}

/// Decode and strictly validate one engine event.
pub fn decode_frame(frame: &Frame) -> Result<EngineEvent, ProtocolError> {
    use IssueCode::*;
    checked_frame(frame)?;
    let mut g = Get::new(&frame.payload);
    Ok(match frame.frame_type {
        FrameType::Ready => {
            let (concurrent, context, vision) = (g.u32(), g.u32(), g.u8());
            if vision > 1 {
                return Err(fatal(InvalidEnumValue, "ready vision must be a boolean", 0));
            }
            if concurrent == 0 || context == 0 {
                return Err(fatal(InvalidCount, "ready event capacities must be non-zero", 0));
            }
            EngineEvent::Ready(ReadyEvent { max_concurrent_requests: concurrent, max_context_tokens: context, vision: vision == 1 })
        }
        FrameType::Start => {
            let (request_id, lane, matched) = (g.u64(), g.u32(), g.u32());
            if request_id == 0 {
                return Err(fatal(InvalidRequestId, "start event request id must be non-zero", 0));
            }
            EngineEvent::Start(StartEvent { request_id, lane, matched_prompt_tokens: matched })
        }
        FrameType::PromptProgress => {
            let (request_id, processed, elapsed) = (g.u64(), g.u32(), g.u64());
            if request_id == 0 || processed as usize > MAX_PROMPT_TOKENS {
                return Err(fatal(InvalidCount, "prompt progress id or token count is invalid", request_id));
            }
            EngineEvent::PromptProgress(PromptProgressEvent { request_id, processed_tokens: processed, elapsed_micros: elapsed })
        }
        FrameType::Tokens => {
            let (request_id, offset, count) = (g.u64(), g.u32(), g.u32());
            tail_check(&g, count, "token", request_id)?;
            let tokens = g.words(count as usize);
            if request_id == 0 {
                return Err(fatal(InvalidRequestId, "tokens event request id must be non-zero", 0));
            }
            if tokens.is_empty() {
                return Err(fatal(LimitExceeded, "tokens event carries no token", request_id));
            }
            if offset as u64 + tokens.len() as u64 > u32::MAX as u64 {
                return Err(fatal(IntegerOverflow, "tokens event sequence range overflows uint32", request_id));
            }
            EngineEvent::Tokens(TokensEvent { request_id, sequence_offset: offset, tokens })
        }
        FrameType::MaskRequest => {
            let (request_id, mask_request_id, words_per_mask, count) = (g.u64(), g.u64(), g.u32(), g.u32());
            tail_check(&g, count, "simulation token", request_id)?;
            let tokens = g.words(count as usize);
            if request_id == 0 || mask_request_id == 0 {
                return Err(fatal(InvalidRequestId, "mask request ids must be non-zero", request_id));
            }
            if words_per_mask == 0 {
                return Err(fatal(InvalidCount, "mask request dimensions are invalid", request_id));
            }
            if words_per_mask as u64 * (tokens.len() as u64 + 1) > MAX_MASK_WORDS {
                return Err(fatal(LimitExceeded, "mask request output would exceed the mask limit", request_id));
            }
            EngineEvent::MaskRequest(MaskRequestEvent { request_id, mask_request_id, words_per_mask, simulation_tokens: tokens })
        }
        FrameType::Done => {
            let request_id = g.u64();
            let (raw_reason, bounded) = (g.u8(), g.u8());
            let (prompt_tokens, completion_tokens) = (g.u32(), g.u32());
            let (prefill_micros, decode_micros, wall_micros) = (g.u64(), g.u64(), g.u64());
            let (deduplicated, drafted, accepted, cycles, logit_count) = (g.u32(), g.u32(), g.u32(), g.u32(), g.u32());
            tail_check(&g, logit_count, "option logit", request_id)?;
            let logits: Vec<f32> = (0..logit_count).map(|_| g.f32()).collect();
            let Some(reason) = FinishReason::from_u8(raw_reason) else {
                return Err(fatal(InvalidEnumValue, "done finish reason is invalid", request_id));
            };
            if request_id == 0 {
                return Err(fatal(InvalidRequestId, "done event request id must be non-zero", 0));
            }
            if bounded > 1 || (bounded == 1 && reason != FinishReason::Length) {
                return Err(fatal(InvalidEnumValue, "only a length-finished done event is bounded", request_id));
            }
            // Each cycle verified at least one drafted token and accepted at most all.
            if accepted > drafted || cycles > drafted || (drafted != 0 && cycles == 0) {
                return Err(fatal(InvalidCount, "done speculative counts are inconsistent", request_id));
            }
            if !logits.is_empty()
                && (logits.len() < MIN_SCORE_TOKENS
                    || !logits.iter().all(|v| v.is_finite())
                    || reason != FinishReason::Stop
                    || completion_tokens != 0
                    || decode_micros != 0
                    || drafted != 0)
            {
                return Err(fatal(
                    InvalidCount,
                    "scored done events need 2..255 finite logits, stop, and no completion or decode activity",
                    request_id,
                ));
            }
            EngineEvent::Done(DoneEvent {
                request_id,
                reason,
                prompt_tokens,
                completion_tokens,
                prefill_micros,
                decode_micros,
                wall_micros,
                option_logits: logits,
                deduplicated_tokens: deduplicated,
                drafted_tokens: drafted,
                accepted_draft_tokens: accepted,
                draft_cycles: cycles,
                bounded: bounded == 1,
            })
        }
        FrameType::Error => {
            let (failure, retryable, retry_after) = (g.u8(), g.u8(), g.u32());
            let (request_id, code_bytes, message_bytes) = (g.u64(), g.u32(), g.u32());
            if retryable > 1 || code_bytes as u64 + message_bytes as u64 != g.rest() as u64 {
                return Err(fatal(InvalidPayloadLength, "error string lengths do not match the frame payload", 0));
            }
            let Some(failure_class) = FailureClass::from_u8(failure) else {
                return Err(fatal(InvalidErrorClassification, "error event classification is invalid", request_id));
            };
            if (failure_class == FailureClass::RequestError) != (request_id != 0) {
                return Err(fatal(
                    InvalidErrorClassification,
                    "only request errors may carry a non-zero request id",
                    request_id,
                ));
            }
            if code_bytes == 0 || code_bytes as u64 > MAX_ERROR_STRING_BYTES || message_bytes as u64 > MAX_ERROR_STRING_BYTES
            {
                return Err(fatal(LimitExceeded, "error code or message exceeds its safe limit", request_id));
            }
            if retry_after != 0 && retryable == 0 {
                return Err(fatal(InvalidErrorClassification, "only a retryable error says when to retry", request_id));
            }
            let code = g.bytes(code_bytes as usize).to_vec();
            let message = g.bytes(message_bytes as usize).to_vec();
            EngineEvent::Error(ErrorEvent {
                failure_class,
                request_id,
                retryable: retryable == 1,
                code,
                message,
                retry_after_seconds: retry_after,
            })
        }
        FrameType::StatusJson => {
            let correlation_id = g.u64();
            let json = g.bytes(g.rest()).to_vec();
            if json.is_empty() {
                return Err(fatal(LimitExceeded, "status JSON is empty", 0));
            }
            EngineEvent::StatusJson(StatusJsonEvent { correlation_id, json })
        }
        _ => return Err(fatal(UnknownFrameType, "frame type is not a native protocol event", 0)),
    })
}

/// Decode one client frame as the engine does: the layout, then `validate_request` for a REQUEST. For the fake
/// engine and tests.
pub fn decode_client_frame(frame: &Frame) -> Result<ClientMessage, ProtocolError> {
    use IssueCode::*;
    checked_frame(frame)?;
    let mut g = Get::new(&frame.payload);
    let length_error = |id: u64| fail_request(InvalidPayloadLength, "request payload length does not match its counts", id);
    Ok(match frame.frame_type {
        FrameType::Request => {
            let request_id = g.u64();
            let (raw_priority, raw_constraint) = (g.u8(), g.u8());
            let (absolute, remaining) = (g.u64(), g.u64());
            let (logical_max, prompt_count, span_count) = (g.u32(), g.u32(), g.u32());
            let sampling = SamplingParameters {
                temperature: g.f32(),
                top_p: g.f32(),
                top_k: g.u32(),
                presence_penalty: g.f32(),
                frequency_penalty: g.f32(),
                repetition_penalty: g.f32(),
                min_p: g.f32(),
            };
            let seed = g.u64();
            let (score_count, generation, flags) = (g.u32(), g.u32(), g.u32());
            let cache_salt = g.u64() as u128 | (g.u64() as u128) << 64;
            let (thinking_tokens, end_count, also_ends) = (g.u32(), g.u32(), g.u32());
            let (Some(priority), Some(constraint)) =
                (RequestPriority::from_u8(raw_priority), ConstraintMode::from_u8(raw_constraint))
            else {
                return Err(fail_request(InvalidEnumValue, "request priority or constraint is not defined", request_id));
            };
            if (g.rest() as u64) < 4 * prompt_count as u64 + IMAGE_SPAN_BYTES as u64 * span_count as u64 {
                return Err(length_error(request_id));
            }
            let prompt_tokens = g.words(prompt_count as usize);
            let image_spans: Vec<ImageSpan> = (0..span_count)
                .map(|_| ImageSpan {
                    offset: g.u32(),
                    tokens: g.u32(),
                    grid_height: g.u32(),
                    grid_width: g.u32(),
                    digest_lo: g.u64(),
                    digest_hi: g.u64(),
                })
                .collect();
            let pixels: u64 = image_spans.iter().map(ImageSpan::pixel_bytes).sum();
            let thinking = thinking_tokens != NO_THINKING_BUDGET;
            let end_words = if thinking { end_count as u64 } else { 0 };
            if g.rest() as u64 != pixels + 4 * score_count as u64 + 4 * end_words
                || (!thinking && (end_count != 0 || also_ends != 0))
            {
                return Err(length_error(request_id));
            }
            let image_pixels = g.bytes(pixels as usize).to_vec();
            let score_tokens = g.words(score_count as usize);
            let thinking_budget = thinking.then(|| ThinkingBudget {
                tokens: thinking_tokens,
                end: g.words(end_count as usize),
                also_ends,
            });
            let request = RequestFrame {
                request_id,
                priority,
                absolute_deadline_unix_micros: absolute,
                remaining_deadline_micros: remaining,
                logical_max_output_tokens: logical_max,
                prompt_tokens,
                sampling,
                seed,
                constraint,
                image_spans,
                image_pixels,
                score_tokens,
                generation_prompt_tokens: generation,
                flags: RequestFlags(flags),
                cache_salt,
                thinking_budget,
            };
            validate_request(&request)?;
            ClientMessage::Request(request)
        }
        FrameType::Cancel => ClientMessage::Cancel(CancelFrame { request_id: g.u64() }),
        FrameType::MaskResponse => {
            let (request_id, mask_request_id, count) = (g.u64(), g.u64(), g.u32());
            if g.rest() as u64 != 4 * count as u64 {
                return Err(fail_request(InvalidPayloadLength, "mask response word count does not match", request_id));
            }
            ClientMessage::MaskResponse(MaskResponseFrame { request_id, mask_request_id, mask_words: g.words(count as usize) })
        }
        FrameType::StatusRequest => ClientMessage::StatusRequest(StatusRequestFrame { correlation_id: g.u64() }),
        FrameType::ReleaseRequest => ClientMessage::ReleaseRequest(ReleaseRequestFrame { correlation_id: g.u64() }),
        _ => return Err(fatal(UnknownFrameType, "frame type is not a native protocol client frame", 0)),
    })
}

// ---- incremental parsing ----

/// Incremental, one-frame-at-a-time parser. The server's parser takes engine events only (a client frame type is an
/// unknown frame type); `client_frames()` is the engine's direction, for the fake engine.
#[derive(Debug)]
pub struct FrameParser {
    events: bool,
    header: Vec<u8>,
    reading_payload: bool,
    current_type: FrameType,
    expected_payload_bytes: usize,
    payload: Vec<u8>,
    terminal_issue: Option<ProtocolIssue>,
}

impl Default for FrameParser {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameParser {
    /// A parser of engine events.
    pub fn new() -> Self {
        Self {
            events: true,
            header: Vec::with_capacity(FRAME_HEADER_BYTES),
            reading_payload: false,
            current_type: FrameType::Ready,
            expected_payload_bytes: 0,
            payload: Vec::new(),
            terminal_issue: None,
        }
    }

    /// A parser of client frames, as the engine reads them.
    pub fn client_frames() -> Self {
        Self { events: false, ..Self::new() }
    }

    fn fail(&mut self, consumed: usize, found: ProtocolIssue) -> ParseStep {
        self.terminal_issue = Some(found.clone());
        ParseStep { consumed, frame: None, issue: Some(found) }
    }

    fn take_frame(&mut self) -> Frame {
        let frame = Frame { frame_type: self.current_type, payload: std::mem::take(&mut self.payload) };
        self.header.clear();
        self.reading_payload = false;
        self.expected_payload_bytes = 0;
        frame
    }

    fn parse_header(&mut self) -> Option<ProtocolIssue> {
        let h = &self.header;
        let fatal_issue = |code, message: &str| Some(issue(FailureClass::ProtocolFatal, code, message, 0));
        if &h[0..4] != MAGIC {
            return fatal_issue(IssueCode::BadMagic, "frame magic is not SPLH");
        }
        if u16::from_le_bytes([h[4], h[5]]) != PROTOCOL_VERSION {
            return fatal_issue(IssueCode::UnsupportedVersion, "unsupported native protocol version");
        }
        let payload_bytes = u64::from_le_bytes(h[8..16].try_into().unwrap());
        let frame_type = match FrameType::from_u16(u16::from_le_bytes([h[6], h[7]])) {
            Some(t) if t.is_event() == self.events => t,
            _ if self.events => {
                return fatal_issue(IssueCode::UnknownFrameType, "frame type is not a native protocol event")
            }
            _ => return fatal_issue(IssueCode::UnknownFrameType, "frame type is not a native protocol client frame"),
        };
        self.current_type = frame_type;
        if let Some(found) = payload_length_issue(frame_type, payload_bytes) {
            return Some(found);
        }
        // Bounded by the type's limit above, so reserving it whole is safe.
        self.expected_payload_bytes = payload_bytes as usize;
        self.payload = Vec::with_capacity(self.expected_payload_bytes);
        self.reading_payload = true;
        None
    }

    /// Consume up to one frame and report exactly how many bytes were used. After a failure every call reports the
    /// same issue again and consumes nothing (Python raises).
    pub fn consume(&mut self, data: &[u8]) -> ParseStep {
        if let Some(found) = &self.terminal_issue {
            return ParseStep { consumed: 0, frame: None, issue: Some(found.clone()) };
        }
        let mut consumed = 0;
        while consumed < data.len() {
            if !self.reading_payload {
                let count = (FRAME_HEADER_BYTES - self.header.len()).min(data.len() - consumed);
                self.header.extend_from_slice(&data[consumed..consumed + count]);
                consumed += count;
                if self.header.len() < FRAME_HEADER_BYTES {
                    return ParseStep { consumed, ..Default::default() };
                }
                if let Some(found) = self.parse_header() {
                    return self.fail(consumed, found);
                }
                if self.expected_payload_bytes == 0 {
                    return ParseStep { consumed, frame: Some(self.take_frame()), issue: None };
                }
            }
            let needed = self.expected_payload_bytes - self.payload.len();
            let count = needed.min(data.len() - consumed);
            self.payload.extend_from_slice(&data[consumed..consumed + count]);
            consumed += count;
            if self.payload.len() == self.expected_payload_bytes {
                return ParseStep { consumed, frame: Some(self.take_frame()), issue: None };
            }
        }
        ParseStep { consumed, ..Default::default() }
    }

    /// Finish EOF processing, making any partial frame terminally fatal.
    pub fn finish(&mut self) -> Option<ProtocolIssue> {
        if let Some(found) = &self.terminal_issue {
            return Some(found.clone());
        }
        if self.header.is_empty() && !self.reading_payload {
            return None;
        }
        let message = if !self.reading_payload {
            format!("stream ended after {} of {FRAME_HEADER_BYTES} frame-header bytes", self.header.len())
        } else {
            format!(
                "stream ended after {} of {} {} payload bytes",
                self.payload.len(),
                self.expected_payload_bytes,
                self.current_type.name()
            )
        };
        let found = issue(FailureClass::ProtocolFatal, IssueCode::TruncatedFrame, message, 0);
        self.terminal_issue = Some(found.clone());
        Some(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RequestFrame {
        RequestFrame {
            request_id: 7,
            absolute_deadline_unix_micros: 1,
            remaining_deadline_micros: 1,
            logical_max_output_tokens: 16,
            prompt_tokens: vec![1, 2, 3],
            ..Default::default()
        }
    }

    fn header(frame: &[u8]) -> (u16, u64) {
        (u16::from_le_bytes([frame[6], frame[7]]), u64::from_le_bytes(frame[8..16].try_into().unwrap()))
    }

    #[test]
    fn fixed_sizes_match_the_struct_formats() {
        let r = serialize_message(&ClientMessage::Request(request())).unwrap();
        assert_eq!(r.len(), FRAME_HEADER_BYTES + REQUEST_FIXED_BYTES + 12);
        let done = encode_event(&EngineEvent::Done(DoneEvent {
            request_id: 1,
            reason: FinishReason::Stop,
            prompt_tokens: 0,
            completion_tokens: 0,
            prefill_micros: 0,
            decode_micros: 0,
            wall_micros: 0,
            option_logits: vec![],
            deduplicated_tokens: 0,
            drafted_tokens: 0,
            accepted_draft_tokens: 0,
            draft_cycles: 0,
            bounded: false,
        }));
        assert_eq!(header(&done).1, DONE_FIXED_BYTES as u64);
        let ready = encode_event(&EngineEvent::Ready(ReadyEvent { max_concurrent_requests: 1, max_context_tokens: 1, vision: false }));
        assert_eq!(header(&ready).1, READY_BYTES as u64);
        let error = encode_event(&EngineEvent::Error(ErrorEvent {
            failure_class: FailureClass::EngineUnhealthy,
            request_id: 0,
            retryable: false,
            code: vec![],
            message: vec![],
            retry_after_seconds: 0,
        }));
        assert_eq!(header(&error).1, ERROR_FIXED_BYTES as u64);
        let start = encode_event(&EngineEvent::Start(StartEvent { request_id: 1, lane: 0, matched_prompt_tokens: 0 }));
        assert_eq!(header(&start).1, START_BYTES as u64);
    }

    #[test]
    fn request_round_trips_through_the_engine_decoder() {
        let mut r = request();
        r.prompt_tokens = vec![9, 8, 7, 6, 5];
        r.image_spans = vec![ImageSpan { offset: 1, tokens: 1, grid_height: 2, grid_width: 2, digest_lo: 3, digest_hi: 4 }];
        r.image_pixels = (0..3072).map(|i| i as u8).collect();
        r.thinking_budget = Some(ThinkingBudget { tokens: 5, end: vec![10, 11], also_ends: 12 });
        r.cache_salt = u128::MAX - 5;
        r.flags = RequestFlags::RETURN_PROGRESS;
        r.sampling.temperature = 0.5;
        let bytes = serialize_message(&ClientMessage::Request(r.clone())).unwrap();
        let mut parser = FrameParser::client_frames();
        let step = parser.consume(&bytes);
        assert_eq!(step.consumed, bytes.len());
        assert_eq!(decode_client_frame(&step.frame.unwrap()).unwrap(), ClientMessage::Request(r));
    }

    #[test]
    fn parser_takes_frames_byte_by_byte_and_reports_truncation() {
        let a = encode_event(&EngineEvent::Start(StartEvent { request_id: 3, lane: 1, matched_prompt_tokens: 2 }));
        let b = encode_event(&EngineEvent::Tokens(TokensEvent { request_id: 3, sequence_offset: 0, tokens: vec![5, 6] }));
        let stream: Vec<u8> = a.iter().chain(&b).copied().collect();
        let mut parser = FrameParser::new();
        let mut frames = vec![];
        for byte in &stream {
            let step = parser.consume(std::slice::from_ref(byte));
            assert_eq!(step.consumed, 1);
            frames.extend(step.frame);
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(
            decode_frame(&frames[1]).unwrap(),
            EngineEvent::Tokens(TokensEvent { request_id: 3, sequence_offset: 0, tokens: vec![5, 6] })
        );
        assert_eq!(parser.finish(), None);
        parser.consume(&a[..20]);
        let found = parser.finish().unwrap();
        assert_eq!(found.code, IssueCode::TruncatedFrame);
        assert_eq!(found.message, "stream ended after 4 of 16 start payload bytes");
    }

    #[test]
    fn parser_rejects_bad_headers() {
        let mut frame = encode_event(&EngineEvent::Ready(ReadyEvent { max_concurrent_requests: 1, max_context_tokens: 1, vision: true }));
        frame[0] = b'X';
        assert_eq!(FrameParser::new().consume(&frame).issue.unwrap().code, IssueCode::BadMagic);
        let mut frame = encode_event(&EngineEvent::Ready(ReadyEvent { max_concurrent_requests: 1, max_context_tokens: 1, vision: true }));
        frame[4] = 10;
        assert_eq!(FrameParser::new().consume(&frame).issue.unwrap().code, IssueCode::UnsupportedVersion);
        let cancel = serialize_message(&ClientMessage::Cancel(CancelFrame { request_id: 1 })).unwrap();
        let mut parser = FrameParser::new();
        let step = parser.consume(&cancel);
        assert_eq!(step.consumed, FRAME_HEADER_BYTES);
        assert_eq!(step.issue.unwrap().code, IssueCode::UnknownFrameType);
        // A failed parser stays failed.
        assert_eq!(parser.consume(&cancel).consumed, 0);
        let mut big = frame_buffer(FrameType::Start, 17);
        big.extend_from_slice(&[0; 17]);
        let found = FrameParser::new().consume(&big).issue.unwrap();
        assert_eq!((found.code, found.message.as_str()), (IssueCode::FrameTooLarge, "start payload length 17 exceeds its safe limit 16"));
    }

    #[test]
    fn decoder_checks_event_invariants() {
        let tokens = |offset, tokens: Vec<u32>| {
            let bytes = encode_event(&EngineEvent::Tokens(TokensEvent { request_id: 2, sequence_offset: offset, tokens }));
            decode_frame(&Frame { frame_type: FrameType::Tokens, payload: bytes[16..].to_vec() })
        };
        assert_eq!(tokens(0, vec![]).unwrap_err().issue.code, IssueCode::LimitExceeded);
        assert_eq!(tokens(u32::MAX, vec![1]).unwrap_err().issue.code, IssueCode::IntegerOverflow);
        let mut payload = encode_event(&EngineEvent::Tokens(TokensEvent { request_id: 2, sequence_offset: 0, tokens: vec![1, 2] }))[16..].to_vec();
        payload.truncate(payload.len() - 4);
        let error = decode_frame(&Frame { frame_type: FrameType::Tokens, payload }).unwrap_err();
        assert_eq!(error.issue.describe(), "protocol_fatal:invalid_payload_length request=2: token count does not match the binary event payload");
        let error_event = |class, request_id, retryable, retry_after| {
            let bytes = encode_event(&EngineEvent::Error(ErrorEvent {
                failure_class: class,
                request_id,
                retryable,
                code: b"x".to_vec(),
                message: vec![],
                retry_after_seconds: retry_after,
            }));
            decode_frame(&Frame { frame_type: FrameType::Error, payload: bytes[16..].to_vec() })
        };
        assert!(error_event(FailureClass::RequestError, 1, true, 3).is_ok());
        assert_eq!(error_event(FailureClass::RequestError, 0, false, 0).unwrap_err().issue.code, IssueCode::InvalidErrorClassification);
        assert_eq!(error_event(FailureClass::EngineUnhealthy, 4, false, 0).unwrap_err().issue.code, IssueCode::InvalidErrorClassification);
        assert_eq!(error_event(FailureClass::RequestError, 1, false, 3).unwrap_err().issue.message, "only a retryable error says when to retry");
        let status = Frame { frame_type: FrameType::StatusJson, payload: vec![0; 8] };
        assert_eq!(decode_frame(&status).unwrap_err().issue.message, "status JSON is empty");
        let client = Frame { frame_type: FrameType::Cancel, payload: vec![1, 0, 0, 0, 0, 0, 0, 0] };
        assert_eq!(decode_frame(&client).unwrap_err().issue.code, IssueCode::UnknownFrameType);
    }

    #[test]
    fn refresh_rewrites_only_the_absolute_deadline() {
        let mut r = request();
        r.remaining_deadline_micros = 500;
        r.sampling.top_p = f32::from_bits(0x3f7f_ffff);
        let bytes = serialize_message(&ClientMessage::Request(r)).unwrap();
        let refreshed = refresh_request_deadline(&bytes, 1_000);
        assert_eq!(&refreshed[26..34], &1_500u64.to_le_bytes());
        assert_eq!(refreshed[..26], bytes[..26]);
        assert_eq!(refreshed[34..], bytes[34..]);
        let cancel = serialize_message(&ClientMessage::Cancel(CancelFrame { request_id: 1 })).unwrap();
        assert_eq!(refresh_request_deadline(&cancel, 5), cancel);
    }

    #[test]
    fn client_frame_failures() {
        let e = serialize_message(&ClientMessage::Cancel(CancelFrame { request_id: 0 })).unwrap_err();
        assert_eq!((e.issue.failure_class, e.issue.code), (FailureClass::ProtocolFatal, IssueCode::InvalidRequestId));
        let mask = |request_id, mask_request_id, words: Vec<u32>| {
            serialize_message(&ClientMessage::MaskResponse(MaskResponseFrame { request_id, mask_request_id, mask_words: words }))
        };
        let e = mask(3, 0, vec![1]).unwrap_err();
        assert_eq!((e.issue.failure_class, e.issue.code), (FailureClass::RequestError, IssueCode::InvalidRequestId));
        assert_eq!(mask(3, 1, vec![]).unwrap_err().issue.message, "mask response word count exceeds its limit");
        let mut huge = request();
        huge.prompt_tokens = vec![1; 2];
        huge.image_spans = (0..16).map(|i| ImageSpan { offset: 0, tokens: 0, grid_height: 128, grid_width: 128, digest_lo: i, digest_hi: 0 }).collect();
        // Spans this large fail the span check before the frame size is known; the frame limit is checked after.
        assert_eq!(validate_request(&huge).unwrap_err().issue.code, IssueCode::InvalidCount);
    }
}

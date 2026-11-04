//! A generation request's own fields, checked before its prompt is prepared (frontend._generation_options,
//! _output_budget, _priority, _cache_salt, request_deadline).
use super::shapes::ApiError;
use crate::protocol::{RequestPriority, SamplingParameters};
use serde_json::{Map, Value};

const MIN_FLOAT32_SUBNORMAL: f64 = 1.401298464324817e-45;
const FLOAT32_MAX: f64 = 3.4028234663852886e38;
/// vLLM raises a nonzero temperature below this to it; Splash does the same.
const MIN_SAMPLING_TEMPERATURE: f64 = 0.01;
/// Qwen's generation config's top_k, as temperature's and top_p's defaults are.
const TOP_K_DEFAULT: i64 = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct GenerationOptions {
    pub sampling: SamplingParameters,
    pub stop_sequences: Vec<String>,
    pub ignore_eos: bool,
    pub seed: u64,
    pub priority: RequestPriority,
    pub cache_salt: u128,
    pub thinking_token_budget: Option<u32>,
}

fn finite(v: &Value) -> Option<f64> {
    // A JSON bool is no number, as Python's is_finite_number refuses it.
    v.as_f64().filter(|f| f.is_finite())
}

/// `default_budget` is the server's --thinking-token-budget.
pub fn generation_options(body: &Map<String, Value>, default_budget: Option<u32>) -> Result<GenerationOptions, ApiError> {
    let number = |name: &str, default: f64, accepts: &dyn Fn(f64) -> bool, requirement: &str| -> Result<f64, ApiError> {
        match body.get(name) {
            None => Ok(default),
            Some(v) => finite(v).filter(|f| accepts(*f)).ok_or_else(|| ApiError::bad(format!("{name} must be {requirement}"))),
        }
    };
    let mut temperature = number("temperature", 1.0, &|v| (0.0..=2.0).contains(&v), "a number in [0, 2]")?;
    let top_p = number("top_p", 0.95, &|v| (MIN_FLOAT32_SUBNORMAL..=1.0).contains(&v), "a number in (0, 1]")?;
    let presence = number("presence_penalty", 0.0, &|v| (-2.0..=2.0).contains(&v), "a number in [-2, 2]")?;
    let frequency = number("frequency_penalty", 0.0, &|v| (-2.0..=2.0).contains(&v), "a number in [-2, 2]")?;
    let repetition = number("repetition_penalty", 1.0, &|v| (MIN_FLOAT32_SUBNORMAL..=FLOAT32_MAX).contains(&v), "a positive number")?;
    let min_p = number("min_p", 0.0, &|v| (0.0..=1.0).contains(&v), "a number in [0, 1]")?;
    if 0.0 < temperature && temperature < MIN_SAMPLING_TEMPERATURE {
        temperature = MIN_SAMPLING_TEMPERATURE;
    }
    // 0 or -1 keeps every token, which the frame says with 0; one past u32 keeps every token too.
    let top_k = match body.get("top_k") {
        None => TOP_K_DEFAULT,
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_u64().map(|_| i64::MAX))
            .filter(|k| *k >= -1 && !v.is_f64())
            .ok_or_else(|| ApiError::bad("top_k must be 0 or -1 (disabled) or a positive integer"))?,
    };
    let top_k = top_k.clamp(0, u32::MAX as i64) as u32;
    let stop_sequences = match body.get("stop") {
        None => vec![],
        Some(Value::Array(a)) if a.is_empty() => vec![],
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(Value::Array(a)) if (1..=4).contains(&a.len()) && a.iter().all(|v| v.as_str().is_some_and(|s| !s.is_empty())) => {
            a.iter().map(|v| v.as_str().unwrap().to_string()).collect()
        }
        _ => return Err(ApiError::bad("stop must be a string or up to four strings")),
    };
    // Splash implements no logit_bias; only an empty one is accepted, a non-empty one refused rather than ignored.
    if body.get("logit_bias").is_some_and(|v| v.as_object().is_none_or(|o| !o.is_empty())) {
        return Err(ApiError::bad("logit_bias is not supported"));
    }
    let ignore_eos = match body.get("ignore_eos") {
        None => false,
        Some(Value::Bool(b)) => *b,
        _ => return Err(ApiError::bad("ignore_eos must be a boolean")),
    };
    // -1 asks for a random seed; another negative one is taken as its 64 bits.
    let seed = match body.get("seed") {
        None => rand::random(),
        Some(v) if v.is_i64() && v.as_i64() == Some(-1) => rand::random(),
        Some(v) if v.is_i64() => v.as_i64().unwrap() as u64,
        Some(v) if v.is_u64() => v.as_u64().unwrap(),
        _ => return Err(ApiError::bad("seed must be a 64-bit integer")),
    };
    // -1 sets no cap; an absent budget takes the server's.
    let thinking_token_budget = match body.get("thinking_token_budget") {
        None => default_budget,
        Some(v) if v.as_i64() == Some(-1) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|_| !v.is_f64())
                .map(|b| b.min(u32::MAX as u64 - 1) as u32)
                .ok_or_else(|| ApiError::bad("thinking_token_budget must be a non-negative integer or -1 for no cap"))?,
        ),
    };
    Ok(GenerationOptions {
        sampling: SamplingParameters {
            temperature: temperature as f32,
            top_p: top_p as f32,
            top_k,
            presence_penalty: presence as f32,
            frequency_penalty: frequency as f32,
            repetition_penalty: repetition as f32,
            min_p: min_p as f32,
        },
        stop_sequences,
        ignore_eos,
        seed,
        priority: priority(body)?,
        cache_salt: cache_salt(body)?,
        thinking_token_budget,
    })
}

pub fn priority(body: &Map<String, Value>) -> Result<RequestPriority, ApiError> {
    match body.get("priority").map(|v| v.as_str()) {
        None | Some(Some("normal")) => Ok(RequestPriority::Normal),
        Some(Some("foreground")) => Ok(RequestPriority::Foreground),
        Some(Some("background")) => Ok(RequestPriority::Background),
        _ => Err(ApiError::bad("priority must be foreground, normal, or background")),
    }
}

/// The request's cache_salt as the engine keys its cache: 128 bits of its BLAKE2b hash, zero without one.
pub fn cache_salt(body: &Map<String, Value>) -> Result<u128, ApiError> {
    match body.get("cache_salt") {
        None => Ok(0),
        Some(Value::String(s)) if !s.is_empty() => {
            let digest = blake2b_simd::Params::new().hash_length(16).personal(b"splash-salt").hash(s.as_bytes());
            Ok(u128::from_le_bytes(digest.as_bytes().try_into().expect("16 bytes")))
        }
        _ => Err(ApiError::bad("cache_salt must be a non-empty string")),
    }
}

/// The output token budget: a ceiling at the end of the context window the prompt leaves.
pub fn output_budget(requested: Option<&Value>, prompt_tokens: usize, max_context: u32, field: &str) -> Result<u32, ApiError> {
    if prompt_tokens >= max_context as usize {
        return Err(ApiError::context_length());
    }
    let remaining = max_context - prompt_tokens as u32;
    let max_new = match requested {
        None => remaining as u64,
        Some(v) => v.as_u64().filter(|n| *n > 0 && !v.is_f64()).ok_or_else(|| ApiError::bad(format!("{field} must be a positive integer")))?,
    };
    Ok(max_new.min(remaining as u64) as u32)
}

/// The request's own timeout, within the server's.
pub fn request_timeout(body: &Map<String, Value>, server: Option<std::time::Duration>) -> Result<Option<std::time::Duration>, ApiError> {
    let own = match body.get("timeout") {
        None => None,
        Some(v) => Some(finite(v).filter(|t| *t > 0.0).ok_or_else(|| ApiError::bad("timeout must be positive"))?),
    };
    Ok(match (own.map(std::time::Duration::from_secs_f64), server) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }
    #[test]
    fn defaults_follow_qwens_generation_config() {
        let o = generation_options(&body(json!({"seed": 7})), None).unwrap();
        assert_eq!((o.sampling.temperature, o.sampling.top_p, o.sampling.top_k, o.seed), (1.0, 0.95, 20, 7));
        let o = generation_options(&body(json!({"temperature": 0.001, "top_k": -1, "seed": -2, "stop": ["a", "b"]})), Some(5)).unwrap();
        assert_eq!((o.sampling.temperature, o.sampling.top_k, o.seed), (0.01, 0, u64::MAX - 1));
        assert_eq!((o.stop_sequences.len(), o.thinking_token_budget), (2, Some(5)));
        for bad in [json!({"temperature": 3}), json!({"top_k": 1.5}), json!({"stop": ""}), json!({"logit_bias": {"1": 2}}), json!({"temperature": true})] {
            assert!(generation_options(&body(bad.clone()), None).is_err(), "{bad}");
        }
    }
    #[test]
    fn cache_salt_hashes_as_splash_does() {
        assert_eq!(cache_salt(&body(json!({"cache_salt": "heimdall"}))).unwrap(), 12671509385503265705964326898390866999);
        assert_eq!(cache_salt(&body(json!({}))).unwrap(), 0);
    }
    #[test]
    fn budget_is_a_ceiling_at_the_context() {
        assert_eq!(output_budget(None, 10, 100, "max_tokens").unwrap(), 90);
        assert_eq!(output_budget(Some(&json!(500)), 10, 100, "max_tokens").unwrap(), 90);
        assert_eq!(output_budget(Some(&json!(5)), 10, 100, "max_tokens").unwrap(), 5);
        assert_eq!(output_budget(None, 100, 100, "max_tokens").unwrap_err().code, "context_length_exceeded");
        assert!(output_budget(Some(&json!(0)), 1, 100, "max_tokens").is_err());
    }
}

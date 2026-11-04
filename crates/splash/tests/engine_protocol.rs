//! The Rust protocol against Splash 1.4.1's protocol.py: golden bytes for representative frames and the reference's
//! verdict on every validation rule. Fixtures come from `tests/fixtures/protocol/generate.py`.
use serde_json::Value;
use splash::protocol::*;
use std::path::PathBuf;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/protocol")
}

fn golden(name: &str) -> Vec<u8> {
    let text = std::fs::read_to_string(fixtures().join(format!("{name}.hex"))).unwrap();
    hex::decode(text.trim()).unwrap()
}

fn frame_of(bytes: &[u8], client: bool) -> Frame {
    let mut parser = if client { FrameParser::client_frames() } else { FrameParser::new() };
    let step = parser.consume(bytes);
    assert_eq!(step.issue, None);
    assert_eq!(step.consumed, bytes.len());
    step.frame.expect("one whole frame")
}

fn full_request() -> RequestFrame {
    RequestFrame {
        request_id: 0x0102030405060708,
        priority: RequestPriority::Foreground,
        absolute_deadline_unix_micros: 1_761_000_000_000_000,
        remaining_deadline_micros: 30_000_000,
        logical_max_output_tokens: 256,
        prompt_tokens: vec![100, 101, 9, 9, 102, 103, 104, 105],
        sampling: SamplingParameters {
            temperature: 0.7,
            top_p: 0.95,
            top_k: 40,
            presence_penalty: 0.5,
            frequency_penalty: -0.25,
            repetition_penalty: 1.05,
            min_p: 0.05,
        },
        seed: 0xDEADBEEFCAFEF00D,
        constraint: ConstraintMode::TokenMask,
        image_spans: vec![ImageSpan {
            offset: 2,
            tokens: 2,
            grid_height: 2,
            grid_width: 4,
            digest_lo: 0x1122334455667788,
            digest_hi: 0x99AABBCCDDEEFF00,
        }],
        image_pixels: (0..2 * 16 * 4 * 16 * 3).map(|i: u32| ((i * 7) % 256) as u8).collect(),
        score_tokens: vec![],
        generation_prompt_tokens: 2,
        flags: RequestFlags::RETURN_PROGRESS,
        cache_salt: 0x0123456789ABCDEF_FEDCBA9876543210,
        thinking_budget: Some(ThinkingBudget { tokens: 512, end: vec![200, 201], also_ends: 202 }),
    }
}

#[test]
fn client_frames_match_python_bytes() {
    let score = RequestFrame {
        request_id: 9,
        priority: RequestPriority::Background,
        absolute_deadline_unix_micros: u64::MAX,
        remaining_deadline_micros: u64::MAX - 1_761_000_000_000_000,
        logical_max_output_tokens: 0,
        prompt_tokens: vec![1, 2, 3],
        score_tokens: vec![10, 20, 30],
        ..Default::default()
    };
    let cases = [
        ("request_full", ClientMessage::Request(full_request())),
        ("request_score", ClientMessage::Request(score)),
        ("cancel", ClientMessage::Cancel(CancelFrame { request_id: 7 })),
        (
            "mask_response",
            ClientMessage::MaskResponse(MaskResponseFrame {
                request_id: 7,
                mask_request_id: 3,
                mask_words: vec![0xFFFFFFFF, 1, 0x80000000, 0],
            }),
        ),
        ("status_request", ClientMessage::StatusRequest(StatusRequestFrame { correlation_id: 42 })),
        ("release_request", ClientMessage::ReleaseRequest(ReleaseRequestFrame { correlation_id: 43 })),
    ];
    for (name, message) in cases {
        let bytes = golden(name);
        assert_eq!(hex::encode(serialize_message(&message).unwrap()), hex::encode(&bytes), "{name}");
        assert_eq!(decode_client_frame(&frame_of(&bytes, true)).unwrap(), message, "{name} decodes back");
    }
    let request = golden("request_full");
    assert_eq!(u64::from_le_bytes(request[8..16].try_into().unwrap()) as usize, request.len() - FRAME_HEADER_BYTES);
    assert_eq!(request.len(), FRAME_HEADER_BYTES + REQUEST_FIXED_BYTES + 8 * 4 + IMAGE_SPAN_BYTES + 6144 + 2 * 4);
}

#[test]
fn engine_events_match_python_bytes() {
    let done = |request_id, reason, bounded, counts: [u64; 5], spec: [u32; 4], logits: Vec<f32>| DoneEvent {
        request_id,
        reason,
        prompt_tokens: counts[0] as u32,
        completion_tokens: counts[1] as u32,
        prefill_micros: counts[2],
        decode_micros: counts[3],
        wall_micros: counts[4],
        option_logits: logits,
        deduplicated_tokens: spec[0],
        drafted_tokens: spec[1],
        accepted_draft_tokens: spec[2],
        draft_cycles: spec[3],
        bounded,
    };
    let cases = [
        ("ready", EngineEvent::Ready(ReadyEvent { max_concurrent_requests: 4, max_context_tokens: 32768, vision: true })),
        ("start", EngineEvent::Start(StartEvent { request_id: 7, lane: 2, matched_prompt_tokens: 128 })),
        (
            "prompt_progress",
            EngineEvent::PromptProgress(PromptProgressEvent { request_id: 7, processed_tokens: 64, elapsed_micros: 1500 }),
        ),
        ("tokens", EngineEvent::Tokens(TokensEvent { request_id: 7, sequence_offset: 3, tokens: vec![100, 101, 102] })),
        (
            "mask_request",
            EngineEvent::MaskRequest(MaskRequestEvent {
                request_id: 7,
                mask_request_id: 3,
                words_per_mask: 2,
                simulation_tokens: vec![100],
            }),
        ),
        ("done_length", EngineEvent::Done(done(7, FinishReason::Length, false, [8, 3, 1000, 2000, 3500], [4, 6, 4, 2], vec![]))),
        ("done_bounded", EngineEvent::Done(done(7, FinishReason::Length, true, [8, 2, 1, 2, 3], [0; 4], vec![]))),
        (
            "done_score",
            EngineEvent::Done(done(9, FinishReason::Stop, false, [3, 0, 10, 0, 20], [0; 4], vec![1.5, -2.25, 0.0])),
        ),
        (
            "error_request",
            EngineEvent::Error(ErrorEvent {
                failure_class: FailureClass::RequestError,
                request_id: 7,
                retryable: true,
                code: b"kv_disk_quota".to_vec(),
                message: b"the SSD cache is full".to_vec(),
                retry_after_seconds: 5,
            }),
        ),
        (
            "error_engine",
            EngineEvent::Error(ErrorEvent {
                failure_class: FailureClass::EngineUnhealthy,
                request_id: 0,
                retryable: false,
                code: b"metal_failure".to_vec(),
                message: b"GPU fault".to_vec(),
                retry_after_seconds: 0,
            }),
        ),
        (
            "status_json",
            EngineEvent::StatusJson(StatusJsonEvent {
                correlation_id: 42,
                json: br#"{"schema_version":6,"ready":true}"#.to_vec(),
            }),
        ),
    ];
    for (name, event) in cases {
        let bytes = golden(name);
        assert_eq!(decode_frame(&frame_of(&bytes, false)).unwrap(), event, "{name}");
        assert_eq!(hex::encode(encode_event(&event)), hex::encode(&bytes), "{name} encodes back");
    }
    let done = golden("done_length");
    assert_eq!(done.len(), FRAME_HEADER_BYTES + DONE_FIXED_BYTES);
    assert_eq!(golden("error_engine").len(), FRAME_HEADER_BYTES + ERROR_FIXED_BYTES + 22);
}

fn number(value: &Value) -> f32 {
    match value {
        Value::String(s) => match s.as_str() {
            "nan" => f32::NAN,
            "inf" => f32::INFINITY,
            "-inf" => f32::NEG_INFINITY,
            other => other.parse::<f64>().unwrap() as f32,
        },
        other => other.as_f64().unwrap() as f32,
    }
}

fn words(value: &Value) -> Vec<u32> {
    match value {
        Value::Object(o) => vec![7; o["repeat"].as_u64().unwrap() as usize],
        other => other.as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect(),
    }
}

/// The same construction as generate.py's `request_from`.
fn request_from(case: &Value) -> RequestFrame {
    let u64_or = |key: &str, default: u64| case.get(key).map_or(default, |v| v.as_u64().unwrap());
    let mut sampling = SamplingParameters::default();
    if let Some(Value::Object(fields)) = case.get("sampling") {
        for (key, value) in fields {
            match key.as_str() {
                "temperature" => sampling.temperature = number(value),
                "top_p" => sampling.top_p = number(value),
                "top_k" => sampling.top_k = value.as_u64().unwrap() as u32,
                "presence_penalty" => sampling.presence_penalty = number(value),
                "frequency_penalty" => sampling.frequency_penalty = number(value),
                "repetition_penalty" => sampling.repetition_penalty = number(value),
                "min_p" => sampling.min_p = number(value),
                other => panic!("unknown sampling field {other}"),
            }
        }
    }
    RequestFrame {
        request_id: u64_or("request_id", 7),
        priority: RequestPriority::from_u8(u64_or("priority", 1) as u8).unwrap(),
        absolute_deadline_unix_micros: u64_or("absolute", 1000),
        remaining_deadline_micros: u64_or("remaining", 500),
        logical_max_output_tokens: u64_or("logical_max_output_tokens", 16) as u32,
        prompt_tokens: case.get("prompt_tokens").map_or(vec![1, 2, 3, 4, 5, 6], words),
        sampling,
        seed: u64_or("seed", 0),
        constraint: ConstraintMode::from_u8(u64_or("constraint", 0) as u8).unwrap(),
        image_spans: case.get("image_spans").map_or(vec![], |spans| {
            spans
                .as_array()
                .unwrap()
                .iter()
                .map(|s| {
                    let n = |i: usize| s[i].as_u64().unwrap();
                    ImageSpan {
                        offset: n(0) as u32,
                        tokens: n(1) as u32,
                        grid_height: n(2) as u32,
                        grid_width: n(3) as u32,
                        digest_lo: n(4),
                        digest_hi: n(5),
                    }
                })
                .collect()
        }),
        image_pixels: vec![0; u64_or("image_pixels", 0) as usize],
        score_tokens: case.get("score_tokens").map_or(vec![], words),
        generation_prompt_tokens: u64_or("generation_prompt_tokens", 0) as u32,
        flags: RequestFlags(u64_or("flags", 0) as u32),
        cache_salt: case.get("cache_salt").map_or(0, |v| v.as_str().unwrap().parse().unwrap()),
        thinking_budget: case.get("thinking_budget").map(|t| ThinkingBudget {
            tokens: t["tokens"].as_u64().unwrap() as u32,
            end: words(&t["end"]),
            also_ends: t["also_ends"].as_u64().unwrap() as u32,
        }),
    }
}

#[test]
fn validation_matches_python_rule_by_rule() {
    let read = |name: &str| -> Value { serde_json::from_str(&std::fs::read_to_string(fixtures().join(name)).unwrap()).unwrap() };
    let cases = read("validation_cases.json");
    let expected = read("validation_expected.json");
    let mut checked = 0;
    for case in cases.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let want = &expected[name];
        let got = serialize_message(&ClientMessage::Request(request_from(case)));
        match (want.get("ok"), got) {
            (Some(Value::String(bytes)), Ok(frame)) => assert_eq!(&hex::encode(frame), bytes, "{name}"),
            (Some(Value::Number(len)), Ok(frame)) => assert_eq!(frame.len() as u64, len.as_u64().unwrap(), "{name}"),
            (None, Err(error)) => {
                let issue = error.issue;
                assert_eq!(issue.failure_class.name(), want["class"], "{name}");
                assert_eq!(issue.code.name(), want["code"], "{name}");
                assert_eq!(issue.request_id, want["request_id"].as_u64().unwrap(), "{name}");
                assert_eq!(issue.message, want["message"], "{name}");
            }
            (want, got) => panic!("{name}: python {want:?}, rust {got:?}"),
        }
        checked += 1;
    }
    assert_eq!(checked, 57);
}

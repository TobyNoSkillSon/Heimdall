//! Chat-completions messages in the canonical form the template sees (api_shapes.normalize_messages,
//! template_messages).

use super::pyjson::{self, Dumps};
use super::ChatError;
use serde_json::{json, Map, Value};

/// Chat's and Responses' kinds of text part. An assistant's refusal is text it wrote, in a field of its own.
const TEXT_PARTS: [&str; 4] = ["text", "input_text", "output_text", "refusal"];

pub const VISION_UNAVAILABLE: &str = "this model is serving without vision (started with --language-only)";

/// Renders a `file` content part (a PDF) to canonical parts; the documents port supplies it.
pub type FileParts<'a> = &'a mut dyn FnMut(&Value) -> Result<Vec<Value>, ChatError>;

/// `value`'s field `key`, or None where it is absent or null, as a null field is one left out.
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|v| !v.is_null())
}

fn part_text(part: &Value) -> Result<String, ChatError> {
    let key = if part.get("type").and_then(Value::as_str) == Some("refusal") { "refusal" } else { "text" };
    match field(part, key) {
        None => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(ChatError::bad_request("text message content must be a string")),
    }
}

fn text_content(content: Option<&Value>) -> Result<String, ChatError> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                let kind = part.as_object().and_then(|p| p.get("type")).and_then(Value::as_str);
                if !kind.is_some_and(|k| TEXT_PARTS.contains(&k)) {
                    return Err(ChatError::bad_request("only text message content is supported"));
                }
                out.push_str(&part_text(part)?);
            }
            Ok(out)
        }
        Some(_) => Err(ChatError::bad_request("message content must be text")),
    }
}

fn require_vision(modality: &str, vision: bool) -> Result<(), ChatError> {
    if vision {
        Ok(())
    } else {
        Err(ChatError::bad_request(format!("{modality} input is not supported: {VISION_UNAVAILABLE}")))
    }
}

/// Text for plain user content; a canonical parts list when it carries images, so the template places each image
/// where the author put it.
fn user_content(content: Option<&Value>, vision: bool, files: FileParts) -> Result<Value, ChatError> {
    let Some(Value::Array(input)) = content else { return text_content(content).map(Value::String) };
    let mut parts = Vec::new();
    for part in input {
        let Some(object) = part.as_object() else {
            return Err(ChatError::bad_request("message content parts must be objects"));
        };
        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "image_url" | "input_image" => {
                require_vision("image", vision)?;
                let image_url = object.get("image_url");
                let url = match image_url {
                    Some(Value::Object(o)) => o.get("url"),
                    other => other,
                };
                let Some(Value::String(url)) = url else {
                    return Err(ChatError::bad_request("invalid image content part"));
                };
                parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            k if TEXT_PARTS.contains(&k) => parts.push(json!({"type": "text", "text": part_text(part)?})),
            "file" => {
                require_vision("PDF", vision)?;
                parts.extend(files(object.get("file").unwrap_or(&Value::Null))?);
            }
            "video" | "video_url" | "input_audio" => {
                return Err(ChatError::bad_request(format!("{kind} content is not supported")));
            }
            _ => return Err(ChatError::bad_request("unsupported message content part")),
        }
    }
    if parts.iter().any(|p| p.get("type").and_then(Value::as_str) != Some("text")) {
        return Ok(Value::Array(parts));
    }
    Ok(Value::String(parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect()))
}

/// A history call's arguments: an object, as sent or as JSON text of one; otherwise their text (the string as sent,
/// or another value's JSON), which template_messages keeps as text.
fn call_arguments(arguments: Option<&Value>) -> Value {
    match arguments {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(Value::String(text)) => {
            let trimmed = pyjson::py_strip(text);
            match pyjson::codec_loads(if trimmed.is_empty() { "{}" } else { trimmed }) {
                Ok(parsed @ Value::Object(_)) => parsed,
                _ => Value::String(text.clone()),
            }
        }
        Some(object @ Value::Object(_)) => object.clone(),
        Some(other) => Value::String(pyjson::codec_dumps(other)),
    }
}

/// `messages` checked and in canonical form (normalize_messages). `files` renders PDF parts.
pub fn normalize_messages(messages: Option<&Value>, vision: bool, files: FileParts) -> Result<Vec<Value>, ChatError> {
    let Some(Value::Array(messages)) = messages.filter(|m| m.as_array().is_some_and(|a| !a.is_empty())) else {
        return Err(ChatError::bad_request("messages must be a non-empty array"));
    };
    let mut normalized: Vec<Value> = Vec::new();
    let mut has_user = false;
    for message in messages {
        if !message.is_object() {
            return Err(ChatError::bad_request("each message must be an object"));
        }
        let role = message.get("role");
        let role_str = role.and_then(Value::as_str);
        if matches!(role_str, Some("system" | "developer")) {
            let text = text_content(message.get("content"))?;
            if normalized.len() == 1 && normalized[0]["role"] == "system" {
                // Responses instructions and developer items, or an Anthropic system and a system message, lead
                // as one system message.
                let lead = normalized[0]["content"].as_str().unwrap_or("").to_owned();
                let joined = [lead.as_str(), text.as_str()].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>();
                normalized[0]["content"] = Value::String(joined.join("\n\n"));
            } else {
                normalized.push(json!({"role": "system", "content": text}));
            }
            continue;
        }
        let Some(role @ ("user" | "assistant" | "tool")) = role_str else {
            let shown = match role {
                None | Some(Value::Null) => "None".to_owned(),
                Some(v) => pyjson::py_str(v),
            };
            return Err(ChatError::bad_request(format!("unsupported message role: {shown}")));
        };
        has_user |= role == "user";
        let content = message.get("content");
        let content = if role == "assistant" {
            Value::String(text_content(content)?)
        } else {
            user_content(content, vision, files)?
        };
        let mut item = Map::from_iter([("role".to_owned(), Value::from(role)), ("content".to_owned(), content)]);
        if role == "assistant" {
            // Clients send an assistant's reasoning back as Splash's reasoning_content or as reasoning text.
            let reasoning = match message.get("reasoning_content").filter(|v| !v.is_null()) {
                Some(r) => Some(r),
                None => message.get("reasoning").filter(|v| v.is_string()),
            };
            if let Some(reasoning) = reasoning {
                item.insert("reasoning_content".into(), Value::String(text_content(Some(reasoning))?));
            }
            if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
                let Value::Array(calls) = calls else {
                    return Err(ChatError::bad_request("assistant tool_calls must be an array"));
                };
                let mut out = Vec::new();
                for call in calls {
                    let function = call.as_object().and_then(|c| c.get("function")).filter(|f| f.is_object());
                    let Some(name) = function.and_then(|f| f.get("name")).and_then(Value::as_str) else {
                        return Err(ChatError::bad_request("invalid assistant tool call"));
                    };
                    let mut canonical = json!({
                        "type": "function",
                        "function": {"name": name, "arguments": call_arguments(function.and_then(|f| f.get("arguments")))},
                    });
                    if let Some(id) = call.get("id").filter(|v| !v.is_null()) {
                        let Value::String(id) = id else {
                            return Err(ChatError::bad_request("tool call id must be a string"));
                        };
                        canonical["id"] = Value::String(id.clone());
                    }
                    out.push(canonical);
                }
                item.insert("tool_calls".into(), Value::Array(out));
            }
        }
        if role == "tool" {
            if let Some(id) = message.get("tool_call_id").filter(|v| !v.is_null()) {
                let Value::String(id) = id else {
                    return Err(ChatError::bad_request("tool_call_id must be a string"));
                };
                item.insert("tool_call_id".into(), Value::String(id.clone()));
            }
        }
        normalized.push(Value::Object(item));
    }
    if !has_user {
        return Err(ChatError::bad_request("messages must include a user message"));
    }
    Ok(normalized)
}

fn has_any(message: &Map<String, Value>) -> bool {
    ["content", "reasoning_content", "tool_calls"].iter().any(|k| message.get(*k).is_some_and(pyjson::truthy))
}


/// Keep a call whose arguments are text, not an object, as text, without presenting it as a complete call
/// (template_messages).
pub fn template_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut result = Vec::with_capacity(messages.len());
    for message in messages {
        let calls = message.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        if !calls.iter().any(|c| c["function"]["arguments"].is_string()) {
            result.push(message);
            continue;
        }
        let mut current: Map<String, Value> =
            message.as_object().expect("message").iter().filter(|(k, _)| *k != "tool_calls").map(|(k, v)| (k.clone(), v.clone())).collect();
        for call in calls {
            if !call["function"]["arguments"].is_string() {
                let list = current.entry("tool_calls").or_insert_with(|| Value::Array(Vec::new()));
                list.as_array_mut().expect("calls").push(call);
                continue;
            }
            if has_any(&current) {
                result.push(Value::Object(current));
            }
            // json.dumps(call["function"], ensure_ascii=False): Python's default separators.
            let text = Dumps::default().dumps(&call["function"]);
            result.push(json!({"role": "assistant", "content": format!("Incomplete tool call:\n{text}")}));
            current = Map::from_iter([("role".to_owned(), Value::from("assistant")), ("content".to_owned(), Value::from(""))]);
        }
        if has_any(&current) {
            result.push(Value::Object(current));
        }
    }
    result
}

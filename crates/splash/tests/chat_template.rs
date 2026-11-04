//! Chat templates against Splash's Python server: the later-system probe and its patch, generation prompts, request
//! preparation, rendered text and prompt tokens.

#[path = "chat_support/mod.rs"]
mod support;

use serde_json::{json, Map, Value};
use splash::chat::prompt::{render_prompt, PromptRequest};
use splash::chat::pyjson::replace_lone_surrogates;
use splash::chat::template::{generation_key, ChatTemplate, ChatTemplates, Encoder, TemplateFiles};
use splash::chat::ChatError;
use std::sync::Arc;

fn files_with(source: &str) -> TemplateFiles {
    let config = TemplateFiles::read(&support::fixtures().join("qwen3.8")).expect("qwen files");
    TemplateFiles { templates: vec![(None, source.to_owned())], special_tokens: config.special_tokens }
}

fn encoder() -> (Encoder, bool) {
    match support::tokenizer() {
        Some(t) => (t.encoder(), true),
        None => (Arc::new(|_: &str| Vec::new()), false),
    }
}

/// The tools a request offers as normalize_tools passes them to the template (a tool without a type is a
/// function); the constrain port owns the rest of normalize_tools.
fn offered_tools(body: &Value) -> Option<Vec<Value>> {
    let tools = body.get("tools")?.as_array()?;
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .iter()
            .map(|tool| {
                if tool.get("type").is_some() {
                    return tool.clone();
                }
                let mut with_type = Map::from_iter([("type".to_owned(), json!("function"))]);
                with_type.extend(tool.as_object().unwrap().clone());
                Value::Object(with_type)
            })
            .collect(),
    )
}

/// Cases where Python's reason is Jinja2's own wording of a TemplateError, which minijinja words differently: the
/// status and the "messages could not be rendered: " prefix must still match.
const OWN_WORDING: [&str; 1] = ["undefined_error/plain"];

fn assert_error(label: &str, error: &ChatError, expected: &Value) {
    let message = expected["message"].as_str().unwrap();
    assert_eq!(u64::from(error.status), expected["status"].as_u64().unwrap(), "{label}");
    if OWN_WORDING.contains(&label) {
        let prefix = "messages could not be rendered: ";
        assert!(message.starts_with(prefix) && error.message.starts_with(prefix), "{label}: {} / {message}", error.message);
    } else {
        assert_eq!(error.message, message, "{label}");
    }
}

fn no_files(_: &Value) -> Result<Vec<Value>, ChatError> {
    Err(ChatError::bad_request("no documents in tests"))
}

fn request(case: &Value) -> (Value, Result<PromptRequest, ChatError>) {
    let text = case["body"].as_str().unwrap();
    let body: Value = serde_json::from_str(&replace_lone_surrogates(text)).expect("body");
    let mut files = no_files;
    let request = PromptRequest::from_body(&body, None, true, &mut files);
    (body, request)
}

#[test]
fn qwen_template_files_read() {
    let files = TemplateFiles::read(&support::fixtures().join("qwen3.8")).unwrap();
    assert_eq!(files.templates.len(), 1);
    assert!(files.templates[0].0.is_none());
    assert_eq!(files.special_tokens.get("eos_token"), Some(&json!("<|im_end|>")));
    assert_eq!(files.special_tokens.get("bos_token"), Some(&json!("<|endoftext|>")));
}

#[test]
fn probes_match_python() {
    let (encode, with_tokens) = encoder();
    for entry in support::fixture("templates.json").as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let files = files_with(entry["source"].as_str().unwrap());
        let template = ChatTemplate::prepare(&files.templates[0].1, &files.special_tokens, encode.clone()).unwrap();
        assert_eq!(template.later_system.as_str(), entry["later_system"], "{name}");
        assert_eq!(template.original.as_str(), entry["original"], "{name}");
        // The patch is written exactly as Python writes it.
        let expected_source = entry["patched_source"].as_str().unwrap_or(entry["source"].as_str().unwrap());
        assert_eq!(template.source.replace("\r\n", "\n"), expected_source.replace("\r\n", "\n"), "{name}");
        let mine: Vec<(Value, &str, Vec<u32>)> = template
            .effort_prompts()
            .filter_map(|(key, found)| Some((serde_json::from_str(key).unwrap(), found?.text.as_str(), found?.tokens.clone())))
            .collect();
        let python = entry["generation_prompts"].as_array().unwrap();
        assert_eq!(mine.len(), python.len(), "{name}: generation prompts");
        for expected in python {
            let found = mine.iter().find(|(k, _, _)| *k == expected["key"]).unwrap_or_else(|| panic!("{name}: {}", expected["key"]));
            assert_eq!(found.1, expected["text"], "{name}");
            if with_tokens {
                assert_eq!(found.2, support::ids(&expected["tokens"]), "{name}");
            }
        }
    }
}

#[test]
fn direct_renders_match_python() {
    let (encode, _) = encoder();
    for entry in support::fixture("templates.json").as_array().unwrap() {
        let files = files_with(entry["source"].as_str().unwrap());
        let template = ChatTemplate::prepare(&files.templates[0].1, &files.special_tokens, encode.clone()).unwrap();
        for render in entry["renders"].as_array().unwrap() {
            let messages = render["messages"].as_array().unwrap();
            let options = render["options"].as_object().unwrap();
            let got = template.render(messages, options);
            match render.get("rendered") {
                Some(text) => assert_eq!(got.as_deref().unwrap(), text.as_str().unwrap(), "{}", entry["name"]),
                None => assert_eq!(got.unwrap_err().to_string(), render["error"].as_str().unwrap()),
            }
        }
    }
}

/// Request preparation and rendered text, without the tokenizer: every case whose outcome the text decides.
#[test]
fn rendered_text_matches_python() {
    let encode: Encoder = Arc::new(|_: &str| Vec::new());
    for entry in support::fixture("templates.json").as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let templates = ChatTemplates::new(&files_with(entry["source"].as_str().unwrap()), encode.clone()).unwrap();
        for case in entry["cases"].as_array().unwrap() {
            let label = format!("{name}/{}", case["name"].as_str().unwrap());
            let (body, prepared) = request(case);
            let request = match prepared {
                Ok(request) => request,
                Err(error) => {
                    assert_eq!(json!({"status": error.status, "message": error.message}), case["error"], "{label}");
                    continue;
                }
            };
            if let Some(messages) = case.get("messages") {
                assert_eq!(Value::Array(request.messages.clone()), *messages, "{label}: normalized messages");
            }
            let tools = offered_tools(&body);
            let template = templates.select(tools.as_deref());
            let Some(rendered) = case.get("rendered") else { continue };
            let options = request.template_variables(tools.as_deref(), true);
            assert_eq!(template.render(&request.messages, &options).unwrap(), rendered.as_str().unwrap(), "{label}");
        }
    }
}

/// The whole of frontend._render_prompt: rendered text, prompt token ids, thinking and the generation prompt's token
/// count, or Python's error.
#[test]
fn prompts_match_python() {
    let Some(tokenizer) = support::tokenizer() else { return };
    for entry in support::fixture("templates.json").as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let templates = ChatTemplates::new(&files_with(entry["source"].as_str().unwrap()), tokenizer.encoder()).unwrap();
        for case in entry["cases"].as_array().unwrap() {
            let label = format!("{name}/{}", case["name"].as_str().unwrap());
            let (body, prepared) = request(case);
            let outcome = prepared.and_then(|request| {
                render_prompt(&templates, &tokenizer, &request, offered_tools(&body).as_deref(), true)
            });
            match outcome {
                Ok(prompt) => {
                    assert_eq!(prompt.text, case["rendered"].as_str().unwrap_or_else(|| panic!("{label}: {:?}", case["error"])), "{label}");
                    assert_eq!(prompt.tokens, support::ids(&case["tokens"]), "{label}: tokens");
                    assert_eq!(prompt.thinking, case["thinking"], "{label}");
                    assert_eq!(prompt.generation_prompt_tokens as u64, case["generation_prompt_tokens"].as_u64().unwrap(), "{label}");
                }
                Err(error) => assert_error(&label, &error, &case["error"]),
            }
        }
    }
}

#[test]
fn later_system_patch_renders_in_place() {
    let files = TemplateFiles::read(&support::fixtures().join("qwen3.8")).unwrap();
    let templates = ChatTemplates::new(&files, Arc::new(|_: &str| Vec::new())).unwrap();
    assert_eq!(templates.status(), json!({"later_system": "patched"}));
    assert!(templates.describe().starts_with(
        "patched to render later system messages in place (the original template rejects them), generation prompt"
    ));
}

#[test]
fn template_files_follow_transformers_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let config = json!({"chat_template": "config", "eos_token": {"content": "<e>"}});
    std::fs::write(dir.path().join("tokenizer_config.json"), config.to_string()).unwrap();
    let names = |files: &TemplateFiles| files.templates.iter().map(|(n, s)| (n.clone(), s.clone())).collect::<Vec<_>>();
    // The config's template alone.
    assert_eq!(names(&TemplateFiles::read(dir.path()).unwrap()), [(None, "config".to_owned())]);
    // Named files without chat_template.jinja replace it, with no default.
    std::fs::create_dir(dir.path().join("additional_chat_templates")).unwrap();
    std::fs::write(dir.path().join("additional_chat_templates/tool_use.jinja"), "tools").unwrap();
    let files = TemplateFiles::read(dir.path()).unwrap();
    assert_eq!(names(&files), [(Some("tool_use".to_owned()), "tools".to_owned())]);
    assert!(ChatTemplates::new(&files, Arc::new(|_: &str| Vec::new())).is_err());
    // A lone default file is the single template.
    std::fs::remove_file(dir.path().join("additional_chat_templates/tool_use.jinja")).unwrap();
    std::fs::write(dir.path().join("additional_chat_templates/default.jinja"), "named default").unwrap();
    assert_eq!(names(&TemplateFiles::read(dir.path()).unwrap()), [(None, "named default".to_owned())]);
    // chat_template.jinja is the default, which a same-named file overrides, beside the named ones.
    std::fs::write(dir.path().join("chat_template.jinja"), "file").unwrap();
    std::fs::write(dir.path().join("additional_chat_templates/tool_use.jinja"), "tools").unwrap();
    let files = TemplateFiles::read(dir.path()).unwrap();
    assert_eq!(names(&files), [(Some("default".to_owned()), "named default".to_owned()), (Some("tool_use".to_owned()), "tools".to_owned())]);
    assert_eq!(files.special_tokens.get("eos_token"), Some(&json!("<e>")));
}

#[test]
fn probe_keys_sort_nested_objects() {
    let a: Map<String, Value> = serde_json::from_value(json!({"z": {"b": 1, "a": [{"d": 1, "c": 2}]}, "enable_thinking": true, "tools": [1], "preserve_thinking": true})).unwrap();
    let b: Map<String, Value> = serde_json::from_value(json!({"enable_thinking": true, "z": {"a": [{"c": 2, "d": 1}], "b": 1}})).unwrap();
    assert_eq!(generation_key(&a), generation_key(&b));
    assert_eq!(generation_key(&b), r#"{"enable_thinking":true,"z":{"a":[{"c":2,"d":1}],"b":1}}"#);
}

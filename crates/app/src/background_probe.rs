//! `Heimdall --background-probe`: one small real call on Background Tasks ▸ Model, through the same client every
//! background job uses ("make one small real background call through the subscription and confirm
//! it works" before the any-provider client installs). Prints one line: the provider, model, request format and the
//! outcome (the answer, or the provider's error with any credential removed). Nothing is written to the profile.
//!
//! The ChatGPT sign-in is read from the Keychain item the app keeps and used only while its access token is fresh:
//! this process never refreshes it, so it cannot race the running app's own refresh of the same sign-in.
//!
//! A T3 choice ("t3:<instance>") runs one background thread through T3 with the app's stored T3 connection, read as it
//! is: this process never pairs or renews it (no T3 CLI), so the running app's connection is untouched. Its launch is
//! recorded for Heimdall's permission hook like the app's, and the thread is archived after the answer.
use anyhow::{Context, Result};
use serde_json::json;
use std::path::Path;

/// `--background-probe raw` (27 Oct, the user: the reset time must come from what OpenAI sends): one minimal streamed
/// Responses request on the Codex subscription's route, printed as it arrived (status, every header but cookies, the
/// first 6 kB of the body) with the token removed. Nothing is parsed or retried.
pub fn raw(data: &Path) -> Result<String> {
    let s = crate::settings::Settings::load(&data.join("settings.json")).context("read settings.json")?;
    let p = agent::provider::provider(agent::provider::CHATGPT).context("no ChatGPT provider")?;
    let session = agent::provider::console_session(p).context("no ChatGPT sign-in stored")?;
    anyhow::ensure!(session.expires_ms.saturating_sub(agent::now_ms()) > 120_000, "the ChatGPT access token expires within 2 minutes; not refreshing it here");
    let model = s.selected_api_model().filter(|m| m.provider == p.id).map(|m| m.id.clone()).unwrap_or_else(|| "gpt-6.1-sol".into());
    let body = json!({"model": model, "instructions": "Reply with OK.", "input": "ping", "stream": true, "store": false, "reasoning": {"effort": "low"}});
    let url = format!("{}/responses", agent::chatgpt::API_BASE);
    let r = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(60)).build()?
        .post(&url).bearer_auth(&session.access).json(&body).send()?;
    let mut out = format!("POST {url} model={model}\nstatus: {}\n", r.status());
    for (k, v) in r.headers() {
        if k.as_str().eq_ignore_ascii_case("set-cookie") { continue; }
        out += &format!("{}: {}\n", k, v.to_str().unwrap_or("<binary>"));
    }
    let text = r.text().unwrap_or_default();
    out += "\n";
    out += &text.chars().take(6000).collect::<String>();
    Ok(out.replace(&session.access, "[token]").replace(&session.refresh, "[refresh]"))
}

pub fn run(data: &Path) -> Result<String> {
    let s = crate::settings::Settings::load(&data.join("settings.json")).context("read settings.json")?;
    crate::background_model::publish(&s.background_model, &[]);
    crate::background_model::publish_efforts(&s.background_efforts);
    let m = &s.background_model;
    if ui::t3_instance(&m.provider).is_some() {
        let home = std::env::var_os("HOME").context("home directory unavailable")?;
        let runtime = std::path::PathBuf::from(home).join(".t3/userdata/server-runtime.json");
        let client = t3::Client::new(runtime, std::sync::Arc::new(t3::Keychain))?.with_launch_record(data.to_owned());
        crate::background_call::t3::set_client(std::sync::Arc::new(client), data);
    }
    let e = match agent::provider::provider(&m.provider).filter(|p| p.id == agent::provider::CHATGPT) {
        Some(p) => {
            let session = agent::provider::console_session(p).context("no ChatGPT sign-in stored")?;
            let left_ms = session.expires_ms.saturating_sub(agent::now_ms());
            anyhow::ensure!(left_ms > 120_000, "the ChatGPT access token expires within 2 minutes; not refreshing it here (the running app owns refreshes)");
            crate::background_model::endpoint_with_key(session.access)
        }
        None => crate::background_model::tool_endpoint(),
    };
    let model = e.model.clone().unwrap_or_default();
    let c = crate::background_call::Call {
        job: "background probe",
        model: model.clone(),
        system: "You answer with the JSON the schema asks for, nothing else.",
        schema: json!({"type":"object","properties":{"ok":{"type":"boolean"},"word":{"type":"string"}},"required":["ok","word"],"additionalProperties":false}),
        effort: "low",
        prompt: "Set ok to true and word to \"pong\".".into(),
        history: Vec::new(),
        session: None,
    };
    let head = format!("background probe: {} {} ({:?})", e.provider, model, e.wire);
    Ok(match crate::background_call::ask_blocking(&e, &c) {
        Ok(a) => format!("{head}: answered {} (input {} output {} tokens)", a.text, a.usage.input + a.usage.cache_read, a.usage.output),
        Err(err) => format!("{head}: failed: {}", format!("{err:#}").chars().take(600).collect::<String>()),
    })
}

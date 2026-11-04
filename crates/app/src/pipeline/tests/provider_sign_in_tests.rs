//! ChatGPT sign-in and model switching with Agent on (bug note 26 Oct 22:08: "I had to turn off the agent and only
//! then I could log in and I couldn't switch"). The whole flow through the real `Pipeline` with the agent running
//! (the fake Pi, `plumbing_tests::rig`) and a fake OpenAI: Connect, the browser's consent, the account's models, Add
//! Model, then a switch between them. Nothing on the path reads the Agent switch; the 26 Oct log shows the first
//! attempt refused by OpenAI's token endpoint (invalid_grant) and the second never finished in the browser.
use super::plumbing_tests::rig;
use super::*;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn wait<T>(what: &str, mut done: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = done() { return v; }
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn chatgpt_sign_in_and_switching_models_work_with_the_agent_on() {
    use axum::{http::HeaderMap, routing::{get, post}, Json, Router};
    use serde_json::json;
    let p = agent::provider::provider(agent::provider::CHATGPT).unwrap();
    if std::env::var_os(p.key_env).is_some() { return; }
    agent::provider::forget_key(p).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let fake = Router::new()
        .route("/api/accounts/oauth/token", post(|| async { Json(json!({"access_token":"chatgpt-access","refresh_token":"r","expires_in":3600,"scope":"openid offline_access resource.invoke chatgpt.tokens.use.direct","id_token":"x.y.z"})) }))
        .route("/v1/models", get(|h: HeaderMap| async move {
            if h["authorization"] != "Bearer chatgpt-access" { return Err(axum::http::StatusCode::UNAUTHORIZED); }
            Ok(Json(json!({"models":[
                {"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","visibility":"list","context_window":500000},
                {"slug":"gpt-6.1-terra","display_name":"GPT-6.1 Terra","visibility":"list","context_window":500000},
                {"slug":"gpt-6-luna","display_name":"GPT-6 Luna","visibility":"list","context_window":272000}]})))
        }))
        .route("/v1/responses", post(|| async { (axum::http::StatusCode::OK, "data: {\"type\":\"response.completed\"}\n\n") }));
    let (base, task) = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (format!("http://{addr}"), tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() }))
    });

    // Agent on and running, as at 22:02 on 26 Oct.
    let (_t, mut a, _mic) = rig(true);
    a.ensure_agent().unwrap();
    a.agent.as_ref().unwrap().barrier().unwrap();
    let agent_on = |a: &Pipeline| !a.settings.dry_run && a.tools.agent_enabled.load(Ordering::Acquire) && a.agent.as_ref().is_some_and(|r| r.is_running());
    assert!(agent_on(&a), "the agent runs");
    let hub = a.tools.api.clone();
    hub.fake_chatgpt(base, 0);

    // Agent ▸ Model ▸ Add Provider ▸ ChatGPT…: the browser opens the consent page and comes back with a code.
    a.action(ui::UiAction::StartProviderLogin { provider: p.id.into() }).unwrap();
    let url = wait("the consent page", || hub.pending_login_url(p.id));
    let q: BTreeMap<String, String> = reqwest::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
    let back = format!("{}?code=c&state={}&client_id=issued-1", q["redirect_uri"], q["state"]);
    assert_eq!(reqwest::blocking::get(back).unwrap().status(), 200);
    wait("ChatGPT connected", || (hub.auth(p.id) == Some(ui::ProviderAuth::Connected)).then_some(()));
    assert!(agent_on(&a), "signing in left the agent on");
    assert!(a.settings.agent_model.api().is_none_or(|(provider, _)| provider != p.id), "signing in never switches the agent");

    // The account's models reach Add Model; each is added, then the agent switches between them.
    for id in ["gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-luna"] {
        wait(&format!("{id} in the catalog"), || hub.catalog_model(p.id, id));
        a.action(ui::UiAction::SetApiModelAdded { provider: p.id.into(), id: id.into(), on: true }).unwrap();
    }
    let added: Vec<_> = a.settings.api_models.iter().filter(|m| m.provider == p.id).map(|m| m.id.as_str()).collect();
    assert_eq!(added, ["gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-luna"]);
    for id in ["gpt-6-luna", "gpt-6.1-sol", "gpt-6.1-terra", "gpt-6.1-sol"] {
        a.action(ui::UiAction::SelectApiModel { provider: p.id.into(), id: id.into() }).unwrap();
        let want = agent::provider::AgentModel::Api { provider: p.id.into(), id: id.into() };
        assert_eq!(a.settings.agent_model, want, "the switch to {id} applied");
        // The agent's supervisor starts the next Pi on it.
        let (tx, rx) = tokio::sync::oneshot::channel();
        a.agent.as_ref().unwrap().send(crate::bridge::Command::InspectSettings(tx)).unwrap();
        assert_eq!(rx.blocking_recv().unwrap().agent_model, want, "the agent runs {id} next");
        assert!(agent_on(&a), "the switch to {id} left the agent on");
    }
    agent::provider::forget_key(p).unwrap();
    task.abort();
}

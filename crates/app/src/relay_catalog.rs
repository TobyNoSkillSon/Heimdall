//! What Agent ▸ Model ▸ Claude Code subscription offers, from T3's live catalog (`orchestrator_capabilities`): the Claude models
//! the relay can run and the Claude accounts (T3 provider instances) it can run on. The menu draws them
//! (`UiUpdate::RelayModels`, `UiUpdate::RelayAccounts`) and the `settings` tool offers and checks the same lists
//! (`settings_tool::View::relay_models`, `relay_accounts`), so the tool never offers what the menu does not.
//!
//! Fetched when the supervisor connects to T3 and each time the menu opens (`UiAction::RefreshApiCatalog`).
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct RelayCatalog {
    lists: Mutex<(Vec<ui::RelayModel>, Vec<ui::RelayAccount>)>,
    /// The app's T3 client and the runtime that calls it (set by the supervisor once it runs).
    source: Mutex<Option<(Arc<t3::Client>, tokio::runtime::Handle)>>,
}

impl RelayCatalog {
    /// What the menu lists now: (models, accounts). Empty until T3 answered once.
    pub fn lists(&self) -> (Vec<ui::RelayModel>, Vec<ui::RelayAccount>) {
        self.lists.lock().unwrap().clone()
    }
    /// T3's catalog read into the menu's lists; `send` tells the menu.
    pub fn take(&self, catalog: &serde_json::Value, send: impl Fn(ui::UiUpdate)) {
        let models: Vec<ui::RelayModel> = agent::emergency::relay_models(catalog).into_iter().map(|(id, name)| ui::RelayModel { id, name }).collect();
        let accounts: Vec<ui::RelayAccount> = agent::emergency::relay_accounts(catalog).into_iter().map(|(id, name)| ui::RelayAccount { id, name }).collect();
        *self.lists.lock().unwrap() = (models.clone(), accounts.clone());
        send(ui::UiUpdate::RelayModels(models));
        send(ui::UiUpdate::RelayAccounts(accounts));
    }
    pub fn connect(&self, client: Arc<t3::Client>, runtime: tokio::runtime::Handle) {
        *self.source.lock().unwrap() = Some((client, runtime));
    }
    /// Asks T3 again in the background; nothing happens before the supervisor connected.
    pub fn refresh(self: &Arc<Self>, send: impl Fn(ui::UiUpdate) + Send + 'static) {
        let Some((client, runtime)) = self.source.lock().unwrap().clone() else { return };
        let me = self.clone();
        runtime.spawn(async move {
            match client.call("orchestrator_capabilities", serde_json::json!({})).await {
                Ok(catalog) => me.take(&catalog, send),
                Err(e) => eprintln!("relay: T3's catalog unavailable: {e:#}"),
            }
        });
    }
    #[cfg(test)]
    pub fn set(&self, models: Vec<ui::RelayModel>, accounts: Vec<ui::RelayAccount>) {
        *self.lists.lock().unwrap() = (models, accounts);
    }
}

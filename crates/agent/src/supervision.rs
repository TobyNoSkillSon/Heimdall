//! Bounded process recovery and a calm, local-only engine readiness probe.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone)]
pub struct EngineProbe {
    client: reqwest::Client,
    pub base: String,
}
impl EngineProbe {
    pub fn new(base: &str) -> Result<Self> {
        let url = reqwest::Url::parse(base)?;
        ensure!(
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port().is_some(),
            "engine must be loopback HTTP"
        );
        Ok(Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(1))
                .timeout(Duration::from_secs(3))
                .build()?,
            base: base.trim_end_matches('/').into(),
        })
    }
    pub async fn ready(&self) -> bool {
        self.client
            .get(format!("{}/ready", self.base))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }
    pub async fn context_limit(&self) -> Result<u64> {
        let r = self
            .client
            .get(format!("{}/v1/models", self.base))
            .send()
            .await?
            .error_for_status()?;
        let bytes = r.bytes().await?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "engine model metadata too large"
        );
        let v: Value = serde_json::from_slice(&bytes)?;
        let model = v["data"]
            .as_array()
            .context("Splash models missing")?
            .iter()
            .find(|m| m["id"] == crate::runtime::MODEL)
            .context("Splash model unavailable")?;
        let n = model["max_model_len"]
            .as_u64()
            .context("Splash context limit unavailable")?;
        ensure!((8192..=4_000_000).contains(&n), "Splash context invalid");
        Ok(n)
    }
}
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct RestartBudget {
    attempts: Vec<u64>,
}
impl RestartBudget {
    pub const MAX: usize = 3;
    pub const WINDOW_MS: u64 = 600_000;
    pub fn load(data: &Path) -> Result<Self> {
        let p = Self::path(data);
        if !p.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice(&std::fs::read(p)?)
            .context("agent recovery history unreadable")?)
    }
    fn path(data: &Path) -> PathBuf {
        data.join("agent/restarts.json")
    }
    /// Count durably BEFORE launching. A failed count never grants a restart.
    pub fn take(&mut self, data: &Path, now: u64) -> Result<Option<Duration>> {
        self.attempts
            .retain(|t| now.saturating_sub(*t) <= Self::WINDOW_MS);
        if self.attempts.len() >= Self::MAX {
            return Ok(None);
        }
        let mut next = self.attempts.clone();
        next.push(now);
        crate::runtime::atomic_write(
            &Self::path(data),
            &serde_json::to_vec(&Self {
                attempts: next.clone(),
            })?,
        )?;
        self.attempts = next;
        Ok(Some(Duration::from_secs(1 << (self.attempts.len() - 1))))
    }
    /// An explicit retry can reset the budget; successful events alone cannot.
    pub fn reset(data: &Path) -> Result<Self> {
        let v = Self::default();
        crate::runtime::atomic_write(&Self::path(data), &serde_json::to_vec(&v)?)?;
        Ok(v)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_budget_survives_launches_and_counts_before_restarting() {
        let d = tempfile::tempdir().unwrap();
        for n in 0..3 {
            let mut b = RestartBudget::load(d.path()).unwrap();
            assert_eq!(
                b.take(d.path(), 1000 + n).unwrap(),
                Some(Duration::from_secs(1 << n))
            );
        }
        assert_eq!(
            RestartBudget::load(d.path())
                .unwrap()
                .take(d.path(), 1004)
                .unwrap(),
            None
        );
        assert!(RestartBudget::load(d.path())
            .unwrap()
            .take(d.path(), 602000)
            .unwrap()
            .is_some());
    }
    #[test]
    fn damaged_or_unwritable_budget_forbids_restart() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("agent")).unwrap();
        std::fs::write(RestartBudget::path(d.path()), b"{").unwrap();
        assert!(RestartBudget::load(d.path()).is_err());
        std::fs::remove_file(RestartBudget::path(d.path())).unwrap();
        std::fs::create_dir(RestartBudget::path(d.path())).unwrap();
        assert!(RestartBudget::default().take(d.path(), 0).is_err());
    }
    #[tokio::test]
    async fn readiness_flaps_without_model_calls() {
        use axum::{routing::get, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app=Router::new().route("/ready",get(move ||{let c=c.clone();async move{if c.fetch_add(1,Ordering::SeqCst)%2==0{axum::http::StatusCode::SERVICE_UNAVAILABLE}else{axum::http::StatusCode::OK}}})).route("/v1/models",get(||async{axum::Json(serde_json::json!({"data":[{"id":crate::runtime::MODEL,"max_model_len":102400}]}))}));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let p = EngineProbe::new(&base).unwrap();
        assert!(!p.ready().await);
        assert!(p.ready().await);
        assert_eq!(p.context_limit().await.unwrap(), 102400);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        task.abort();
    }
    #[test]
    fn readiness_cannot_contact_cloud() {
        assert!(EngineProbe::new("https://example.com").is_err());
    }
}

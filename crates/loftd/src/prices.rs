//! Model prices for the Usage page's cost estimates: fetched from
//! `[usage] price_catalog` (models.dev) once a day, kept in
//! `cache/model-prices.json` so the app and a restarted daemon have them
//! without asking again.

use loft_config::Paths;
use loft_config::pricing::{Catalog, catalog_file};
use loft_core::Core;
use std::sync::Arc;
use std::time::Duration;

const DAY_MS: i64 = 86_400_000;
/// How often to look whether the prices are a day old.
const CHECK: Duration = Duration::from_secs(3600);
/// The catalog is ~6 MB of JSON.
const MAX_BYTES: u64 = 64 << 20;

pub(crate) fn spawn(core: Arc<Core>, paths: Paths) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let file = catalog_file(&paths);
        if let Some(catalog) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str::<Catalog>(&text).ok())
        {
            core.set_price_catalog(catalog);
        }
        loop {
            let url = core.config().usage.price_catalog;
            let fresh = core
                .price_catalog()
                .is_some_and(|c| loft_store::now_ms() - c.fetched_at_ms < DAY_MS);
            if !url.trim().is_empty() && !fresh {
                let fetched = tokio::task::spawn_blocking(move || fetch(&url)).await;
                match fetched {
                    Ok(Ok(catalog)) => {
                        tracing::info!(models = catalog.models.len(), "model prices updated");
                        if let Some(dir) = file.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        match serde_json::to_vec(&catalog) {
                            Ok(bytes) => {
                                if let Err(err) = std::fs::write(&file, bytes) {
                                    tracing::warn!(%err, "couldn't save model prices");
                                }
                            }
                            Err(err) => tracing::warn!(%err, "couldn't save model prices"),
                        }
                        core.set_price_catalog(catalog);
                    }
                    Ok(Err(err)) => tracing::warn!(%err, "couldn't fetch model prices"),
                    Err(err) => tracing::warn!(%err, "model price fetch stopped"),
                }
            }
            tokio::time::sleep(CHECK).await;
        }
    })
}

fn fetch(url: &str) -> anyhow::Result<Catalog> {
    use ureq::tls::{TlsConfig, TlsProvider};
    // Cargo.toml picks the TLS stack per platform; ureq has to be told which.
    let provider = if cfg!(any(target_os = "macos", windows)) {
        TlsProvider::NativeTls
    } else {
        TlsProvider::Rustls
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(provider).build())
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent("agentloft")
        .build()
        .into();
    let text = agent
        .get(url)
        .call()?
        .body_mut()
        .with_config()
        .limit(MAX_BYTES)
        .read_to_string()?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let catalog = Catalog::from_models_dev(&json, loft_store::now_ms());
    anyhow::ensure!(!catalog.models.is_empty(), "no model prices in {url}");
    Ok(catalog)
}

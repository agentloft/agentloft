//! Small HTTPS reads (the agents' latest releases).

use std::time::Duration;

/// The body of `url`, up to `limit` bytes, given five seconds (an agent's
/// status waits on it the first time).
pub(crate) fn get_text(url: &str, limit: u64) -> anyhow::Result<String> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // Cargo.toml picks the TLS stack per platform; ureq has to be told which
    // (and, with the OS's stack, to trust the OS's certificates).
    let (provider, roots) = if cfg!(any(target_os = "macos", windows)) {
        (TlsProvider::NativeTls, RootCerts::PlatformVerifier)
    } else {
        (TlsProvider::Rustls, RootCerts::WebPki)
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .provider(provider)
                .root_certs(roots)
                .build(),
        )
        .timeout_global(Some(Duration::from_secs(5)))
        .user_agent("agentloft")
        .build()
        .into();
    Ok(agent
        .get(url)
        .call()?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_string()?)
}

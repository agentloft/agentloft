//! Address-bar input → URL, and the scheme policy shared by the host and the
//! native navigation delegate.
//!
//! Rules for [`normalize_address`]:
//! - `http:`, `https:`, `about:` and `file:` URLs are taken as typed.
//! - Any other explicit scheme (`javascript:`, `data:`, `mailto:`, …) is
//!   rejected with [`BrowserError::Blocked`].
//! - Something that looks like a host (`example.com`, `github.com/a/b`,
//!   `localhost:3000`, `10.0.0.2`, `[::1]:8080`) gets a scheme: `https://`,
//!   or `http://` for loopback hosts, where dev servers rarely speak TLS.
//! - An absolute filesystem path (`/tmp/page.html`) becomes a `file://` URL.
//! - Everything else is a search query.

use crate::BrowserError;
use url::Url;

/// Default search engine; `{}` is replaced by the URL-encoded query.
pub const DEFAULT_SEARCH_TEMPLATE: &str = "https://duckduckgo.com/?q={}";

/// Schemes a page may navigate to on its own.
const PAGE_SCHEMES: &[&str] = &["http", "https", "about"];

/// What the scheme policy says about a navigation target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemePolicy {
    /// `http`, `https`, `about`: always allowed.
    Allowed,
    /// `file`: allowed only when the host initiated the navigation.
    HostOnly,
    /// Anything else.
    Denied,
}

/// Classifies a URL string by scheme.
pub fn scheme_policy(url: &str) -> SchemePolicy {
    let scheme = url
        .split_once(':')
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default();
    if PAGE_SCHEMES.contains(&scheme.as_str()) {
        SchemePolicy::Allowed
    } else if scheme == "file" {
        SchemePolicy::HostOnly
    } else {
        SchemePolicy::Denied
    }
}

/// Turns address-bar input (or an agent's `navigate` argument) into a URL
/// the browser may load. See the module docs for the rules.
pub fn normalize_address(input: &str, search_template: &str) -> Result<Url, BrowserError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(BrowserError::InvalidUrl("empty address".into()));
    }

    if let Some(scheme) = explicit_scheme(input) {
        let url =
            Url::parse(input).map_err(|e| BrowserError::InvalidUrl(format!("{input}: {e}")))?;
        return match scheme_policy(url.as_str()) {
            SchemePolicy::Denied => Err(BrowserError::Blocked(format!(
                "the `{scheme}:` scheme is not allowed (only http, https, about and file)"
            ))),
            _ => Ok(url),
        };
    }

    if input.starts_with('/') && !input.contains(char::is_whitespace) {
        // `Url::from_file_path` doesn't exist on wasm32 (no file paths
        // there); the embedded browser doesn't either.
        #[cfg(not(target_arch = "wasm32"))]
        return Url::from_file_path(input)
            .map_err(|()| BrowserError::InvalidUrl(format!("not an absolute path: {input}")));
        #[cfg(target_arch = "wasm32")]
        return Url::parse(&format!("file://{input}"))
            .map_err(|e| BrowserError::InvalidUrl(format!("{input}: {e}")));
    }

    if looks_like_host(input) {
        let scheme = if is_loopback(host_part(input)) {
            "http"
        } else {
            "https"
        };
        if let Ok(url) = Url::parse(&format!("{scheme}://{input}"))
            && url.host().is_some()
        {
            return Ok(url);
        }
    }

    Ok(search_url(input, search_template))
}

/// Builds a search URL for `query` from a template containing `{}`.
pub fn search_url(query: &str, template: &str) -> Url {
    let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let template = if template.contains("{}") {
        template
    } else {
        DEFAULT_SEARCH_TEMPLATE
    };
    Url::parse(&template.replacen("{}", &encoded, 1)).unwrap_or_else(|_| {
        Url::parse(&DEFAULT_SEARCH_TEMPLATE.replacen("{}", &encoded, 1))
            .expect("default search template is a valid URL")
    })
}

/// Returns the scheme if `input` starts with one, as opposed to `host:port`.
fn explicit_scheme(input: &str) -> Option<String> {
    let (scheme, rest) = input.split_once(':')?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !valid {
        return None;
    }
    // `example.com:8080` and `localhost:3000` are hosts with ports.
    let port_like = rest
        .split(['/', '?', '#'])
        .next()
        .is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if scheme.contains('.') || (port_like && !rest.starts_with("//")) {
        return None;
    }
    if scheme.eq_ignore_ascii_case("localhost") {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

/// The `host[:port]` prefix of a scheme-less address.
fn host_part(input: &str) -> &str {
    let end = input.find(['/', '?', '#']).unwrap_or(input.len());
    let authority = &input[..end];
    if let Some(rest) = authority.strip_prefix('[') {
        // [v6]:port
        return rest.split(']').next().unwrap_or(rest);
    }
    authority.rsplit_once(':').map_or(authority, |(h, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            authority
        }
    })
}

fn looks_like_host(input: &str) -> bool {
    if input.contains(char::is_whitespace) {
        return false;
    }
    let host = host_part(input);
    if host.is_empty() {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    // A dotted name whose last label is a plausible TLD.
    let mut labels = host.split('.');
    let Some(tld) = labels.next_back() else {
        return false;
    };
    labels.next_back().is_some_and(|l| !l.is_empty())
        && host
            .split('.')
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(input: &str) -> String {
        normalize_address(input, DEFAULT_SEARCH_TEMPLATE)
            .unwrap()
            .to_string()
    }

    #[test]
    fn full_urls_pass_through() {
        assert_eq!(norm("https://example.com"), "https://example.com/");
        assert_eq!(
            norm("http://news.ycombinator.com/item?id=1"),
            "http://news.ycombinator.com/item?id=1"
        );
        assert_eq!(norm("about:blank"), "about:blank");
        assert_eq!(norm("file:///tmp/a.html"), "file:///tmp/a.html");
        assert_eq!(norm("  HTTPS://Example.COM/x  "), "https://example.com/x");
    }

    #[test]
    fn missing_scheme_becomes_https() {
        assert_eq!(norm("example.com"), "https://example.com/");
        assert_eq!(
            norm("github.com/agentloft/agentloft"),
            "https://github.com/agentloft/agentloft"
        );
        assert_eq!(
            norm("sub.example.co.uk:8443/a?b=c"),
            "https://sub.example.co.uk:8443/a?b=c"
        );
        assert_eq!(norm("8.8.8.8"), "https://8.8.8.8/");
    }

    #[test]
    fn loopback_hosts_get_http() {
        assert_eq!(norm("localhost"), "http://localhost/");
        assert_eq!(norm("localhost:3000/app"), "http://localhost:3000/app");
        assert_eq!(norm("127.0.0.1:8080"), "http://127.0.0.1:8080/");
        assert_eq!(norm("[::1]:5173"), "http://[::1]:5173/");
        assert_eq!(norm("app.localhost:3000"), "http://app.localhost:3000/");
    }

    #[test]
    fn bare_words_become_a_search() {
        assert_eq!(norm("rust"), "https://duckduckgo.com/?q=rust");
        assert_eq!(
            norm("rust gpui webview"),
            "https://duckduckgo.com/?q=rust+gpui+webview"
        );
        assert_eq!(
            norm("what's 1+1?"),
            "https://duckduckgo.com/?q=what%27s+1%2B1%3F"
        );
        // Spaces win over dots: this is a sentence, not a host.
        assert_eq!(
            norm("see example.com"),
            "https://duckduckgo.com/?q=see+example.com"
        );
        // Numeric "TLDs" are not hosts.
        assert_eq!(norm("3.14"), "https://duckduckgo.com/?q=3.14");
    }

    #[test]
    fn custom_search_template() {
        let url = normalize_address("gpui", "https://www.google.com/search?q={}").unwrap();
        assert_eq!(url.as_str(), "https://www.google.com/search?q=gpui");
        // A template without `{}` falls back to the default.
        let url = normalize_address("gpui", "https://bad.example").unwrap();
        assert_eq!(url.as_str(), "https://duckduckgo.com/?q=gpui");
    }

    #[test]
    fn absolute_paths_become_file_urls() {
        assert_eq!(norm("/tmp/page.html"), "file:///tmp/page.html");
    }

    #[test]
    fn disallowed_schemes_are_blocked() {
        for input in [
            "javascript:alert(1)",
            "data:text/html,<b>x</b>",
            "mailto:someone@example.com",
            "ftp://example.com/file",
            "blob:https://example.com/123",
            "chrome://settings",
        ] {
            let err = normalize_address(input, DEFAULT_SEARCH_TEMPLATE).unwrap_err();
            assert!(matches!(err, BrowserError::Blocked(_)), "{input}: {err}");
        }
        assert!(matches!(
            normalize_address("   ", DEFAULT_SEARCH_TEMPLATE),
            Err(BrowserError::InvalidUrl(_))
        ));
    }

    #[test]
    fn scheme_policy_classification() {
        assert_eq!(scheme_policy("https://a.b/"), SchemePolicy::Allowed);
        assert_eq!(scheme_policy("HTTP://a.b/"), SchemePolicy::Allowed);
        assert_eq!(scheme_policy("about:srcdoc"), SchemePolicy::Allowed);
        assert_eq!(scheme_policy("file:///etc/passwd"), SchemePolicy::HostOnly);
        assert_eq!(scheme_policy("data:text/html,hi"), SchemePolicy::Denied);
        assert_eq!(scheme_policy("javascript:void(0)"), SchemePolicy::Denied);
        assert_eq!(scheme_policy("blob:https://a.b/x"), SchemePolicy::Denied);
        assert_eq!(scheme_policy("no-scheme"), SchemePolicy::Denied);
    }
}

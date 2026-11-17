//! A tab's allowed origins (ADR-0017 §9): the addresses an agent may send a
//! tab it opened with `allowed_origins`, and the popups that tab opens.
//!
//! An entry is a host (`example.com`, `localhost:3000`, `[::1]`), the same
//! with a scheme (`https://example.com`), or either with a leading `*.` for
//! the host and every name below it. Matching is by spelling, with two
//! exceptions for addresses on the machine's own networks: a private host is
//! only reached through an entry that names it (never through a wildcard),
//! and a name only a wildcard matched must not resolve to a private address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use url::{Host, Url};

/// At most this many entries per tab.
const MAX_ENTRIES: usize = 100;
/// How long a navigation waits for a name to resolve before going ahead.
const LOOKUP_LIMIT: Duration = Duration::from_secs(2);
/// How much of an unreadable address a refusal repeats.
const SHOWN_CHARS: usize = 200;

/// One parsed entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OriginRule {
    /// The entry as given (trimmed, lower-case), for messages.
    text: String,
    /// `http` or `https`; `None`: either.
    scheme: Option<String>,
    /// Lower-case, IPv6 without brackets.
    host: String,
    /// `*.host`: the host and every name ending in `.host`.
    wildcard: bool,
    /// The port given, else the scheme's; `None`: any.
    port: Option<u16>,
}

impl OriginRule {
    fn matches(&self, scheme: &str, host: &str, port: Option<u16>) -> bool {
        if self.scheme.as_deref().is_some_and(|s| s != scheme) {
            return false;
        }
        if self.port.is_some() && self.port != port {
            return false;
        }
        if self.wildcard {
            host == self.host
                || host
                    .strip_suffix(self.host.as_str())
                    .is_some_and(|rest| rest.ends_with('.'))
        } else {
            host == self.host
        }
    }
}

/// Parse each entry as one pattern.
pub(super) fn parse(raw: &[String]) -> Result<Vec<OriginRule>, String> {
    if raw.len() > MAX_ENTRIES {
        return Err(format!(
            "allowed_origins has {} entries; at most {MAX_ENTRIES} are allowed.",
            raw.len()
        ));
    }
    raw.iter().map(|entry| parse_one(entry)).collect()
}

fn parse_one(raw: &str) -> Result<OriginRule, String> {
    let entry = raw.trim();
    let fail = |why: &str| Err(format!("allowed origin {entry:?}: {why}"));
    if entry.is_empty() {
        return Err("allowed_origins has an empty entry.".to_owned());
    }
    let (scheme, rest) = match entry.split_once("://") {
        Some((scheme, rest))
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) =>
        {
            (Some(scheme.to_ascii_lowercase()), rest)
        }
        _ => (None, entry),
    };
    if let Some(scheme) = &scheme
        && scheme != "http"
        && scheme != "https"
    {
        return fail("only http and https addresses can be allowed.");
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest == "*" || rest == "*.*" {
        return fail("this would allow every site; leave allowed_origins out instead.");
    }
    if rest.contains('@') {
        return fail("give just a host or an origin, without a user name.");
    }
    if rest.contains(['/', '?', '#']) {
        return fail("give just a host or an origin, without a path, query or fragment.");
    }
    let (wildcard, rest) = match rest.strip_prefix("*.") {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    if rest.contains('*') {
        return fail("a wildcard may only be a leading `*.`, as in `*.example.com`.");
    }
    let (host, port) = split_port(rest).ok_or_else(|| {
        format!("allowed origin {entry:?}: not a host, or a host and port, that can be read.")
    })?;
    let host = match Host::parse(host) {
        Ok(host) if !host_text(&host).is_empty() => host,
        _ => return fail("not a host that can be read."),
    };
    if wildcard {
        match &host {
            Host::Ipv4(_) | Host::Ipv6(_) => {
                return fail("a wildcard can't stand before an IP address.");
            }
            Host::Domain(name) if !name.contains('.') => {
                return fail("a wildcard needs a dotted domain after it, as in `*.example.com`.");
            }
            Host::Domain(_) => {}
        }
    }
    let port = port.or(match scheme.as_deref() {
        Some("https") => Some(443),
        Some("http") => Some(80),
        _ => None,
    });
    Ok(OriginRule {
        text: entry.to_lowercase(),
        scheme,
        host: host_text(&host),
        wildcard,
        port,
    })
}

/// `host` and its port, if it has one. `None`: there is no reading it.
fn split_port(rest: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if rest.starts_with('[') {
        let end = rest.find(']')?;
        let (host, after) = rest.split_at(end + 1);
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':')?)),
        }
    } else {
        match rest.split_once(':') {
            // An IPv6 address must be bracketed.
            Some((_, port)) if port.contains(':') => return None,
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) => Some(port.parse::<u16>().ok().filter(|p| *p != 0)?),
        None => None,
    };
    Some((host, port))
}

/// A host as rules and addresses compare it: lower-case, without brackets
/// or a trailing dot.
fn host_text(host: &Host<impl AsRef<str>>) -> String {
    match host {
        Host::Domain(name) => name.as_ref().trim_end_matches('.').to_ascii_lowercase(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
    }
}

/// The list as a tab keeps it: entries split on commas, checked, lower-cased
/// and without repeats, in the order given.
pub(super) fn normalize(raw: &[String]) -> Result<Vec<String>, String> {
    let pieces: Vec<String> = raw
        .iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    parse(&pieces)?;
    let mut kept: Vec<String> = Vec::new();
    for piece in pieces {
        let piece = piece.to_lowercase();
        if !kept.contains(&piece) {
            kept.push(piece);
        }
    }
    Ok(kept)
}

/// Whether an address is one a tab with these rules may show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Decision {
    /// `host`: the address's (none for `about:` documents). `explicit`: an
    /// entry without a wildcard matched it.
    Allowed {
        host: Option<String>,
        explicit: bool,
    },
    /// Why not, as a clause for the agent.
    Refused(String),
}

#[cfg(test)]
impl Decision {
    fn allowed(&self) -> bool {
        matches!(self, Decision::Allowed { .. })
    }
}

/// Check an address against the rules, by its spelling alone.
pub(super) fn check(raw: &str, rules: &[OriginRule]) -> Decision {
    let Ok(url) = Url::parse(raw.trim()) else {
        let shown: String = raw.trim().chars().take(SHOWN_CHARS).collect();
        return Decision::Refused(format!("{shown:?} is not a web address"));
    };
    let scheme = url.scheme();
    if scheme == "about" {
        return Decision::Allowed {
            host: None,
            explicit: false,
        };
    }
    if scheme != "http" && scheme != "https" {
        return Decision::Refused(format!(
            "{scheme}: addresses are never allowed, only http and https ones"
        ));
    }
    let Some(host) = url.host().map(|h| host_text(&h)) else {
        return Decision::Refused(format!("{} has no host", url.as_str()));
    };
    let port = url.port_or_known_default();
    let matching: Vec<&OriginRule> = rules
        .iter()
        .filter(|rule| rule.matches(scheme, &host, port))
        .collect();
    if matching.is_empty() {
        let origin = url.origin().ascii_serialization();
        return Decision::Refused(if rules.is_empty() {
            format!("{origin} is not allowed: the tab has no allowed origins")
        } else {
            let list: Vec<&str> = rules.iter().map(|r| r.text.as_str()).collect();
            format!(
                "{origin} is not among the tab's allowed origins ({})",
                list.join(", ")
            )
        });
    }
    let explicit = matching.iter().any(|rule| !rule.wildcard);
    if !explicit && is_private(&host) {
        return Decision::Refused(format!(
            "{host} is a private address, which a wildcard never allows: only an entry \
             naming `{host}` exactly does"
        ));
    }
    Decision::Allowed {
        host: Some(host),
        explicit,
    }
}

/// [`check`], and for a name that only a wildcard allowed, where it
/// resolves: a public-looking name for a private address is refused. A
/// lookup that fails or takes too long refuses nothing (the load fails on
/// its own if the name doesn't resolve).
pub(super) async fn check_navigation(raw: &str, rules: &[OriginRule]) -> Decision {
    let decision = check(raw, rules);
    let Decision::Allowed {
        host: Some(host),
        explicit: false,
    } = &decision
    else {
        return decision;
    };
    // An address, or a name for this machine, was judged by its spelling.
    if is_literal_or_local(host) {
        return decision;
    }
    let lookup = tokio::net::lookup_host((host.as_str(), 443));
    let Ok(Ok(addresses)) = tokio::time::timeout(LOOKUP_LIMIT, lookup).await else {
        return decision;
    };
    match first_private(addresses.map(|a| a.ip())) {
        Some(ip) => Decision::Refused(format!(
            "{host} resolves to the private address {ip}, which a wildcard never allows: \
             only an entry naming `{host}` exactly does"
        )),
        None => decision,
    }
}

fn first_private(addresses: impl IntoIterator<Item = IpAddr>) -> Option<IpAddr> {
    addresses.into_iter().find(|ip| private_ip(*ip))
}

fn is_literal_or_local(host: &str) -> bool {
    local_name(host) || parse_ip(host).is_some()
}

fn local_name(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host == "localhost.localdomain"
}

/// An IP address as written in a host: brackets and a zone (`%en0`) are
/// not part of it.
fn parse_ip(host: &str) -> Option<IpAddr> {
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let host = host.split_once('%').map_or(host, |(ip, _zone)| ip);
    host.parse().ok()
}

/// A host on this machine or its networks: `localhost` and its kin, and
/// addresses in private and special-use ranges.
fn is_private(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    local_name(&host) || parse_ip(&host).is_some_and(private_ip)
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => private_v4(ip),
        IpAddr::V6(ip) => private_v6(ip),
    }
}

fn private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0
        || a == 10
        || (a == 100 && (64..128).contains(&b))
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        // Multicast (224/4) and reserved (240/4).
        || a >= 224
}

fn private_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    if ip.is_unspecified() || ip.is_loopback() {
        return true;
    }
    // IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible (`::a.b.c.d`).
    if s[..5] == [0; 5] && (s[5] == 0xffff || s[5] == 0) {
        let [.., hi, lo] = s;
        let v4 = Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
        return private_v4(v4);
    }
    (s[0] & 0xfe00) == 0xfc00 // unique local
        || (s[0] & 0xffc0) == 0xfe80 // link-local
        || (s[0] & 0xff00) == 0xff00 // multicast
        || (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4]) // NAT64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(entries: &[&str]) -> Vec<OriginRule> {
        parse(&entries.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    fn one(entry: &str) -> Result<OriginRule, String> {
        parse(&[entry.to_owned()]).map(|mut r| r.remove(0))
    }

    #[test]
    fn entries_are_read() {
        let rule = one(" https://*.Example.com/ ").unwrap();
        assert_eq!(rule.host, "example.com");
        assert!(rule.wildcard);
        assert_eq!(rule.port, Some(443));
        assert_eq!(rule.scheme.as_deref(), Some("https"));
        let bare = one("localhost:3000").unwrap();
        assert_eq!((bare.scheme, bare.port), (None, Some(3000)));
        assert_eq!(one("[::1]").unwrap().host, "::1");
        assert_eq!(one("HTTP://127.0.0.1").unwrap().port, Some(80));
    }

    #[test]
    fn bad_entries_are_refused_by_name() {
        for bad in [
            "",
            "*",
            "*.*",
            "https://*",
            "ftp://x.com",
            "example.com/path",
            "example.com?q=1",
            "example.com#top",
            "user@example.com",
            "https://user:pw@example.com",
            "ex*ample.com",
            "*example.com",
            "a.*.example.com",
            "*.10.0.0.1",
            "*.[::1]",
            "*.localhost",
            "*.com",
            "example.com:",
            "example.com:99999",
            "::1",
            "exa mple.com",
        ] {
            let err = one(bad).unwrap_err();
            if !bad.trim().is_empty() {
                assert!(err.contains(bad), "{bad}: {err}");
            }
        }
        assert!(one("*").unwrap_err().contains("leave allowed_origins out"));
        let many: Vec<String> = (0..101).map(|i| format!("site{i}.com")).collect();
        assert!(parse(&many).is_err());
        assert!(parse(&many[..100]).is_ok());
        assert!(normalize(&[many.join(",")]).is_err());
    }

    #[test]
    fn lists_are_tidied() {
        assert_eq!(
            normalize(&["A.com, b.com".into(), "a.com".into()]).unwrap(),
            vec!["a.com", "b.com"]
        );
        assert_eq!(normalize(&[" , ".into()]).unwrap(), Vec::<String>::new());
        assert!(normalize(&["a.com, ftp://b.com".into()]).is_err());
    }

    #[test]
    fn addresses_are_checked() {
        let r = rules(&["example.com", "*.github.com", "http://localhost:3000"]);
        for fine in [
            "https://example.com/x",
            "http://example.com:8080/",
            "https://api.github.com",
            "https://github.com/",
            "http://localhost:3000/",
            "about:blank",
        ] {
            assert!(check(fine, &r).allowed(), "{fine}");
        }
        for refused in [
            "https://evil.com",
            "https://notgithub.com",
            "https://example.com.evil.com",
            "http://localhost:3001/",
            "https://localhost:3000/",
            "file:///etc/passwd",
            "chrome://settings",
            "data:text/html,hi",
            "not a url",
        ] {
            assert!(!check(refused, &r).allowed(), "{refused}");
        }
        let Decision::Refused(why) = check("https://evil.com", &r) else {
            panic!()
        };
        assert!(why.contains("allowed origins") && why.contains("*.github.com"));
        assert_eq!(
            check("about:blank", &r),
            Decision::Allowed {
                host: None,
                explicit: false
            }
        );
        assert_eq!(
            check("https://API.github.com/", &r),
            Decision::Allowed {
                host: Some("api.github.com".into()),
                explicit: false
            }
        );
        assert!(check("https://a.internal.com", &rules(&["*.internal.com"])).allowed());
        let long = format!("x{}", "y".repeat(500));
        let Decision::Refused(why) = check(&long, &r) else {
            panic!()
        };
        assert!(why.len() < 260, "{why}");
    }

    #[test]
    fn private_hosts_need_an_exact_entry() {
        let local = rules(&["localhost"]);
        assert!(check("http://localhost:8080/", &local).allowed());
        assert!(!check("http://127.0.0.1:8080/", &local).allowed());
        let exact = rules(&["http://127.0.0.1:8080"]);
        assert_eq!(
            check("http://127.0.0.1:8080/x", &exact),
            Decision::Allowed {
                host: Some("127.0.0.1".into()),
                explicit: true
            }
        );
        assert!(check("http://[::1]/", &rules(&["[::1]"])).allowed());
        // A wildcard never reaches this machine, even when it spells it.
        let wild = rules(&["*.app.localhost"]);
        let Decision::Refused(why) = check("http://x.app.localhost/", &wild) else {
            panic!()
        };
        assert!(why.contains("`x.app.localhost`"), "{why}");
        assert!(check("http://x.app.localhost/", &rules(&["x.app.localhost"])).allowed());
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "[::1]",
            "::ffff:192.168.1.1",
            "fd00::1",
            "fe80::1%en0",
            "localhost",
            "app.localhost",
            "100.64.0.1",
            "172.31.255.255",
            "169.254.1.1",
            "224.0.0.1",
            "::",
            "64:ff9b::1.2.3.4",
        ] {
            assert!(is_private(private), "{private}");
        }
        for public in ["8.8.8.8", "2606:4700::1111", "example.com", "172.32.0.1"] {
            assert!(!is_private(public), "{public}");
        }
    }

    #[tokio::test]
    async fn names_a_wildcard_allowed_must_not_be_private() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(
            first_private([ip("93.184.215.14"), ip("10.0.0.7")]),
            Some(ip("10.0.0.7"))
        );
        assert_eq!(first_private([ip("93.184.215.14")]), None);
        // Explicit entries and addresses are judged without a lookup.
        let r = rules(&["example.com", "*.example.org"]);
        assert!(check_navigation("https://example.com/", &r).await.allowed());
        assert!(!check_navigation("https://evil.com/", &r).await.allowed());
        assert!(
            check_navigation("http://8.8.8.8/", &rules(&["8.8.8.8"]))
                .await
                .allowed()
        );
    }
}

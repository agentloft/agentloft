//! Bot checks and block pages: after a page loads for an agent, whether it
//! is a bot check (Cloudflare, a captcha …) or a block page rather than the
//! site, with advice to hand it to the user (`browser_handoff`) or stop.
//! An agent is never told to solve one itself.
//!
//! The page is read by `loftProbe` (`agent.js`, top document only): its
//! address, title, visible text, the addresses of its frames and scripts,
//! and whether the elements the vendors' checks are known by are there.
//! What it is is decided here ([`classify`]), so it can be tested without
//! a browser. A check the browser clears by itself is waited for.

use super::automation::Agent;
use loft_page_script::Helper;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// How long one look at the page may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a check that clears by itself is waited for, in all.
const CLEAR_WAIT: Duration = Duration::from_secs(10);
/// How often it is looked at meanwhile.
const CLEAR_POLL: Duration = Duration::from_secs(1);
/// Pages with less body text than this are "short": a wall, not an article.
const SHORT_PAGE: usize = 1500;
const MAX_SIGNAL: usize = 160;

/// Elements the checks are known by (CSS selectors the probe looks for).
const ELEMENTS: &[&str] = &[
    "#cf-error-details",
    "#challenge-form",
    "#challenge-running",
    "#cf-challenge-running",
    "#challenge-stage",
    ".cf-turnstile",
    "#px-captcha",
    "#px-captcha-wrapper",
    "#ddv1-captcha-container",
    "#sec-if-cpt-container",
    "#sec-cpt-if",
    "#sec-text-if",
    "iframe#main-iframe",
    "awswaf-captcha",
    "#challenge-container",
    ".g-recaptcha",
    ".h-captcha",
];
/// Id prefixes the probe looks for.
const ID_PREFIXES: &[&str] = &["px-captcha"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An interstitial the browser clears by itself.
    NonInteractive,
    /// A check only a person should answer.
    Interactive,
    /// The site refused the visit.
    Block,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::NonInteractive => "non-interactive",
            Kind::Interactive => "interactive",
            Kind::Block => "block",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    pub vendor: &'static str,
    pub kind: Kind,
    /// What gave it away.
    pub signal: String,
}

fn found(vendor: &'static str, kind: Kind, signal: impl Into<String>) -> Option<Detection> {
    let signal: String = signal.into();
    let signal = if signal.chars().count() > MAX_SIGNAL {
        let mut cut: String = signal.chars().take(MAX_SIGNAL - 1).collect();
        cut.push('…');
        cut
    } else {
        signal
    };
    Some(Detection {
        vendor,
        kind,
        signal,
    })
}

/// An element the probe looked for.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Element {
    pub found: bool,
    pub visible: bool,
    pub src: String,
}

/// What `loftProbe` read.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Probe {
    pub url: String,
    pub title: String,
    /// The body's visible text, its start.
    pub text: String,
    /// Its whole length.
    pub text_length: usize,
    pub iframes: Vec<String>,
    pub scripts: Vec<String>,
    /// By selector (and by `id^=prefix` for [`ID_PREFIXES`]).
    pub elements: HashMap<String, Element>,
}

impl Probe {
    fn element(&self, selector: &str) -> Element {
        self.elements.get(selector).cloned().unwrap_or_default()
    }

    fn has(&self, selector: &str) -> bool {
        self.element(selector).found
    }

    /// The first of `selectors` that is there.
    fn any<'s>(&self, selectors: &[&'s str]) -> Option<&'s str> {
        selectors.iter().copied().find(|s| self.has(s))
    }
}

/// Whole-word phrases a generic bot check says.
static CHECK_PHRASES: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"\b(?:verify (?:that )?you are (?:a )?human|confirm you are (?:a )?human|pardon our interruption|please verify you are a human|bot check|bot detection)\b|\bare you a robot\?",
    )
    .expect("a valid pattern")
});

/// What the page is, if it is a bot check or a block page. The first rule
/// that matches wins.
pub fn classify(probe: &Probe) -> Option<Detection> {
    use Kind::*;
    let title = probe.title.trim().to_lowercase();
    let text = probe.text.to_lowercase();
    let short = probe.text_length < SHORT_PAGE;
    let says = |phrase: &str| text.contains(phrase);
    let iframe = |test: &dyn Fn(&str) -> bool| probe.iframes.iter().find(|src| test(src)).cloned();
    let script = |test: &dyn Fn(&str) -> bool| probe.scripts.iter().find(|src| test(src)).cloned();
    let title_signal = || format!("title: {}", probe.title.trim());
    let text_signal = |phrase: &str| format!("text: {phrase}");

    // 1. Cloudflare's block page.
    if says("sorry, you have been blocked")
        && (probe.has("#cf-error-details") || says("cloudflare"))
    {
        return found(
            "cloudflare",
            Block,
            text_signal("Sorry, you have been blocked"),
        );
    }

    // 2. Cloudflare's interstitial.
    const CF_TITLES: [&str; 5] = [
        "just a moment",
        "attention required! | cloudflare",
        "checking your browser",
        "un instant",
        "einen moment",
    ];
    let cf_signal = if CF_TITLES.iter().any(|t| title.starts_with(t)) {
        Some(title_signal())
    } else if let Some(selector) = probe.any(&[
        "#challenge-form",
        "#challenge-running",
        "#cf-challenge-running",
        "#challenge-stage",
    ]) {
        Some(format!("element {selector}"))
    } else if short {
        iframe(&|src| {
            src.starts_with("https://challenges.cloudflare.com/")
                && (src.contains("turnstile") || src.contains("challenge-platform"))
        })
        .map(|src| format!("iframe {src}"))
    } else {
        None
    };
    if let Some(signal) = cf_signal {
        let interactive = probe.element(".cf-turnstile").visible
            || says("verify you are human by completing")
            || says("click to verify")
            || says("confirm you are human");
        return found(
            "cloudflare",
            if interactive {
                Interactive
            } else {
                NonInteractive
            },
            signal,
        );
    }

    // 3. PerimeterX / HUMAN.
    if let Some(selector) = probe.any(&["#px-captcha", "#px-captcha-wrapper", "[id^=px-captcha]"]) {
        let shown = selector.replace("[id^=px-captcha]", "#px-captcha…");
        return found("perimeterx", Interactive, format!("element {shown}"));
    }
    for phrase in ["press & hold", "press and hold"] {
        if says(phrase) {
            return found("perimeterx", Interactive, text_signal(phrase));
        }
    }

    // 4. DataDome.
    if let Some(src) = iframe(&|src| src.contains("captcha-delivery.com")) {
        let kind = if src.contains("/interstitial/") {
            NonInteractive
        } else {
            Interactive
        };
        return found("datadome", kind, format!("iframe {src}"));
    }
    if probe.has("#ddv1-captcha-container") {
        return found("datadome", Interactive, "element #ddv1-captcha-container");
    }

    // 5. Akamai Bot Manager.
    if let Some(selector) = probe.any(&["#sec-if-cpt-container", "#sec-cpt-if", "#sec-text-if"]) {
        let interactive = probe.element("#sec-cpt-if").visible
            && ["verify", "press", "slide", "captcha"]
                .iter()
                .any(|w| says(w));
        let kind = if interactive {
            Interactive
        } else {
            NonInteractive
        };
        return found("akamai", kind, format!("element {selector}"));
    }
    if title == "access denied" && says("you don't have permission to access") && says("reference")
    {
        return found("akamai", Block, title_signal());
    }

    // 6. Imperva / Incapsula.
    let incapsula = |src: &str| src.to_lowercase().contains("_incapsula_resource");
    let main_iframe = probe.element("iframe#main-iframe");
    let imperva = iframe(&|src| incapsula(src))
        .map(|src| format!("iframe {src}"))
        .or_else(|| script(&|src| incapsula(src)).map(|src| format!("script {src}")))
        .or_else(|| {
            (main_iframe.found && incapsula(&main_iframe.src))
                .then(|| format!("iframe {}", main_iframe.src))
        });
    if let Some(signal) = imperva {
        let captcha = ["captcha", "verify you are human", "i am human"]
            .iter()
            .any(|w| says(w))
            || iframe(&|src| {
                let src = src.to_lowercase();
                src.contains("hcaptcha") || src.contains("recaptcha")
            })
            .is_some();
        let kind = if captcha {
            Interactive
        } else if says("request unsuccessful") || says("incapsula incident id") {
            Block
        } else {
            NonInteractive
        };
        return found("imperva", kind, signal);
    }
    if short && says("incapsula incident id") {
        return found("imperva", Block, text_signal("Incapsula incident ID"));
    }

    // 7. AWS WAF.
    if probe.has("awswaf-captcha") {
        return found("aws-waf", Interactive, "element awswaf-captcha");
    }
    if short
        && let Some(src) = script(&|src| src.contains(".awswaf.com/"))
        && (probe.has("#challenge-container") || probe.text_length < 300)
    {
        return found("aws-waf", NonInteractive, format!("script {src}"));
    }

    if !short {
        return None;
    }

    // 8. Captcha walls.
    let wall = [
        "verify you are human",
        "verify you are a human",
        "are you a robot",
        "not a robot",
        "unusual traffic",
        "security check",
        "complete the captcha",
        "captcha",
    ]
    .iter()
    .any(|w| says(w))
        || ["captcha", "security check", "verify"]
            .iter()
            .any(|w| title.contains(w));
    if wall {
        if let Some(src) = iframe(&|src| {
            [
                "https://hcaptcha.com/captcha/",
                "https://newassets.hcaptcha.com/captcha/",
                "https://assets.hcaptcha.com/captcha/",
            ]
            .iter()
            .any(|p| src.starts_with(p))
        }) {
            return found("hcaptcha", Interactive, format!("iframe {src}"));
        }
        if let Some(src) = iframe(&|src| {
            (src.starts_with("https://www.google.com/recaptcha/")
                || src.starts_with("https://www.recaptcha.net/recaptcha/"))
                && src.contains("/anchor")
        }) {
            return found("recaptcha", Interactive, format!("iframe {src}"));
        }
    }
    let google_sorry = url::Url::parse(&probe.url).is_ok_and(|url| {
        url.host_str().is_some_and(|h| h.starts_with("www.google."))
            && probe.url.contains("/sorry/")
    });
    if google_sorry {
        return found("recaptcha", Interactive, "url: google.com/sorry");
    }

    // 9. Anything else that says so.
    const BLOCK_TITLES: [&str; 6] = [
        "access denied",
        "403 forbidden",
        "forbidden",
        "blocked",
        "request blocked",
        "you have been blocked",
    ];
    let trimmed = text.trim_start();
    if BLOCK_TITLES.contains(&title.as_str())
        || trimmed.starts_with("access to this page has been denied")
        || trimmed.starts_with("access denied")
    {
        let signal = if probe.title.trim().is_empty() {
            format!(
                "text: {}",
                probe.text.trim().chars().take(60).collect::<String>()
            )
        } else {
            title_signal()
        };
        return found("generic", Block, signal);
    }
    for (what, haystack) in [("title", &title), ("text", &text)] {
        if let Some(m) = CHECK_PHRASES.find(haystack) {
            return found("generic", Interactive, format!("{what}: {}", m.as_str()));
        }
    }
    None
}

/// `scheme://host[:port]` of `url`, or the address itself.
fn origin(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) if parsed.has_host() => parsed.origin().ascii_serialization(),
        _ => url.to_owned(),
    }
}

/// What the agent should do about it.
fn advice(found: &Detection, url: &str, waited: Option<u64>) -> String {
    let origin = origin(url);
    let Detection {
        vendor,
        kind,
        signal,
    } = found;
    if *kind == Kind::Block {
        return format!(
            "{origin} refused this visit ({vendor} block page; {signal}). Don't retry or reload \
             in a loop: that makes the block worse for the user. Better to stop here, or to \
             tell the user the site blocked the visit."
        );
    }
    let still = waited.map_or_else(String::new, |s| format!("; still there after {s} s"));
    format!(
        "A {vendor} {} bot check is in the way on {origin} ({signal}{still}). Don't retry, \
         reload or try to solve it yourself: call browser_handoff so the user does it (for \
         example \"Solve the security check on {origin}\"), then carry on; or stop and tell the \
         user.",
        kind.name()
    )
}

impl Agent<'_> {
    /// One look at the page, or nothing when it can't be read in time.
    async fn probe(&self) -> Option<Probe> {
        let args = json!({ "selectors": ELEMENTS, "idPrefixes": ID_PREFIXES });
        let read = tokio::time::timeout(PROBE_TIMEOUT, self.helper(Helper::Probe, args))
            .await
            .ok()?
            .ok()?;
        serde_json::from_value(read).ok()
    }

    /// `result` with `challenge` and `warning` when the page is a bot check
    /// or a block page (or `challenge_cleared` when one cleared by itself).
    pub(super) async fn with_challenge(&self, mut result: Value) -> Value {
        if self.driver.dialogs.held().is_some() {
            return result;
        }
        let url = self.summary().await["url"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !(url.starts_with("http:") || url.starts_with("https:")) {
            return result;
        }
        let started = Instant::now();
        let Some(probe) = self.probe().await else {
            return result;
        };
        let Some(mut detection) = classify(&probe) else {
            return result;
        };
        let mut probed_url = probe.url;
        let mut waited = None;
        if detection.kind == Kind::NonInteractive {
            while started.elapsed() < CLEAR_WAIT {
                tokio::time::sleep(CLEAR_POLL).await;
                if self.driver.dialogs.held().is_some() {
                    return result;
                }
                // Mid-navigation it can't be read: look again.
                let Some(probe) = self.probe().await else {
                    continue;
                };
                match classify(&probe) {
                    None => {
                        let now = self.summary().await;
                        result["url"] = now["url"].clone();
                        result["title"] = now["title"].clone();
                        result["challenge_cleared"] = json!(format!(
                            "a {} check cleared by itself after {} s",
                            detection.vendor,
                            started.elapsed().as_secs_f64().round() as u64
                        ));
                        return result;
                    }
                    Some(now) => {
                        detection = now;
                        probed_url = probe.url;
                        if detection.kind != Kind::NonInteractive {
                            break;
                        }
                    }
                }
            }
            if detection.kind == Kind::NonInteractive {
                waited = Some(started.elapsed().as_secs_f64().round() as u64);
            }
        }
        result["challenge"] = json!({
            "vendor": detection.vendor,
            "kind": detection.kind.name(),
            "signal": detection.signal,
            "url": probed_url,
        });
        result["warning"] = json!(advice(&detection, &probed_url, waited));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(title: &str, text: &str) -> Probe {
        Probe {
            url: "https://a.example/x?y".into(),
            title: title.into(),
            text: text.chars().take(3000).collect(),
            text_length: text.chars().count(),
            ..Probe::default()
        }
    }

    fn with(mut probe: Probe, selector: &str, visible: bool) -> Probe {
        probe.elements.insert(
            selector.into(),
            Element {
                found: true,
                visible,
                src: String::new(),
            },
        );
        probe
    }

    fn kind(probe: &Probe) -> Option<(&'static str, Kind)> {
        classify(probe).map(|d| (d.vendor, d.kind))
    }

    #[test]
    fn cloudflare() {
        let waiting = page("Just a moment...", "Checking the connection.");
        assert_eq!(kind(&waiting), Some(("cloudflare", Kind::NonInteractive)));
        assert_eq!(
            classify(&waiting).unwrap().signal,
            "title: Just a moment..."
        );
        let asking = with(
            page(
                "Just a moment...",
                "Verify you are human by completing the action below.",
            ),
            ".cf-turnstile",
            true,
        );
        assert_eq!(kind(&asking), Some(("cloudflare", Kind::Interactive)));
        let blocked = with(
            page(
                "Attention Required!",
                "Sorry, you have been blocked. Cloudflare Ray ID: 123",
            ),
            "#cf-error-details",
            true,
        );
        assert_eq!(kind(&blocked), Some(("cloudflare", Kind::Block)));
        let mut framed = page("Example", "One moment.");
        framed.iframes = vec![
            "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/h/b/turnstile".into(),
        ];
        assert_eq!(kind(&framed), Some(("cloudflare", Kind::NonInteractive)));
        assert!(
            classify(&framed)
                .unwrap()
                .signal
                .starts_with("iframe https://")
        );
    }

    #[test]
    fn articles_and_ordinary_pages_are_not_checks() {
        let article = format!(
            "{} To sign up, verify you are human. {}",
            "Words of the article. ".repeat(60),
            "More words. ".repeat(40)
        );
        assert!(article.len() > SHORT_PAGE);
        assert_eq!(kind(&page("How captchas work", &article)), None);
        assert_eq!(
            kind(&page(
                "Example Domain",
                "This domain is for use in examples."
            )),
            None
        );
        assert_eq!(
            kind(&page("Products", "Robot checks are our new product")),
            None
        );
    }

    #[test]
    fn generic_checks_and_blocks() {
        assert_eq!(
            kind(&page("Hold on", "Please verify you are a human")),
            Some(("generic", Kind::Interactive))
        );
        assert_eq!(
            kind(&page("Hold on", "Running a bot check")),
            Some(("generic", Kind::Interactive))
        );
        let forbidden = page("403 Forbidden", "");
        assert_eq!(kind(&forbidden), Some(("generic", Kind::Block)));
        assert_eq!(classify(&forbidden).unwrap().signal, "title: 403 Forbidden");
        let untitled = page("", "Access denied. You do not have access to this server.");
        assert_eq!(kind(&untitled), Some(("generic", Kind::Block)));
        assert!(
            classify(&untitled)
                .unwrap()
                .signal
                .starts_with("text: Access denied")
        );
    }

    #[test]
    fn vendors() {
        assert_eq!(
            kind(&page("Access", "Press & Hold to confirm you are a human")),
            Some(("perimeterx", Kind::Interactive))
        );
        assert_eq!(
            kind(&with(page("", ""), "[id^=px-captcha]", true)),
            Some(("perimeterx", Kind::Interactive))
        );
        let mut sorry = page("", "Our systems have detected unusual traffic.");
        sorry.url = "https://www.google.com/sorry/index?continue=x".into();
        let found = classify(&sorry).unwrap();
        assert_eq!(
            (found.vendor, found.signal.as_str()),
            ("recaptcha", "url: google.com/sorry")
        );
        let mut anchor = page("Hold on", "Please complete the captcha");
        anchor.iframes = vec!["https://www.google.com/recaptcha/api2/anchor?ar=1&k=key".into()];
        assert_eq!(kind(&anchor), Some(("recaptcha", Kind::Interactive)));
        let mut hcaptcha = page("Security check", "One more step");
        hcaptcha.iframes = vec!["https://newassets.hcaptcha.com/captcha/v1/abc".into()];
        assert_eq!(kind(&hcaptcha), Some(("hcaptcha", Kind::Interactive)));
        let mut datadome = page("", "");
        datadome.iframes = vec!["https://geo.captcha-delivery.com/interstitial/?x".into()];
        assert_eq!(kind(&datadome), Some(("datadome", Kind::NonInteractive)));
        let mut imperva = page("", "Request unsuccessful. Incapsula incident ID: 1");
        imperva.scripts = vec!["https://a.example/_Incapsula_Resource?x".into()];
        assert_eq!(kind(&imperva), Some(("imperva", Kind::Block)));
        assert_eq!(
            kind(&page(
                "Access Denied",
                "You don't have permission to access this. Reference #18"
            )),
            Some(("akamai", Kind::Block))
        );
        let mut waf = page("", "");
        waf.scripts = vec!["https://x.token.awswaf.com/x/challenge.js".into()];
        assert_eq!(kind(&waf), Some(("aws-waf", Kind::NonInteractive)));
    }

    #[test]
    fn advice_says_what_to_do() {
        let check = Detection {
            vendor: "cloudflare",
            kind: Kind::Interactive,
            signal: "title: Just a moment...".into(),
        };
        let said = advice(&check, "https://a.example/x?y", None);
        assert!(said.contains("browser_handoff"), "{said}");
        assert!(said.contains("on https://a.example ("), "{said}");
        let waited = advice(
            &Detection {
                kind: Kind::NonInteractive,
                ..check.clone()
            },
            "https://a.example:8443/",
            Some(10),
        );
        assert!(waited.contains("on https://a.example:8443 (") && waited.contains("after 10 s"));
        let block = advice(
            &Detection {
                kind: Kind::Block,
                ..check
            },
            "https://a.example/x",
            None,
        );
        assert!(block.contains("stop"), "{block}");
        assert!(block.contains("Don't retry"), "{block}");
    }
}

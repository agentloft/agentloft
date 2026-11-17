//! `emulate`: how the tab presents itself.
//!
//! A size is the viewers' responsive mode (ADR-0017 §6), so every viewer
//! shows the page as the agent set it and a person can end it from the
//! viewer's strip. Everything else (media features, user agent, location,
//! time zone, locale, network, CPU, touch) is the page's alone. Every
//! setting is checked before any is applied; each one applied or refused
//! is reported, and what was set is remembered so it can be put back.

use super::super::automation::Agent;
use super::super::lock;
use super::{NetworkUse, PageCapture, network_off};
use loft_page_script::{BrowserError, EmulateOptions, SCRIPT_TIMEOUT};
use loft_protocol::PageDevice;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// What an agent set on a page, to put back.
#[derive(Default)]
pub(super) struct Emulation {
    viewport: bool,
    /// Media features by CSS name (`prefers-color-scheme`), and the media
    /// type: Chrome replaces them all on every call.
    features: BTreeMap<&'static str, String>,
    media: Option<String>,
    user_agent: bool,
    geolocation: bool,
    /// The site the location permission was given to.
    granted: Option<String>,
    timezone: bool,
    locale: bool,
    network: bool,
    cpu: bool,
    touch: bool,
}

/// The media features an agent may set: option key, CSS feature, values.
const FEATURES: [(&str, &str, &[&str]); 4] = [
    (
        "color_scheme",
        "prefers-color-scheme",
        &["light", "dark", "no-preference"],
    ),
    (
        "reduced_motion",
        "prefers-reduced-motion",
        &["reduce", "no-preference"],
    ),
    ("forced_colors", "forced-colors", &["active", "none"]),
    (
        "contrast",
        "prefers-contrast",
        &["more", "less", "no-preference", "custom"],
    ),
];

const RESET: &str = "reset";
const NOTE_RELOAD: &str = "pages loaded from now on see it; reload for this one";

#[derive(Debug, Clone, PartialEq)]
enum Viewport {
    Reset,
    Set {
        width: u32,
        height: u32,
        mobile: bool,
        user_agent: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum Geolocation {
    Reset,
    Set {
        latitude: f64,
        longitude: f64,
        accuracy: f64,
    },
}

/// Network conditions as DevTools takes them (throughputs in bytes a
/// second, -1 for no limit).
#[derive(Debug, Clone, PartialEq)]
struct Conditions {
    name: &'static str,
    offline: bool,
    latency_ms: f64,
    download: f64,
    upload: f64,
}

impl Conditions {
    fn preset(name: &str) -> Option<Self> {
        let (name, offline, latency_ms, download, upload) = match name {
            "offline" => ("offline", true, 0.0, 0.0, 0.0),
            "slow-3g" => ("slow-3g", false, 2000.0, 50_000.0, 50_000.0),
            "fast-3g" => ("fast-3g", false, 562.5, 180_000.0, 84_375.0),
            "4g" => ("4g", false, 170.0, 1_012_500.0, 1_012_500.0),
            _ => return None,
        };
        Some(Self {
            name,
            offline,
            latency_ms,
            download,
            upload,
        })
    }

    fn json(&self) -> Value {
        json!({
            "name": self.name,
            "offline": self.offline,
            "latency_ms": self.latency_ms,
            "download_bytes_per_s": self.download,
            "upload_bytes_per_s": self.upload,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Network {
    Reset,
    Set(Conditions),
}

/// Everything asked, checked.
#[derive(Debug, Clone, Default, PartialEq)]
struct Plan {
    viewport: Option<Viewport>,
    /// Option key → value (`reset` included).
    features: Vec<(&'static str, String)>,
    media: Option<String>,
    user_agent: Option<String>,
    geolocation: Option<Geolocation>,
    timezone: Option<String>,
    locale: Option<String>,
    /// Under which key (`network` or `offline`), and what.
    network: Option<(&'static str, Network)>,
    cpu: Option<f64>,
    touch: Option<bool>,
}

fn refuse(message: impl Into<String>) -> BrowserError {
    BrowserError::Action(message.into())
}

/// Check every setting before any is applied.
fn plan(options: &EmulateOptions) -> Result<Plan, BrowserError> {
    if options.is_empty() {
        return Err(refuse(
            "emulate needs something to set: viewport, color_scheme, reduced_motion, \
             forced_colors, contrast, media, user_agent, geolocation, timezone, locale, network, \
             offline, cpu_throttling, touch, or reset",
        ));
    }
    let mut plan = Plan::default();
    let asked = [
        &options.color_scheme,
        &options.reduced_motion,
        &options.forced_colors,
        &options.contrast,
    ];
    for ((key, _, values), value) in FEATURES.iter().zip(asked) {
        let Some(value) = value else { continue };
        if value != RESET && !values.contains(&value.as_str()) {
            return Err(refuse(format!(
                "{key} must be one of {}, or reset; got {value:?}",
                values.join(", ")
            )));
        }
        plan.features.push((key, value.clone()));
    }
    if let Some(media) = &options.media {
        if !matches!(media.as_str(), "screen" | "print" | RESET) {
            return Err(refuse(format!(
                "media must be screen, print or reset; got {media:?}"
            )));
        }
        plan.media = Some(media.clone());
    }
    if let Some(viewport) = &options.viewport {
        plan.viewport = Some(viewport_of(viewport)?);
    }
    if let Some(geolocation) = &options.geolocation {
        plan.geolocation = Some(geolocation_of(geolocation)?);
    }
    if let Some(network) = &options.network {
        plan.network = Some(("network", network_of(network)?));
    }
    if let Some(offline) = options.offline {
        if plan.network.is_some() {
            return Err(refuse("use network or offline, not both"));
        }
        let conditions = match offline {
            true => Conditions::preset("offline").map_or(Network::Reset, Network::Set),
            false => Network::Reset,
        };
        plan.network = Some(("offline", conditions));
    }
    if let Some(rate) = options.cpu_throttling {
        if !(rate.is_finite() && (1.0..=20.0).contains(&rate)) {
            return Err(refuse("cpu_throttling must be from 1 (not slowed) to 20"));
        }
        plan.cpu = Some(rate);
    }
    for (key, value) in [
        ("user_agent", &options.user_agent),
        ("timezone", &options.timezone),
        ("locale", &options.locale),
    ] {
        if value.as_ref().is_some_and(|v| v.trim().is_empty()) {
            return Err(refuse(format!(
                "{key} must not be empty (\"reset\" puts it back)"
            )));
        }
    }
    plan.user_agent = options.user_agent.as_ref().map(|v| v.trim().to_owned());
    plan.timezone = options.timezone.as_ref().map(|v| v.trim().to_owned());
    plan.locale = options.locale.as_ref().map(|v| v.trim().to_owned());
    plan.touch = options.touch;
    Ok(plan)
}

fn viewport_of(value: &Value) -> Result<Viewport, BrowserError> {
    if value == RESET {
        return Ok(Viewport::Reset);
    }
    let Some(object) = value.as_object() else {
        return Err(refuse(format!(
            "viewport must be {{width, height}} or \"reset\"; got {value}"
        )));
    };
    let side = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_f64)
            .filter(|n| n.fract() == 0.0 && (120.0..=3840.0).contains(n))
            .map(|n| n as u32)
            .ok_or_else(|| {
                refuse(format!(
                    "viewport {key} must be a whole number of CSS pixels from 120 to 3840"
                ))
            })
    };
    let (width, height) = (side("width")?, side("height")?);
    let mobile = match object.get("mobile") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(mobile)) => *mobile,
        Some(_) => return Err(refuse("viewport mobile must be true or false")),
    };
    let user_agent = match object.get("user_agent") {
        None | Some(Value::Null) => None,
        Some(Value::String(agent)) => Some(agent.clone()),
        Some(_) => return Err(refuse("viewport user_agent must be a string")),
    };
    Ok(Viewport::Set {
        width,
        height,
        mobile,
        user_agent,
    })
}

fn geolocation_of(value: &Value) -> Result<Geolocation, BrowserError> {
    if value == RESET {
        return Ok(Geolocation::Reset);
    }
    let Some(object) = value.as_object() else {
        return Err(refuse(format!(
            "geolocation must be {{latitude, longitude}} or \"reset\"; got {value}"
        )));
    };
    let number = |key: &str, limit: f64| {
        object
            .get(key)
            .and_then(Value::as_f64)
            .filter(|n| (-limit..=limit).contains(n))
    };
    let (Some(latitude), Some(longitude)) = (number("latitude", 90.0), number("longitude", 180.0))
    else {
        return Err(refuse(
            "geolocation needs latitude (-90 to 90) and longitude (-180 to 180)",
        ));
    };
    let accuracy = match object.get("accuracy") {
        None | Some(Value::Null) => 50.0,
        Some(value) => value
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or_else(|| {
                refuse("geolocation accuracy must be a non-negative number of meters")
            })?,
    };
    Ok(Geolocation::Set {
        latitude,
        longitude,
        accuracy,
    })
}

fn network_of(value: &Value) -> Result<Network, BrowserError> {
    let wrong = || {
        refuse(format!(
            "network must be offline, slow-3g, fast-3g, 4g, reset or {{latency_ms, download_kbps, upload_kbps}}; got {value}"
        ))
    };
    match value {
        Value::String(name) if name == RESET => Ok(Network::Reset),
        Value::String(name) => Conditions::preset(name).map(Network::Set).ok_or_else(wrong),
        Value::Object(object) => {
            let number = |key: &str| match object.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(value) => value
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .map(Some)
                    .ok_or_else(|| refuse(format!("network {key} must be a number, 0 or more"))),
            };
            let latency_ms = number("latency_ms")?.unwrap_or(0.0);
            let throughput = |kbps: Option<f64>| kbps.map_or(-1.0, |k| k * 1000.0 / 8.0);
            let download = throughput(number("download_kbps")?);
            let upload = throughput(number("upload_kbps")?);
            Ok(Network::Set(Conditions {
                name: "custom",
                offline: false,
                latency_ms,
                download,
                upload,
            }))
        }
        _ => Err(wrong()),
    }
}

/// `reset: true`: everything set earlier and not set again now goes back.
fn with_resets(mut plan: Plan, set: &Emulation) -> Plan {
    if set.viewport && plan.viewport.is_none() {
        plan.viewport = Some(Viewport::Reset);
    }
    for (key, feature, _) in FEATURES {
        if set.features.contains_key(feature) && !plan.features.iter().any(|(k, _)| *k == key) {
            plan.features.push((key, RESET.into()));
        }
    }
    if set.media.is_some() && plan.media.is_none() {
        plan.media = Some(RESET.into());
    }
    if set.user_agent && plan.user_agent.is_none() {
        plan.user_agent = Some(RESET.into());
    }
    if set.timezone && plan.timezone.is_none() {
        plan.timezone = Some(RESET.into());
    }
    if set.locale && plan.locale.is_none() {
        plan.locale = Some(RESET.into());
    }
    if set.network && plan.network.is_none() {
        plan.network = Some(("network", Network::Reset));
    }
    if set.cpu && plan.cpu.is_none() {
        plan.cpu = Some(1.0);
    }
    if set.touch && plan.touch.is_none() {
        plan.touch = Some(false);
    }
    if set.geolocation && plan.geolocation.is_none() {
        plan.geolocation = Some(Geolocation::Reset);
    }
    plan
}

/// The origin a site's permissions are given to; `None` but for http(s).
fn origin(address: &str) -> Option<String> {
    let url = url::Url::parse(address).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

/// What each key did.
#[derive(Default)]
struct Report {
    applied: Map<String, Value>,
    failed: Map<String, Value>,
}

impl Report {
    fn done(&mut self, key: &str, value: Value) {
        self.applied.insert(key.to_owned(), value);
    }

    fn refused(&mut self, key: &str, error: impl ToString) {
        self.failed
            .insert(key.to_owned(), Value::String(error.to_string()));
    }

    fn result(self) -> Result<Value, BrowserError> {
        if self.applied.is_empty() && !self.failed.is_empty() {
            let said: Vec<String> = self
                .failed
                .iter()
                .map(|(key, error)| format!("{key}: {}", error.as_str().unwrap_or_default()))
                .collect();
            return Err(refuse(format!(
                "the browser refused every setting: {}",
                said.join("; ")
            )));
        }
        let mut out = Map::new();
        out.insert("applied".into(), Value::Object(self.applied));
        if !self.failed.is_empty() {
            out.insert("failed".into(), Value::Object(self.failed));
        }
        Ok(Value::Object(out))
    }
}

/// `emulate`.
pub(super) async fn emulate(
    agent: &Agent<'_>,
    state: &PageCapture,
    options: EmulateOptions,
) -> Result<Value, BrowserError> {
    let mut plan = plan(&options)?;
    if options.reset {
        plan = with_resets(plan, &lock(&state.emulation));
    }
    let mut report = Report::default();
    if let Some(viewport) = plan.viewport {
        apply_viewport(agent, state, viewport, &mut report).await;
    }
    if !plan.features.is_empty() || plan.media.is_some() {
        apply_media(
            agent,
            state,
            &plan.features,
            plan.media.as_deref(),
            &mut report,
        )
        .await;
    }
    if let Some(user_agent) = plan.user_agent {
        apply_user_agent(agent, state, &user_agent, &mut report).await;
    }
    if let Some(geolocation) = plan.geolocation {
        apply_geolocation(agent, state, geolocation, &mut report).await;
    }
    if let Some(timezone) = plan.timezone {
        let was = lock(&state.emulation).timezone;
        let applied = override_again(
            agent,
            was,
            "Emulation.setTimezoneOverride",
            json!({ "timezoneId": "" }),
            (timezone != RESET).then(|| json!({ "timezoneId": timezone })),
            "setting the time zone",
        )
        .await;
        match applied {
            Ok(on) => {
                lock(&state.emulation).timezone = on;
                report.done("timezone", json!(timezone));
            }
            Err(err) => report.refused("timezone", err),
        }
    }
    if let Some(locale) = plan.locale {
        let was = lock(&state.emulation).locale;
        let applied = override_again(
            agent,
            was,
            "Emulation.setLocaleOverride",
            json!({}),
            (locale != RESET).then(|| json!({ "locale": locale })),
            "setting the locale",
        )
        .await;
        match applied {
            Ok(on) => {
                lock(&state.emulation).locale = on;
                let value = if on {
                    json!({ "locale": locale, "note": NOTE_RELOAD })
                } else {
                    json!(RESET)
                };
                report.done("locale", value);
            }
            Err(err) => report.refused("locale", err),
        }
    }
    if let Some((key, network)) = plan.network {
        apply_network(agent, state, key, network, &mut report).await;
    }
    if let Some(rate) = plan.cpu {
        let sent = agent
            .send(
                "Emulation.setCPUThrottlingRate",
                json!({ "rate": rate }),
                SCRIPT_TIMEOUT,
                "slowing the CPU",
            )
            .await;
        match sent {
            Ok(_) => {
                lock(&state.emulation).cpu = rate > 1.0;
                report.done("cpu_throttling", json!(rate));
            }
            Err(err) => report.refused("cpu_throttling", err),
        }
    }
    if let Some(enabled) = plan.touch {
        let sent = agent
            .send(
                "Emulation.setTouchEmulationEnabled",
                json!({ "enabled": enabled, "maxTouchPoints": if enabled { 5 } else { 1 } }),
                SCRIPT_TIMEOUT,
                "setting touch",
            )
            .await;
        match sent {
            Ok(_) => {
                lock(&state.emulation).touch = enabled;
                report.done("touch", json!(enabled));
            }
            Err(err) => report.refused("touch", err),
        }
    }
    report.result()
}

async fn apply_viewport(
    agent: &Agent<'_>,
    state: &PageCapture,
    viewport: Viewport,
    report: &mut Report,
) {
    let (device, reported) = match viewport {
        Viewport::Reset => (None, json!(RESET)),
        Viewport::Set {
            width,
            height,
            mobile,
            user_agent,
        } => (
            Some(PageDevice {
                name: "Responsive".into(),
                width,
                height,
                mobile,
                user_agent,
            }),
            json!({ "width": width, "height": height, "mobile": mobile }),
        ),
    };
    let on = device.is_some();
    match agent.browser.emulate_device(agent.page, device).await {
        Ok(()) => {
            lock(&state.emulation).viewport = on;
            report.done("viewport", reported);
        }
        Err(err) => report.refused("viewport", err.message),
    }
}

async fn apply_media(
    agent: &Agent<'_>,
    state: &PageCapture,
    asked: &[(&'static str, String)],
    media: Option<&str>,
    report: &mut Report,
) {
    let (mut features, mut media_type) = {
        let emulation = lock(&state.emulation);
        (emulation.features.clone(), emulation.media.clone())
    };
    for (key, value) in asked {
        let Some((_, feature, _)) = FEATURES.iter().find(|(k, _, _)| k == key) else {
            continue;
        };
        if value == RESET {
            features.remove(feature);
        } else {
            features.insert(feature, value.clone());
        }
    }
    if let Some(media) = media {
        media_type = (media != RESET).then(|| media.to_owned());
    }
    let list: Vec<Value> = features
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    let sent = agent
        .send(
            "Emulation.setEmulatedMedia",
            json!({ "media": media_type.as_deref().unwrap_or(""), "features": list }),
            SCRIPT_TIMEOUT,
            "emulating media",
        )
        .await;
    let keys = asked
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .chain(media.map(|m| ("media", m)));
    match sent {
        Ok(_) => {
            {
                let mut emulation = lock(&state.emulation);
                emulation.features = features;
                emulation.media = media_type;
            }
            for (key, value) in keys {
                report.done(key, json!(value));
            }
        }
        Err(err) => {
            for (key, _) in keys {
                report.refused(key, &err);
            }
        }
    }
}

async fn apply_user_agent(
    agent: &Agent<'_>,
    state: &PageCapture,
    user_agent: &str,
    report: &mut Report,
) {
    if user_agent == RESET {
        // The device's own, if the page has one, else an ordinary Chrome's.
        let device = lock(&agent.page.state)
            .info
            .device
            .as_ref()
            .and_then(|d| d.user_agent.clone());
        agent
            .browser
            .set_user_agent(agent.page, device.as_deref())
            .await;
        lock(&state.emulation).user_agent = false;
        report.done("user_agent", json!(RESET));
        return;
    }
    let sent = agent
        .send(
            "Emulation.setUserAgentOverride",
            json!({ "userAgent": user_agent }),
            SCRIPT_TIMEOUT,
            "setting the user agent",
        )
        .await;
    match sent {
        Ok(_) => {
            lock(&state.emulation).user_agent = true;
            report.done(
                "user_agent",
                json!({ "user_agent": user_agent, "note": NOTE_RELOAD }),
            );
        }
        Err(err) => report.refused("user_agent", err),
    }
}

/// Give the geolocation permission to `origin` or take it back (to
/// "prompt"); browser-wide, so no session.
async fn permission(agent: &Agent<'_>, origin: &str, setting: &str) -> Result<(), String> {
    agent
        .browser
        .cdp
        .send_within(
            "Browser.setPermission",
            json!({ "permission": { "name": "geolocation" }, "setting": setting, "origin": origin }),
            None,
            SCRIPT_TIMEOUT,
        )
        .await
        .map(|_| ())
}

async fn apply_geolocation(
    agent: &Agent<'_>,
    state: &PageCapture,
    geolocation: Geolocation,
    report: &mut Report,
) {
    let granted_before = lock(&state.emulation).granted.clone();
    let Geolocation::Set {
        latitude,
        longitude,
        accuracy,
    } = geolocation
    else {
        let cleared = agent
            .send(
                "Emulation.clearGeolocationOverride",
                json!({}),
                SCRIPT_TIMEOUT,
                "clearing the location",
            )
            .await;
        match cleared {
            Ok(_) => {
                if let Some(origin) = &granted_before {
                    let _ = permission(agent, origin, "prompt").await;
                }
                let mut emulation = lock(&state.emulation);
                emulation.geolocation = false;
                emulation.granted = None;
                report.done("geolocation", json!(RESET));
            }
            Err(err) => report.refused("geolocation", err),
        }
        return;
    };
    let sent = agent
        .send(
            "Emulation.setGeolocationOverride",
            json!({ "latitude": latitude, "longitude": longitude, "accuracy": accuracy }),
            SCRIPT_TIMEOUT,
            "setting the location",
        )
        .await;
    if let Err(err) = sent {
        report.refused("geolocation", err);
        return;
    }
    lock(&state.emulation).geolocation = true;
    let mut reported =
        json!({ "latitude": latitude, "longitude": longitude, "accuracy": accuracy });
    let url = lock(&agent.page.state).info.url.clone();
    match origin(&url) {
        Some(origin) => {
            // Nobody could answer the permission question in a headless
            // browser; the site only ever gets the emulated location.
            match permission(agent, &origin, "granted").await {
                Ok(()) => {
                    if let Some(before) = granted_before.filter(|b| *b != origin) {
                        let _ = permission(agent, &before, "prompt").await;
                    }
                    lock(&state.emulation).granted = Some(origin.clone());
                    reported["granted_to"] = json!(origin);
                }
                Err(err) => {
                    reported["note"] = json!(format!(
                        "the site may ask for it: the permission couldn't be given ({err})"
                    ));
                }
            }
        }
        None => {
            reported["note"] =
                json!("open the site first: its permission is given for the site open now");
        }
    }
    report.done("geolocation", reported);
}

/// Chrome takes a new time zone or locale only once the old one is gone:
/// clear it first if one was set, then set the new one (`None`: leave it
/// cleared). Whether one is set afterwards.
async fn override_again(
    agent: &Agent<'_>,
    was_set: bool,
    method: &str,
    clear: Value,
    set: Option<Value>,
    what: &str,
) -> Result<bool, BrowserError> {
    if was_set || set.is_none() {
        let _ = agent.send(method, clear, SCRIPT_TIMEOUT, what).await;
    }
    match set {
        Some(params) => agent
            .send(method, params, SCRIPT_TIMEOUT, what)
            .await
            .map(|_| true),
        None => Ok(false),
    }
}

fn conditions(use_: &mut NetworkUse) -> &mut bool {
    &mut use_.conditions
}

async fn apply_network(
    agent: &Agent<'_>,
    state: &PageCapture,
    key: &'static str,
    network: Network,
    report: &mut Report,
) {
    let Network::Set(set) = network else {
        // Back to an ordinary network: never fails.
        let _ = agent
            .send(
                "Network.emulateNetworkConditions",
                json!({ "offline": false, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1 }),
                SCRIPT_TIMEOUT,
                "resetting the network",
            )
            .await;
        network_off(&agent.browser.cdp, &agent.page.session, state, conditions).await;
        lock(&state.emulation).network = false;
        report.done(
            key,
            if key == "offline" {
                json!(false)
            } else {
                json!(RESET)
            },
        );
        return;
    };
    // Conditions hold only while the page's `Network` domain is on.
    let applied = match agent.network_on(state, conditions).await {
        Ok(()) => agent
            .send(
                "Network.emulateNetworkConditions",
                json!({
                    "offline": set.offline,
                    "latency": set.latency_ms,
                    "downloadThroughput": set.download,
                    "uploadThroughput": set.upload,
                }),
                SCRIPT_TIMEOUT,
                "emulating the network",
            )
            .await
            .map(|_| ()),
        Err(err) => Err(err),
    };
    match applied {
        Ok(()) => {
            lock(&state.emulation).network = true;
            report.done(
                key,
                if key == "offline" {
                    json!(true)
                } else {
                    set.json()
                },
            );
        }
        Err(err) => {
            network_off(&agent.browser.cdp, &agent.page.session, state, conditions).await;
            lock(&state.emulation).network = false;
            report.refused(key, err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(value: Value) -> EmulateOptions {
        serde_json::from_value(value).unwrap()
    }

    fn check(value: Value) -> Result<Plan, String> {
        plan(&options(value)).map_err(|e| e.to_string())
    }

    #[test]
    fn every_setting_is_checked_first() {
        assert!(
            check(json!({}))
                .unwrap_err()
                .contains("emulate needs something")
        );
        assert!(check(json!({ "color_scheme": "dark" })).is_ok());
        let purple = check(json!({ "color_scheme": "purple" })).unwrap_err();
        assert_eq!(
            purple,
            "color_scheme must be one of light, dark, no-preference, or reset; got \"purple\""
        );
        let plan = check(json!({ "media": "print", "reduced_motion": "reset" })).unwrap();
        assert_eq!(plan.media.as_deref(), Some("print"));
        assert_eq!(plan.features, [("reduced_motion", "reset".to_owned())]);
        assert!(
            check(json!({ "media": "tv" }))
                .unwrap_err()
                .starts_with("media must be")
        );

        let phone =
            check(json!({ "viewport": { "width": 390, "height": 844, "mobile": true } })).unwrap();
        assert_eq!(
            phone.viewport,
            Some(Viewport::Set {
                width: 390,
                height: 844,
                mobile: true,
                user_agent: None
            })
        );
        assert!(
            check(json!({ "viewport": { "width": 5, "height": 844 } }))
                .unwrap_err()
                .contains("viewport width must be a whole number")
        );
        assert!(check(json!({ "viewport": { "width": 390.5, "height": 844 } })).is_err());
        assert!(
            check(json!({ "viewport": { "width": 390, "height": 844, "mobile": "yes" } }))
                .unwrap_err()
                .contains("mobile")
        );
        assert_eq!(
            check(json!({ "viewport": "reset" })).unwrap().viewport,
            Some(Viewport::Reset)
        );
        assert!(
            check(json!({ "viewport": "small" }))
                .unwrap_err()
                .starts_with("viewport must be {width, height} or \"reset\"; got \"small\"")
        );

        assert!(check(json!({ "geolocation": { "latitude": 48.85, "longitude": 2.35 } })).is_ok());
        assert!(
            check(json!({ "geolocation": { "latitude": 91, "longitude": 2.35 } }))
                .unwrap_err()
                .contains("latitude (-90 to 90)")
        );
        assert!(
            check(json!({ "geolocation": { "latitude": 1, "longitude": 2, "accuracy": -1 } }))
                .unwrap_err()
                .contains("accuracy")
        );

        assert_eq!(
            check(json!({ "network": "slow-3g" })).unwrap().network,
            Some((
                "network",
                Network::Set(Conditions::preset("slow-3g").unwrap())
            ))
        );
        let custom =
            check(json!({ "network": { "latency_ms": 600, "download_kbps": 800 } })).unwrap();
        assert_eq!(
            custom.network,
            Some((
                "network",
                Network::Set(Conditions {
                    name: "custom",
                    offline: false,
                    latency_ms: 600.0,
                    download: 100_000.0,
                    upload: -1.0
                })
            ))
        );
        assert!(
            check(json!({ "network": { "latency_ms": -1 } }))
                .unwrap_err()
                .contains("network latency_ms must be a number, 0 or more")
        );
        assert!(
            check(json!({ "network": "2g" }))
                .unwrap_err()
                .starts_with("network must be")
        );
        assert_eq!(
            check(json!({ "network": "4g", "offline": true })).unwrap_err(),
            "use network or offline, not both"
        );
        assert_eq!(
            check(json!({ "offline": false })).unwrap().network,
            Some(("offline", Network::Reset))
        );
        assert_eq!(
            check(json!({ "cpu_throttling": 4 })).unwrap().cpu,
            Some(4.0)
        );
        assert!(
            check(json!({ "cpu_throttling": 0.5 }))
                .unwrap_err()
                .contains("from 1")
        );
        assert_eq!(
            check(json!({ "timezone": " " })).unwrap_err(),
            "timezone must not be empty (\"reset\" puts it back)"
        );
        assert!(check(json!({ "reset": true })).is_ok());
    }

    #[test]
    fn presets_are_devtools_own() {
        let fast = Conditions::preset("fast-3g").unwrap();
        assert_eq!(
            (fast.latency_ms, fast.download, fast.upload),
            (562.5, 180_000.0, 84_375.0)
        );
        let slow = Conditions::preset("slow-3g").unwrap();
        assert_eq!(
            (slow.latency_ms, slow.download, slow.upload),
            (2000.0, 50_000.0, 50_000.0)
        );
        let lte = Conditions::preset("4g").unwrap();
        assert_eq!(
            (lte.latency_ms, lte.download, lte.upload),
            (170.0, 1_012_500.0, 1_012_500.0)
        );
        assert!(Conditions::preset("offline").unwrap().offline);
        assert!(Conditions::preset("2g").is_none());
    }

    #[test]
    fn reset_puts_back_what_was_set() {
        let set = Emulation {
            viewport: true,
            features: BTreeMap::from([("prefers-color-scheme", "dark".to_owned())]),
            timezone: true,
            network: true,
            cpu: true,
            geolocation: true,
            ..Emulation::default()
        };
        let plan = with_resets(
            plan(&options(json!({ "reset": true, "timezone": "Asia/Tokyo" }))).unwrap(),
            &set,
        );
        assert_eq!(plan.viewport, Some(Viewport::Reset));
        assert_eq!(plan.features, [("color_scheme", "reset".to_owned())]);
        assert_eq!(plan.timezone.as_deref(), Some("Asia/Tokyo"), "asked wins");
        assert_eq!(plan.network, Some(("network", Network::Reset)));
        assert_eq!(plan.cpu, Some(1.0));
        assert_eq!(plan.geolocation, Some(Geolocation::Reset));
        assert_eq!(plan.locale, None);
        assert_eq!(plan.touch, None);
    }

    #[test]
    fn permissions_go_to_the_sites_origin() {
        assert_eq!(
            origin("https://a.test:8443/x?y").as_deref(),
            Some("https://a.test:8443")
        );
        assert_eq!(origin("http://a.test/").as_deref(), Some("http://a.test"));
        assert_eq!(origin("about:blank"), None);
        assert_eq!(origin("file:///tmp/a.html"), None);
    }
}

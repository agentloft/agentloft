//! Credential redaction for captured network exchanges (ADR-0017 §8).
//!
//! Nothing a capture returns has a credential in it: an agent's context is
//! not a place for the user's sessions, keys or passwords. Secrets are found
//! two ways:
//!
//! - **By name**: header names, query, fragment, form and multipart fields,
//!   JSON keys ([`is_secret_name`]); authorization headers and every cookie
//!   value whatever its name.
//! - **By shape**: path segments that look like tokens ([`looks_like_token`]).
//!
//! A [`Redactor`] remembers every value it took out, so the second pass of a
//! capture ([`Redactor::known_in_text`], [`Redactor::known_in_json`]) takes
//! the same value out wherever it shows up again (a token a login answered,
//! sent back in a later request's address).
//!
//! The design is inspired by OpenCode Browse's redaction rules; this is our
//! own implementation of them, with a few rules tightened (see the comments
//! on [`is_secret_name`] and [`Redactor::candidates`]).

use serde_json::Value;
use std::collections::BTreeMap;

/// What a secret is replaced with, everywhere (never percent-encoded).
pub(super) const REDACTED: &str = "[redacted]";

/// Learned values shorter than this are not looked for in headers and
/// addresses (too likely to be a word or a number by chance)...
pub(super) const MIN_IN_TEXT: usize = 8;
/// ... nor in JSON strings, which are mostly content.
pub(super) const MIN_IN_JSON: usize = 16;
/// A learned number is looked for as a JSON number only from this long.
const MIN_NUMBER: usize = 6;
/// One-time values (passwords, codes) are looked for whatever their length,
/// down to this: a one- to three-character value would take out half the
/// digits of a capture.
const MIN_ONE_TIME: usize = 4;

/// Whether a field name (header, query, fragment, form or multipart field,
/// JSON key) names a credential. ASCII case doesn't matter.
///
/// It is when it contains `cookie`, `credential`, `csrf`, `xsrf`, `token`,
/// `secret`, `session`, `password`, `passwd`, `pwd`, `passcode`,
/// `passphrase`, `signature` or `jwt`; `auth` (but not in `author`, unless it
/// is `authoriz…`/`authoris…`); `otp` at the start or end of a word (`otp`,
/// `totp`, `otpCode`, not `footprint`); `api`, `access`, `refresh` or
/// `private` then `key` (`apikey`, `X-Api-Key`, `private_key`); or a
/// segment (between `-`, `_`, `.`) that is `sig`, `pin` or `code`. A `code`
/// segment right after one that says what kind of code it is and is never a
/// credential (`status_code`, `country-code`, …) doesn't count; nor does a
/// name that is only `key`'s cousins (`keyboard`). The whole name `key`
/// does (an API key in `?key=`).
pub(super) fn is_secret_name(name: &str) -> bool {
    const ANYWHERE: [&str; 14] = [
        "cookie",
        "credential",
        "csrf",
        "xsrf",
        "token",
        "secret",
        "session",
        "password",
        "passwd",
        "pwd",
        "passcode",
        "passphrase",
        "signature",
        "jwt",
    ];
    let lower = name.to_ascii_lowercase();
    if ANYWHERE.iter().any(|w| lower.contains(w))
        || mentions_auth(&lower)
        || has_otp_word(name)
        || lower == "key"
    {
        return true;
    }
    if ["api", "access", "refresh", "private"]
        .iter()
        .any(|prefix| followed_by_key(&lower, prefix))
    {
        return true;
    }
    let segments: Vec<&str> = segments(&lower).collect();
    segments.iter().enumerate().any(|(i, s)| match *s {
        "sig" | "pin" => true,
        "code" => !(i > 0 && HARMLESS_CODES.contains(&segments[i - 1])),
        _ => false,
    })
}

/// Kinds of code that are never credentials: `status_code` is worth seeing.
const HARMLESS_CODES: [&str; 22] = [
    "status", "error", "http", "response", "exit", "reason", "result", "country", "currency",
    "language", "lang", "locale", "region", "zip", "postal", "area", "dial", "color", "colour",
    "char", "key", "event",
];

/// Whether a value learned under this field name is looked for elsewhere
/// whatever its length (down to [`MIN_ONE_TIME`]): passwords and one-time
/// codes are often short.
pub(super) fn is_one_time_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["password", "passwd", "passcode", "passphrase"]
        .iter()
        .any(|w| lower.contains(w))
        || has_otp_word(name)
        || segments(&lower).any(|s| {
            s == "pwd"
                || s == "pin"
                || s.strip_suffix("code")
                    .is_some_and(|p| matches!(p, "" | "mfa" | "2fa" | "verification" | "auth"))
        })
}

/// A name's segments between `-`, `_` and `.`.
fn segments(lower: &str) -> impl Iterator<Item = &str> {
    lower.split(['-', '_', '.'])
}

/// `auth` somewhere other than in `author` (`authorization` and
/// `authorise` count).
fn mentions_auth(lower: &str) -> bool {
    lower.match_indices("auth").any(|(i, _)| {
        let rest = &lower[i + 4..];
        !rest.starts_with("or") || rest.starts_with("oriz") || rest.starts_with("oris")
    })
}

/// `<prefix>key`, `<prefix>-key`, `<prefix>_key` or `<prefix>.key`.
fn followed_by_key(lower: &str, prefix: &str) -> bool {
    lower.match_indices(prefix).any(|(i, _)| {
        let rest = &lower[i + prefix.len()..];
        let rest = rest.strip_prefix(['-', '_', '.']).unwrap_or(rest);
        rest.starts_with("key")
    })
}

/// A word (split at anything but letters and digits, and where camel case
/// starts a new word) that starts or ends with `otp`.
fn has_otp_word(name: &str) -> bool {
    words(name).iter().any(|w| {
        let w = w.to_ascii_lowercase();
        w.starts_with("otp") || w.ends_with("otp")
    })
}

fn words(name: &str) -> Vec<&str> {
    let bytes = name.as_bytes();
    let mut cuts = vec![0];
    for i in 1..bytes.len() {
        let (before, here) = (bytes[i - 1], bytes[i]);
        let next_lower = bytes.get(i + 1).is_some_and(u8::is_ascii_lowercase);
        if here.is_ascii_uppercase()
            && (before.is_ascii_lowercase()
                || before.is_ascii_digit()
                || (before.is_ascii_uppercase() && next_lower))
        {
            cuts.push(i);
        }
    }
    cuts.push(bytes.len());
    cuts.windows(2)
        .flat_map(|w| name[w[0]..w[1]].split(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect()
}

/// Whether a path segment (decoded) looks like a token rather than a name:
/// a JWT, a long hex string, or a long mix of letters and digits as base64
/// is made of. UUIDs (identifiers, not secrets), slugs and template
/// placeholders don't.
pub(super) fn looks_like_token(segment: &str) -> bool {
    let s = segment;
    if s.len() < 24 || is_uuid(s) || s.contains("${") {
        return false;
    }
    if let Some(rest) = s.strip_prefix("eyJ") {
        let pieces: Vec<&str> = rest.split('.').collect();
        let url_safe = |p: &&str| {
            p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        if pieces.len() == 3
            && !pieces[0].is_empty()
            && !pieces[1].is_empty()
            && pieces.iter().all(url_safe)
        {
            return true;
        }
    }
    if s.len() >= 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    let unpadded = s.trim_end_matches('=');
    if s.len() - unpadded.len() > 2
        || unpadded.is_empty()
        || !unpadded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'~' | b'+' | b'/' | b'-'))
    {
        return false;
    }
    let pieces: Vec<&str> = s.split(['-', '_']).collect();
    if pieces.len() > 1
        && pieces
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphabetic()))
    {
        return false;
    }
    let mut distinct: Vec<char> = s.chars().collect();
    distinct.sort_unstable();
    distinct.dedup();
    let digit = s.bytes().any(|b| b.is_ascii_digit());
    let letter = s.bytes().any(|b| b.is_ascii_alphabetic());
    let mixed_case =
        s.bytes().any(|b| b.is_ascii_lowercase()) && s.bytes().any(|b| b.is_ascii_uppercase());
    distinct.len() >= 12 && digit && letter && (mixed_case || s.len() >= 32)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Takes credentials out of a capture, remembering each value it took out.
#[derive(Default)]
pub(super) struct Redactor {
    /// Every value taken out, and whether one-time (looked for at any
    /// length). Ordered, so what is replaced first doesn't depend on a hash.
    learned: BTreeMap<String, bool>,
}

impl Redactor {
    /// Remember a value taken out from under `field` (`None`: a header, a
    /// cookie or a path segment, which are never one-time).
    fn learn(&mut self, value: &str, field: Option<&str>) {
        if value.is_empty() {
            return;
        }
        let one_time = field.is_some_and(is_one_time_name);
        let known = self.learned.entry(value.to_owned()).or_default();
        *known |= one_time;
    }

    #[cfg(test)]
    pub(super) fn knows(&self, value: &str) -> bool {
        self.learned.contains_key(value)
    }

    // -- the first pass: by name and by shape -------------------------------

    /// A header's value, with what is secret in it taken out.
    pub(super) fn header(&mut self, name: &str, value: &str) -> String {
        if value.is_empty() {
            return String::new();
        }
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            ":path" => return self.url(value),
            ":authority" | ":method" | ":scheme" | ":status" | ":protocol" => {
                return value.to_owned();
            }
            "cookie" => return self.cookies(value),
            "set-cookie" => {
                return value
                    .split('\n')
                    .map(|line| self.set_cookie(line))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            "location" | "referer" | "referrer" => return self.url(value),
            _ => {}
        }
        if let Some(cors) = lower.strip_prefix("access-control-")
            && matches!(
                cors,
                "allow-credentials"
                    | "allow-origin"
                    | "allow-methods"
                    | "allow-headers"
                    | "expose-headers"
                    | "max-age"
                    | "request-method"
                    | "request-headers"
                    | "allow-private-network"
                    | "request-private-network"
            )
        {
            return value.to_owned();
        }
        let authorization = matches!(lower.as_str(), "authorization" | "proxy-authorization");
        if authorization
            && let Some(space) = value.find(char::is_whitespace)
            && space > 0
        {
            let credential = value[space..].trim_start();
            if !credential.is_empty() {
                self.learn(credential.trim_end(), None);
                return format!("{} {REDACTED}", &value[..space]);
            }
        }
        if authorization
            || matches!(
                lower.as_str(),
                "x-api-key" | "api-key" | "x-csrf-token" | "csrf-token" | "x-xsrf-token"
            )
            || is_secret_name(&lower)
        {
            self.learn(value.trim(), None);
            return REDACTED.to_owned();
        }
        value.to_owned()
    }

    /// A `Cookie` header: every cookie's value, whatever its name.
    fn cookies(&mut self, value: &str) -> String {
        value
            .split(';')
            .map(|part| {
                let part = part.trim_start();
                self.cookie(part).unwrap_or_else(|| part.to_owned())
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// `name=value` with its value taken out, if it has one.
    fn cookie(&mut self, part: &str) -> Option<String> {
        let (name, value) = part.split_once('=')?;
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            return None;
        }
        self.learn(value, None);
        Some(format!("{}={REDACTED}", name.trim()))
    }

    /// One `Set-Cookie` line: the cookie's value, and any other attribute
    /// with a value that isn't a known cookie attribute.
    fn set_cookie(&mut self, line: &str) -> String {
        const ATTRIBUTES: [&str; 9] = [
            "domain",
            "path",
            "expires",
            "max-age",
            "samesite",
            "priority",
            "partitioned",
            "secure",
            "httponly",
        ];
        line.split(';')
            .enumerate()
            .map(|(i, part)| {
                let part = part.trim_start();
                let name = part.split('=').next().unwrap_or_default().trim();
                if i > 0 && ATTRIBUTES.iter().any(|a| a.eq_ignore_ascii_case(name)) {
                    return part.to_owned();
                }
                self.cookie(part).unwrap_or_else(|| part.to_owned())
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// An address (absolute or relative): secret-named query and fragment
    /// fields, token-like path segments and a password in it are taken out.
    /// Everything else stays as written.
    pub(super) fn url(&mut self, url: &str) -> String {
        let (rest, fragment) = match url.split_once('#') {
            Some((rest, fragment)) => (rest, Some(fragment)),
            None => (url, None),
        };
        let (base, query) = match rest.split_once('?') {
            Some((base, query)) => (base, Some(query)),
            None => (rest, None),
        };
        let mut out = self.url_path(base);
        if let Some(query) = query {
            out.push('?');
            out.push_str(&self.fields(query));
        }
        if let Some(fragment) = fragment {
            out.push('#');
            out.push_str(&self.fields(fragment));
        }
        out
    }

    fn url_path(&mut self, base: &str) -> String {
        if let Some(scheme_end) = base.find("://") {
            let start = scheme_end + 3;
            let (authority, path) = match base[start..].find('/') {
                Some(slash) => base[start..].split_at(slash),
                None => (&base[start..], ""),
            };
            return format!(
                "{}{}{}",
                &base[..start],
                self.authority(authority),
                self.path(path)
            );
        }
        let mut chars = base.chars();
        let scheme = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && base.split_once(':').is_some_and(|(scheme, _)| {
                scheme
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            });
        if scheme {
            return base.to_owned();
        }
        self.path(base)
    }

    /// `user:password@host`: the password.
    fn authority(&mut self, authority: &str) -> String {
        let Some((userinfo, host)) = authority.rsplit_once('@') else {
            return authority.to_owned();
        };
        match userinfo.split_once(':') {
            Some((user, password)) if !password.is_empty() => {
                self.learn(&percent_decode_lossy(password, false), None);
                format!("{user}:{REDACTED}@{host}")
            }
            _ => authority.to_owned(),
        }
    }

    fn path(&mut self, path: &str) -> String {
        path.split('/')
            .map(|segment| {
                let decoded = percent_decode_strict(segment);
                let judged = decoded.as_deref().unwrap_or(segment);
                if looks_like_token(judged) {
                    self.learn(judged, None);
                    REDACTED.to_owned()
                } else {
                    segment.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// `name=value&…` (a query, a fragment, a form body): secret-named
    /// fields' values taken out, every other byte as it was.
    fn fields(&mut self, text: &str) -> String {
        text.split('&')
            .map(|piece| {
                if let Some((raw_name, raw_value)) = piece.split_once('=') {
                    let name = percent_decode_lossy(raw_name, true);
                    let value = percent_decode_lossy(raw_value, true);
                    if !value.is_empty() && is_secret_name(&name) {
                        self.learn(&value, Some(&name));
                        return format!("{raw_name}={REDACTED}");
                    }
                }
                piece.to_owned()
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// A body's text with what is secret in it taken out, by its MIME type
    /// (JSON, a form, multipart). `None`: it can't be checked, so it mustn't
    /// be returned.
    pub(super) fn body(&mut self, mime: &str, text: &str) -> Option<String> {
        let lower = mime.to_ascii_lowercase();
        if lower.contains("json") {
            let mut document: Value = serde_json::from_str(text).ok()?;
            self.json(&mut document, Json::Open, None);
            serde_json::to_string(&document).ok()
        } else if lower.contains("application/x-www-form-urlencoded") {
            Some(self.fields(text))
        } else if lower.contains("multipart/form-data") {
            self.multipart(mime, text)
        } else {
            None
        }
    }

    /// Walk a JSON document: everything under a secret-named key, at any
    /// depth, is taken out. `key`: the nearest key above `value`.
    fn json(&mut self, value: &mut Value, state: Json, key: Option<&str>) {
        match value {
            Value::Object(members) => {
                let kind = members
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                for (name, child) in members.iter_mut() {
                    let child_state = match state {
                        Json::Open if !is_secret_name(name) => Json::Open,
                        // DocC pages put code samples under `code`.
                        Json::Open if name == "code" => match kind.as_deref() {
                            Some("codeListing") if child.is_array() => Json::CodeListing,
                            Some("codeVoice") if child.is_string() => Json::CodeVoice,
                            _ => Json::Secret,
                        },
                        _ => Json::Secret,
                    };
                    self.json(child, child_state, Some(name));
                }
            }
            Value::Array(items) => {
                for item in items {
                    match state {
                        Json::CodeListing if item.is_string() => {}
                        Json::CodeListing => self.json(item, Json::Secret, key),
                        state => self.json(item, state, key),
                    }
                }
            }
            Value::Null => {}
            Value::String(s) if s.is_empty() => {}
            _ if state == Json::Open => {}
            Value::String(_) if state == Json::CodeVoice => {}
            Value::String(s) => {
                self.learn(s, key);
                *value = Value::String(REDACTED.into());
            }
            Value::Number(n) => {
                self.learn(&n.to_string(), key);
                *value = Value::String(REDACTED.into());
            }
            // Not learned: `true` is in every capture, and no secret.
            Value::Bool(_) => *value = Value::String(REDACTED.into()),
        }
    }

    /// A `multipart/form-data` body: secret-named parts' values taken out,
    /// every other byte as it was. `None` for anything it can't be sure of,
    /// file uploads included.
    fn multipart(&mut self, mime: &str, text: &str) -> Option<String> {
        let boundary = parameter(mime, "boundary").filter(|b| !b.is_empty())?;
        let delimiter = format!("--{boundary}");
        let pieces: Vec<&str> = text.split(delimiter.as_str()).collect();
        if pieces.len() < 3 {
            return None;
        }
        let mut out = Vec::with_capacity(pieces.len());
        out.push(pieces[0].to_owned());
        for part in &pieces[1..pieces.len() - 1] {
            let blank = part.find("\r\n\r\n")?;
            if !part.starts_with("\r\n") || !part.ends_with("\r\n") || blank < 2 {
                return None;
            }
            let disposition = part[2..blank].split("\r\n").find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-disposition")
                    .then_some(value)
            })?;
            if parameter(disposition, "filename").is_some()
                || parameter(disposition, "filename*").is_some()
            {
                return None;
            }
            let name = parameter(disposition, "name")?;
            let start = blank + 4;
            let end = part.len() - 2;
            if start < end && is_secret_name(&name) {
                self.learn(&part[start..end], Some(&name));
                out.push(format!("{}{REDACTED}\r\n", &part[..start]));
            } else {
                out.push((*part).to_owned());
            }
        }
        out.push(pieces[pieces.len() - 1].to_owned());
        Some(out.join(&delimiter))
    }

    // -- the second pass: what was learned, wherever it shows up -------------

    /// The values to look for in a text: those at least `min` characters
    /// long, and one-time values (down to [`MIN_ONE_TIME`]); each also as
    /// `encodeURIComponent` writes it. Longest first, so a value is taken
    /// out whole before a shorter one inside it is looked for.
    fn candidates(&self, min: usize) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        for (value, one_time) in &self.learned {
            let length = value.chars().count();
            if length >= min || (*one_time && length >= MIN_ONE_TIME) {
                let encoded = encode_uri_component(value);
                if encoded != *value {
                    found.push(encoded);
                }
                found.push(value.clone());
            }
        }
        found.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        found.dedup();
        found
    }

    /// `text` with every learned value of at least `min` characters (and
    /// every one-time value) replaced.
    pub(super) fn known_in_text(&self, text: &str, min: usize) -> String {
        replace_all(text, &self.candidates(min))
    }

    /// A JSON body with learned values taken out of its strings (from
    /// [`MIN_IN_JSON`] characters), and numbers that are a learned value
    /// replaced. `None`: it isn't JSON.
    pub(super) fn known_in_json(&self, text: &str) -> Option<String> {
        let mut document: Value = serde_json::from_str(text).ok()?;
        let strings = self.candidates(MIN_IN_JSON);
        let numbers = self.candidates(MIN_NUMBER);
        if strings.is_empty() && numbers.is_empty() {
            return Some(text.to_owned());
        }
        fn walk(value: &mut Value, strings: &[String], numbers: &[String]) {
            match value {
                Value::Object(members) => {
                    members.values_mut().for_each(|v| walk(v, strings, numbers))
                }
                Value::Array(items) => items.iter_mut().for_each(|v| walk(v, strings, numbers)),
                Value::String(s) => *s = replace_all(s, strings),
                Value::Number(n) => {
                    if numbers.contains(&n.to_string()) {
                        *value = Value::String(REDACTED.into());
                    }
                }
                Value::Bool(_) | Value::Null => {}
            }
        }
        walk(&mut document, &strings, &numbers);
        serde_json::to_string(&document).ok()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Json {
    /// Kept, unless a key says otherwise.
    Open,
    /// Under a secret-named key: every scalar is taken out.
    Secret,
    /// A DocC code listing: an array of lines, kept.
    CodeListing,
    /// A DocC inline code span: a string, kept.
    CodeVoice,
}

/// Every occurrence of any of `candidates` (longest first) in `text`
/// replaced, in one pass: a replacement is never looked into again.
fn replace_all(text: &str, candidates: &[String]) -> String {
    if candidates.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'scan: while !rest.is_empty() {
        for candidate in candidates {
            if rest.starts_with(candidate.as_str()) {
                out.push_str(REDACTED);
                rest = &rest[candidate.len()..];
                continue 'scan;
            }
        }
        let next = rest.chars().next().map_or(1, char::len_utf8);
        out.push_str(&rest[..next]);
        rest = &rest[next..];
    }
    out
}

/// A `; name=value` parameter of a header value (quoted or not); the name's
/// case doesn't matter.
fn parameter(header: &str, wanted: &str) -> Option<String> {
    let mut rest = header;
    loop {
        // The next `;` outside quotes.
        let mut quoted = false;
        let end = rest
            .char_indices()
            .find(|&(_, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                c == ';' && !quoted
            })
            .map(|(i, _)| i);
        let piece = &rest[..end.unwrap_or(rest.len())];
        if let Some((name, value)) = piece.split_once('=')
            && name.trim().eq_ignore_ascii_case(wanted)
        {
            let value = value.trim();
            return Some(match value.strip_prefix('"') {
                Some(inner) => inner.split('"').next().unwrap_or_default().to_owned(),
                None => value
                    .split(|c: char| c.is_whitespace() || c == ';')
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
        rest = &rest[end? + 1..];
    }
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Percent-decoded, or `None` if an escape is malformed or the bytes aren't
/// UTF-8.
fn percent_decode_strict(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_owned());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = hex(*bytes.get(i + 1)?)?;
            let low = hex(*bytes.get(i + 2)?)?;
            out.push(high << 4 | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Decoded as a form field is (`plus`: `+` is a space): a malformed escape
/// is kept as written, bytes that aren't UTF-8 are replaced.
fn percent_decode_lossy(s: &str, plus: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high << 4 | low);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' if plus => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// As JavaScript's `encodeURIComponent`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_names() {
        for secret in [
            "Authorization",
            "oauth_state",
            "x-auth-user",
            "WWW-Authenticate",
            "authorise",
            "Cookie",
            "x-csrf-token",
            "client_secret",
            "JSESSIONID",
            "sessionId",
            "password",
            "new_passwd",
            "pwd",
            "passcode",
            "passphrase",
            "otp",
            "totp",
            "otpCode",
            "mfaOtpValue",
            "TOTPSecret",
            "X-Signature",
            "jwt",
            "code",
            "auth_code",
            "code.verifier",
            "sig",
            "x-sig",
            "pin",
            "apikey",
            "api_key",
            "X-Api-Key",
            "access-key",
            "refresh.key",
            "private_key",
            "key",
        ] {
            assert!(is_secret_name(secret), "{secret}");
        }
        for harmless in [
            "author",
            "authored_by",
            "coauthors",
            ":authority",
            "footprint",
            "hotplug",
            "encoded",
            "barcode",
            "codec",
            "signal",
            "design",
            "status_code",
            "x-error-code",
            "country.code",
            "keyboard",
            "sort",
            "page",
            "state",
            "pinned",
            "content-type",
        ] {
            assert!(!is_secret_name(harmless), "{harmless}");
        }
        for one_time in [
            "password",
            "user_passphrase",
            "otp",
            "totp",
            "pwd",
            "pin",
            "code",
            "mfa_code",
            "2facode",
            "verification_code",
            "authcode",
        ] {
            assert!(is_one_time_name(one_time), "{one_time}");
        }
        for not in ["token", "auth_token", "barcode", "session", "pinned"] {
            assert!(!is_one_time_name(not), "{not}");
        }
    }

    #[test]
    fn headers() {
        let mut r = Redactor::default();
        assert_eq!(
            r.header("Authorization", "Bearer abc.def.ghi-1234567890"),
            "Bearer [redacted]"
        );
        assert!(r.knows("abc.def.ghi-1234567890"));
        assert_eq!(r.header("authorization", "opaque"), REDACTED);
        assert_eq!(
            r.header("Cookie", "sid=s3cr3tvalue; theme=dark"),
            "sid=[redacted]; theme=[redacted]"
        );
        assert!(r.knows("s3cr3tvalue") && r.knows("dark"));
        assert_eq!(
            r.header("Set-Cookie", "sid=s3cr3tvalue; Path=/; HttpOnly\nother=1"),
            "sid=[redacted]; Path=/; HttpOnly\nother=[redacted]"
        );
        assert_eq!(
            r.header(
                "set-cookie",
                "a=1; Domain=x.test; Max-Age=60; SameSite=Lax; extra=zz"
            ),
            "a=[redacted]; Domain=x.test; Max-Age=60; SameSite=Lax; extra=[redacted]"
        );
        assert_eq!(r.header("X-Api-Key", "k-123"), REDACTED);
        assert_eq!(r.header("X-Token", "short"), REDACTED);
        assert_eq!(
            r.header("Content-Type", "application/json"),
            "application/json"
        );
        assert_eq!(r.header("Access-Control-Allow-Credentials", "true"), "true");
        assert_eq!(r.header(":authority", "x.test"), "x.test");
        assert_eq!(r.header("X-Auth-User", ""), "");
        assert_eq!(
            r.header("Location", "https://x.test/cb?code=abc&state=ok"),
            "https://x.test/cb?code=[redacted]&state=ok"
        );
        assert_eq!(r.header(":path", "/cb?token=abc"), "/cb?token=[redacted]");
    }

    #[test]
    fn urls() {
        let mut r = Redactor::default();
        assert_eq!(
            r.url("https://api.test/v1/items?access_token=xyz&page=2#id_token=abc&tab=1"),
            "https://api.test/v1/items?access_token=[redacted]&page=2#id_token=[redacted]&tab=1"
        );
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl";
        assert_eq!(
            r.url(&format!("https://x.test/reset/{jwt}/done")),
            "https://x.test/reset/[redacted]/done"
        );
        assert!(r.knows(jwt));
        for unchanged in [
            "https://x.test/docs/how-to-install-the-extension-now",
            "https://x.test/u/123e4567-e89b-12d3-a456-426614174000",
            "about:blank",
            "data:text/plain,token=abc",
            "https://x.test",
            "https://x.test/a?flag&b=%7e+c",
            "https://x.test/files/${id}abcdefghijklmnopqrstuvwxyz0123",
        ] {
            assert_eq!(r.url(unchanged), unchanged);
        }
        assert_eq!(r.url("/relative?session=1"), "/relative?session=[redacted]");
        let hex = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            r.url(&format!("https://x.test/v/{hex}")),
            "https://x.test/v/[redacted]"
        );
        assert_eq!(
            r.url("https://x.test/k/Ab3dEf9hIjKlMn0pQrStUvWx"),
            "https://x.test/k/[redacted]"
        );
        // Decoded before it is judged, and learned decoded.
        assert_eq!(
            r.url("https://x.test/k/Ab3dEf9hIjKlMn0pQrStUv%2BWx?q=a%20b"),
            "https://x.test/k/[redacted]?q=a%20b"
        );
        assert!(r.knows("Ab3dEf9hIjKlMn0pQrStUv+Wx"));
        // Fields are decoded to be judged; the value learned decoded.
        assert_eq!(
            r.url("https://x.test/?api%5Fkey=a+b%21"),
            "https://x.test/?api%5Fkey=[redacted]"
        );
        assert!(r.knows("a b!"));
        assert_eq!(
            r.url("https://ada:pa55word@x.test/p"),
            "https://ada:[redacted]@x.test/p"
        );
        assert!(r.knows("pa55word"));
    }

    #[test]
    fn token_shapes() {
        assert!(!looks_like_token("short"));
        assert!(!looks_like_token("123e4567-e89b-12d3-a456-426614174000"));
        assert!(!looks_like_token("how-to-install-the-extension-now"));
        assert!(!looks_like_token("abcdefghijklmnopqrstuvwxyz"));
        assert!(looks_like_token(
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln"
        ));
        assert!(looks_like_token("0123456789abcdef0123456789abcdef"));
        assert!(looks_like_token("tok_live_1234567890ABCDEF"));
        assert!(looks_like_token("aGVsbG8gd29ybGQgdGhpcyBpcyAx=="));
        assert!(!looks_like_token("aGVsbG8gd29ybGQgdGhpcyBpcyAx==="));
        assert!(!looks_like_token("has spaces in it 1234567890 AB"));
        // Lower case only and shorter than 32: an identifier more likely.
        assert!(!looks_like_token("abcdefghijklm1234567890x"));
    }

    fn json_body(r: &mut Redactor, text: &str) -> Value {
        serde_json::from_str(&r.body("application/json", text).unwrap()).unwrap()
    }

    #[test]
    fn bodies() {
        let mut r = Redactor::default();
        assert_eq!(
            json_body(
                &mut r,
                r#"{"user":"dax","password":"hunter2","nested":{"refresh_token":"r-1"},"n":1}"#
            ),
            json!({"user":"dax","password":"[redacted]","nested":{"refresh_token":"[redacted]"},"n":1})
        );
        assert_eq!(
            r.body(
                "application/x-www-form-urlencoded",
                "user=dax&password=hunter2"
            )
            .unwrap(),
            "user=dax&password=[redacted]"
        );
        let multipart = "--b\r\nContent-Disposition: form-data; name=\"otp\"\r\n\r\n123456\r\n\
                         --b\r\nContent-Disposition: form-data; name=\"q\"\r\n\r\nhi\r\n--b--\r\n";
        assert_eq!(
            r.body("multipart/form-data; boundary=b", multipart)
                .unwrap(),
            multipart.replace("123456", REDACTED)
        );
        assert_eq!(
            r.body("Multipart/Form-Data; BOUNDARY=\"b\"", multipart)
                .unwrap(),
            multipart.replace("123456", REDACTED)
        );
        let upload = "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a.txt\"\r\n\r\nx\r\n--b--";
        assert_eq!(r.body("multipart/form-data; boundary=b", upload), None);
        assert_eq!(r.body("multipart/form-data", multipart), None);
        assert_eq!(r.body("multipart/form-data; boundary=b", "--b--"), None);
        assert_eq!(r.body("application/json", "not json"), None);
        assert_eq!(r.body("text/html", "<html>"), None);

        for unchanged in [
            r#"{"type":"codeListing","code":["let x = 1"]}"#,
            r#"{"type":"codeVoice","code":"x"}"#,
            r#"{"ok":true,"list":[1,"a",null],"token":""}"#,
        ] {
            assert_eq!(
                json_body(&mut r, unchanged),
                serde_json::from_str::<Value>(unchanged).unwrap()
            );
        }
        assert_eq!(
            json_body(&mut r, r#"{"code":"123"}"#),
            json!({"code":"[redacted]"})
        );
        assert_eq!(
            json_body(&mut r, r#"{"session":{"id":5,"live":true}}"#),
            json!({"session":{"id":"[redacted]","live":"[redacted]"}})
        );
        assert_eq!(
            json_body(&mut r, r#"{"type":"codeListing","code":["a",{"b":1}]}"#),
            json!({"type":"codeListing","code":["a",{"b":"[redacted]"}]})
        );
        assert_eq!(
            json_body(&mut r, r#"{"otp_codes":["1234","5678"]}"#),
            json!({"otp_codes":["[redacted]","[redacted]"]})
        );
        // Key order is kept.
        assert_eq!(
            r.body("application/json", r#"{"z":1,"a":2}"#).unwrap(),
            r#"{"z":1,"a":2}"#
        );
    }

    #[test]
    fn known_values_elsewhere() {
        let mut r = Redactor::default();
        r.header("Authorization", "Bearer longsecretvalue123");
        r.body("application/json", r#"{"otp":"4821"}"#).unwrap();
        assert_eq!(
            r.known_in_text("echo longsecretvalue123 and 4821", MIN_IN_TEXT),
            "echo [redacted] and [redacted]"
        );
        let mut r = Redactor::default();
        r.header("X-Token", "short");
        assert_eq!(r.known_in_text("a short word", MIN_IN_TEXT), "a short word");

        // Encoded as an address would have it.
        let mut r = Redactor::default();
        r.header("X-Api-Key", "a b/c+d=efgh");
        assert_eq!(
            r.known_in_text("?next=a%20b%2Fc%2Bd%3Defgh", MIN_IN_TEXT),
            "?next=[redacted]"
        );
        // A one-time value of three characters is too short to look for.
        let mut r = Redactor::default();
        r.body("application/json", r#"{"pin":"123"}"#).unwrap();
        assert_eq!(r.known_in_text("x=123", MIN_IN_TEXT), "x=123");

        // JSON: strings from 16 characters, numbers that are a learned value.
        let mut r = Redactor::default();
        r.header("X-Api-Key", "0123456789abcdefXYZ");
        r.body("application/json", r#"{"session":{"id":5,"n":987654}}"#)
            .unwrap();
        assert_eq!(
            r.known_in_json(r#"{"a":"key 0123456789abcdefXYZ","n":5,"m":987654,"t":true}"#)
                .unwrap(),
            r#"{"a":"key [redacted]","n":5,"m":"[redacted]","t":true}"#
        );
        assert_eq!(r.known_in_json("{nope"), None);
    }

    #[test]
    fn replacements_are_not_looked_into_again() {
        let mut r = Redactor::default();
        r.header("X-Api-Key", "abcdefgh-redacted");
        r.header("X-Other-Key", "redacted]xyz");
        assert_eq!(
            r.known_in_text("abcdefgh-redacted]xyz", MIN_IN_TEXT),
            "[redacted]]xyz"
        );
    }

    #[test]
    fn decoding() {
        assert_eq!(percent_decode_strict("a%20b").as_deref(), Some("a b"));
        assert_eq!(percent_decode_strict("a%2"), None);
        assert_eq!(percent_decode_strict("%ff"), None);
        assert_eq!(percent_decode_lossy("a+b%2", true), "a b%2");
        assert_eq!(percent_decode_lossy("%zz%41", true), "%zzA");
        assert_eq!(encode_uri_component("a b~"), "a%20b~");
        assert_eq!(
            parameter("form-data; name=\"a;b\"; x=1", "x").as_deref(),
            Some("1")
        );
        assert_eq!(
            parameter("form-data; name=\"a;b\"", "name").as_deref(),
            Some("a;b")
        );
        assert_eq!(parameter("form-data; name=q", "filename"), None);
    }
}

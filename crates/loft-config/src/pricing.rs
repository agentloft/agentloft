//! API list prices, to estimate what a turn would cost at API rates when
//! its agent doesn't say (Codex, most ACP agents). USD per million tokens.
//!
//! Where a price comes from, first to last: `[pricing."<model>"]` in
//! config.toml; the catalog the daemon fetches from models.dev each day
//! ([`Catalog`], `[usage] price_catalog`); the table below (Anthropic's and
//! OpenAI's list prices as of November 2026), for when there's no catalog.

use crate::{ModelPrice, Paths};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// `(model id prefix, input, cache read, output)`; cache writes cost 1.25×
/// input. The longest matching prefix wins.
const PRICES: &[(&str, f64, f64, f64)] = &[
    // Anthropic.
    ("claude-fable-5-2", 8.0, 0.20, 40.0),
    ("claude-mythos-5-2", 8.0, 0.20, 40.0),
    ("claude-fable-5", 10.0, 1.0, 50.0),
    ("claude-mythos-5", 10.0, 1.0, 50.0),
    ("claude-opus-5-5", 4.0, 0.20, 20.0),
    ("claude-opus-5", 5.0, 0.50, 25.0),
    ("claude-opus-4", 5.0, 0.50, 25.0),
    ("claude-sonnet-5-5", 2.0, 0.10, 10.0),
    ("claude-sonnet-5", 2.0, 0.20, 10.0),
    ("claude-sonnet-4", 3.0, 0.30, 15.0),
    ("claude-haiku-5-5", 0.10, 0.01, 0.50),
    ("claude-haiku-4", 1.0, 0.10, 5.0),
    // OpenAI.
    ("gpt-6.1-sol", 2.0, 0.10, 10.0),
    ("gpt-6.1-terra", 1.0, 0.05, 5.0),
    ("gpt-6-astra", 10.0, 1.0, 50.0),
    ("gpt-6-sol", 2.0, 0.20, 10.0),
    ("gpt-6-luna", 0.10, 0.01, 0.50),
    ("gpt-5.6-sol", 4.0, 0.40, 20.0),
    ("gpt-5.6-terra", 2.0, 0.20, 12.0),
    ("gpt-5.6-luna", 0.20, 0.02, 1.20),
    ("gpt-5.5-pro", 30.0, 30.0, 180.0),
    ("gpt-5.5", 5.0, 0.50, 30.0),
    ("gpt-5.4-pro", 30.0, 30.0, 180.0),
    ("gpt-5.4-mini", 0.75, 0.075, 4.50),
    ("gpt-5.4-nano", 0.20, 0.02, 1.25),
    ("gpt-5.4", 2.50, 0.25, 15.0),
    ("gpt-5.3", 1.75, 0.175, 14.0),
    ("gpt-5.2", 1.75, 0.175, 14.0),
    ("gpt-5.1", 1.25, 0.125, 10.0),
    ("gpt-5-mini", 0.25, 0.025, 2.0),
    ("gpt-5-nano", 0.05, 0.005, 0.40),
    ("gpt-5", 1.25, 0.125, 10.0),
];

/// A price per token kind, USD per million.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl Price {
    fn from_config(p: &ModelPrice) -> Self {
        Price {
            input: p.input,
            cache_read: p.cache_read.unwrap_or(p.input * 0.1),
            cache_write: p.cache_write.unwrap_or(p.input * 1.25),
            output: p.output,
        }
    }
}

/// The model id a name stands for: Claude Code's aliases, without a
/// context-size suffix (`[1m]`) or a provider prefix (`openai/`).
pub fn canonical(model: &str) -> String {
    let m = model.trim().to_ascii_lowercase();
    let m = m.split('[').next().unwrap_or(&m).trim();
    let m = m.rsplit('/').next().unwrap_or(m);
    match m {
        "opus" | "opusplan" => "claude-opus-5-5".into(),
        "fable" => "claude-fable-5-2".into(),
        "sonnet" => "claude-sonnet-5-5".into(),
        "haiku" => "claude-haiku-5-5".into(),
        "mythos" => "claude-mythos-5-2".into(),
        other => other.to_owned(),
    }
}

/// Where the daemon keeps the fetched catalog.
pub fn catalog_file(paths: &Paths) -> PathBuf {
    paths.home.join("cache").join("model-prices.json")
}

/// Model prices from a catalog (models.dev), by model id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    /// When it was fetched (ms since the epoch).
    pub fetched_at_ms: i64,
    pub models: BTreeMap<String, CatalogPrice>,
}

/// A model's price, and dearer ones past a context size (long-context
/// pricing), smallest size first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogPrice {
    pub price: Price,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub above: Vec<(u64, Price)>,
}

/// Providers whose prices win when several list the same model id: the
/// model makers' own, then the rest in name order.
const FIRST_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "xai",
    "mistral",
    "deepseek",
];

impl Catalog {
    /// From models.dev's `api.json`: `{provider: {models: {id: {cost}}}}`
    /// with `cost` in USD per million tokens.
    pub fn from_models_dev(json: &Value, fetched_at_ms: i64) -> Self {
        fn price(c: &Value) -> Option<Price> {
            let n = |k: &str| c.get(k).and_then(Value::as_f64);
            let input = n("input")?;
            Some(Price {
                input,
                output: n("output")?,
                cache_read: n("cache_read").unwrap_or(input * 0.1),
                // No price listed: writing to the cache costs nothing extra.
                cache_write: n("cache_write").unwrap_or(input),
            })
        }
        let Some(providers) = json.as_object() else {
            return Self::default();
        };
        let mut order: Vec<&String> = providers.keys().collect();
        order.sort_by_key(|p| {
            (
                FIRST_PROVIDERS
                    .iter()
                    .position(|f| f == p)
                    .unwrap_or(usize::MAX),
                p.as_str(),
            )
        });
        let mut models = BTreeMap::new();
        for provider in order {
            let Some(list) = providers[provider].get("models").and_then(Value::as_object) else {
                continue;
            };
            for (id, m) in list {
                let Some(cost) = m.get("cost") else { continue };
                let Some(base) = price(cost) else { continue };
                // Free and local models say nothing about spend.
                if base.input <= 0. && base.output <= 0. {
                    continue;
                }
                let mut above: Vec<(u64, Price)> = cost
                    .get("tiers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|t| {
                        let size = t.pointer("/tier/size")?.as_u64()?;
                        Some((size, price(t)?))
                    })
                    .collect();
                above.sort_by_key(|(size, _)| *size);
                models
                    .entry(canonical(id))
                    .or_insert(CatalogPrice { price: base, above });
            }
        }
        Self {
            fetched_at_ms,
            models,
        }
    }

    /// `id`'s price for a request holding `context` tokens: its own entry,
    /// or one without a date suffix (`-20251101`).
    fn price(&self, id: &str, context: Option<u64>) -> Option<Price> {
        let undated = id
            .rsplit_once('-')
            .filter(|(_, d)| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
            .map(|(base, _)| base);
        let entry = self
            .models
            .get(id)
            .or_else(|| undated.and_then(|base| self.models.get(base)))?;
        let tier = entry
            .above
            .iter()
            .rev()
            .find(|(size, _)| context.is_some_and(|c| c > *size));
        Some(tier.map_or(entry.price, |(_, p)| *p))
    }
}

/// `model`'s price for a request holding `context` tokens: the config's
/// (longest matching prefix), else the catalog's, else the built-in table's.
pub fn price(
    model: &str,
    context: Option<u64>,
    overrides: &BTreeMap<String, ModelPrice>,
    catalog: Option<&Catalog>,
) -> Option<Price> {
    let id = canonical(model);
    let own = overrides
        .iter()
        .filter(|(k, _)| id.starts_with(&canonical(k)))
        .max_by_key(|(k, _)| k.len())
        .map(|(_, p)| Price::from_config(p));
    own.or_else(|| catalog.and_then(|c| c.price(&id, context)))
        .or_else(|| {
            PRICES
                .iter()
                .filter(|(prefix, ..)| id.starts_with(prefix))
                .max_by_key(|(prefix, ..)| prefix.len())
                .map(|&(_, input, cache_read, output)| Price {
                    input,
                    cache_read,
                    cache_write: input * 1.25,
                    output,
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve_to_prices() {
        let none = BTreeMap::new();
        let opus = price("opus", None, &none, None).unwrap();
        assert_eq!(
            (opus.input, opus.output, opus.cache_read),
            (4.0, 20.0, 0.20)
        );
        assert_eq!(price("claude-opus-5-5[1m]", None, &none, None), Some(opus));
        // The longest prefix wins: Opus 5.5 isn't priced as Opus 5.
        assert_eq!(
            price("claude-opus-5", None, &none, None).unwrap().input,
            5.0
        );
        assert_eq!(
            price("gpt-5.4-mini", None, &none, None).unwrap().input,
            0.75
        );
        assert_eq!(
            price("openai/gpt-5.5", None, &none, None).unwrap().output,
            30.0
        );
        assert_eq!(price("some-local-model", None, &none, None), None);
        // The config's own prices come first.
        let mine = BTreeMap::from([(
            "gpt-5.5".to_owned(),
            ModelPrice {
                input: 1.0,
                output: 2.0,
                ..Default::default()
            },
        )]);
        let p = price("gpt-5.5", None, &mine, None).unwrap();
        assert_eq!((p.input, p.cache_read, p.output), (1.0, 0.1, 2.0));
    }

    #[test]
    fn models_dev_prices_win_over_the_table() {
        let json: Value = serde_json::from_str(
            r#"{
              "zz-reseller": {"models": {"gpt-5.5": {"cost": {"input": 99, "output": 99}}}},
              "openai": {"models": {
                "gpt-5.5": {"cost": {"input": 5, "output": 30, "cache_read": 0.5,
                  "tiers": [{"input": 10, "output": 45, "cache_read": 1, "tier": {"type": "context", "size": 272000}}]}},
                "gpt-oss-local": {"cost": {"input": 0, "output": 0}}
              }},
              "anthropic": {"models": {
                "claude-opus-5-5": {"cost": {"input": 4.2, "output": 21, "cache_read": 0.21, "cache_write": 5.25}}
              }}
            }"#,
        )
        .unwrap();
        let c = Catalog::from_models_dev(&json, 1);
        // The maker's price, not a reseller's; free models left out.
        assert_eq!(c.models.len(), 2);
        let none = BTreeMap::new();
        let gpt = price("gpt-5.5", Some(1_000), &none, Some(&c)).unwrap();
        assert_eq!((gpt.input, gpt.cache_write), (5.0, 5.0));
        // Past 272K tokens of context, the long-context price.
        assert_eq!(
            price("gpt-5.5", Some(300_000), &none, Some(&c))
                .unwrap()
                .input,
            10.0
        );
        // The catalog over the table; date suffixes and aliases resolve.
        assert_eq!(price("opus", None, &none, Some(&c)).unwrap().input, 4.2);
        assert_eq!(
            price("claude-opus-5-5-20261001", None, &none, Some(&c))
                .unwrap()
                .input,
            4.2
        );
        // Not in the catalog: the table.
        assert_eq!(price("gpt-5.4", None, &none, Some(&c)).unwrap().input, 2.5);
        // Round trip through the cache file.
        let back: Catalog = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }
}

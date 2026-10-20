//! Per-token prices. A snapshot of LiteLLM's Claude entries is compiled in;
//! `claude-monitor pricing update` writes a fresher copy next to the database.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const SNAPSHOT: &str = include_str!("pricing.json");
const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// USD per token.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub cache_read: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub cache_5m: i64,
    pub cache_1h: i64,
    pub cache_read: i64,
}

impl Tokens {
    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_5m += o.cache_5m;
        self.cache_1h += o.cache_1h;
        self.cache_read += o.cache_read;
    }
    pub fn cache_write(&self) -> i64 {
        self.cache_5m + self.cache_1h
    }
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_write() + self.cache_read
    }
    /// Share of prompt tokens served from cache.
    pub fn cache_hit_rate(&self) -> f64 {
        let prompt = self.input + self.cache_write() + self.cache_read;
        if prompt == 0 { 0.0 } else { self.cache_read as f64 / prompt as f64 }
    }
}

impl Price {
    // Anthropic's published multipliers, used when an entry lacks cache prices.
    fn write_5m(&self) -> f64 {
        self.cache_write_5m.unwrap_or(self.input * 1.25)
    }
    fn write_1h(&self) -> f64 {
        self.cache_write_1h.unwrap_or(self.input * 2.0)
    }
    fn read(&self) -> f64 {
        self.cache_read.unwrap_or(self.input * 0.1)
    }
    pub fn cost(&self, t: &Tokens) -> f64 {
        t.input as f64 * self.input
            + t.output as f64 * self.output
            + t.cache_5m as f64 * self.write_5m()
            + t.cache_1h as f64 * self.write_1h()
            + t.cache_read as f64 * self.read()
    }
    /// What cache reads would have cost as plain input, minus what they did cost.
    pub fn cache_savings(&self, t: &Tokens) -> f64 {
        t.cache_read as f64 * (self.input - self.read())
    }
}

pub struct Pricing {
    table: HashMap<String, Price>,
}

impl Pricing {
    /// The override file if present and valid, else the compiled-in snapshot.
    pub fn load(override_path: &Path) -> Self {
        let table = std::fs::read_to_string(override_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::from_str(SNAPSHOT).expect("bundled pricing.json is valid"));
        Self { table }
    }

    /// Exact id, then the id without a date suffix, then the newest known model of
    /// the same family. `None` means the model is unpriced.
    pub fn get(&self, model: &str) -> Option<Price> {
        if model.is_empty() || model == "<synthetic>" {
            return Some(Price::default());
        }
        if let Some(p) = self.table.get(model) {
            return Some(*p);
        }
        let undated = model.trim_end_matches(|c: char| c.is_ascii_digit()).trim_end_matches('-');
        if let Some(p) = self.table.get(undated) {
            return Some(*p);
        }
        let family = ["opus", "sonnet", "haiku", "fable"].into_iter().find(|f| model.contains(f))?;
        self.table
            .iter()
            .filter(|(k, _)| k.contains(family))
            .max_by(|a, b| version_key(a.0).cmp(&version_key(b.0)))
            .map(|(_, p)| *p)
    }

    pub fn cost(&self, model: &str, t: &Tokens) -> f64 {
        self.get(model).map_or(0.0, |p| p.cost(t))
    }

    pub fn savings(&self, model: &str, t: &Tokens) -> f64 {
        self.get(model).map_or(0.0, |p| p.cache_savings(t))
    }
}

/// Numeric parts of a model id, for picking the newest in a family.
fn version_key(id: &str) -> Vec<u32> {
    id.split('-').filter_map(|p| p.parse().ok()).filter(|n| *n < 1000).collect()
}

/// Download LiteLLM's table and keep only first-party Claude ids.
pub fn update(dest: &Path) -> Result<usize> {
    let text = ureq::get(LITELLM_URL)
        .call()
        .context("fetching LiteLLM prices")?
        .body_mut()
        .with_config()
        .limit(50 * 1024 * 1024)
        .read_to_string()?;
    let body: serde_json::Value = serde_json::from_str(&text)?;
    let obj = body.as_object().context("unexpected LiteLLM format")?;
    let mut table = std::collections::BTreeMap::new();
    for (k, v) in obj {
        let first_party = k.starts_with("claude-") && !k.contains('/') && !k.contains('@') && !k.contains(':');
        let (Some(input), Some(output)) = (
            v.get("input_cost_per_token").and_then(|x| x.as_f64()),
            v.get("output_cost_per_token").and_then(|x| x.as_f64()),
        ) else {
            continue;
        };
        if first_party {
            let f = |name| v.get(name).and_then(|x| x.as_f64());
            table.insert(k.clone(), Price {
                input,
                output,
                cache_write_5m: f("cache_creation_input_token_cost"),
                cache_write_1h: f("cache_creation_input_token_cost_above_1hr"),
                cache_read: f("cache_read_input_token_cost"),
            });
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, serde_json::to_string_pretty(&table)?)?;
    Ok(table.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing() -> Pricing {
        Pricing::load(Path::new("/nonexistent"))
    }

    #[test]
    fn lookups() {
        let p = pricing();
        assert_eq!(p.get("claude-opus-5-5").unwrap().input, 0.000004);
        assert_eq!(p.get("claude-sonnet-4-5-20250929").unwrap().input, 0.000003);
        assert_eq!(p.get("claude-haiku-4-5-20991231").unwrap().input, 0.000001);
        assert_eq!(p.get("<synthetic>").unwrap(), Price::default());
        assert!(p.get("gpt-4").is_none());
        // Unknown member of a known family falls back to that family.
        assert!(p.get("claude-opus-9-9").is_some());
    }

    #[test]
    fn cost_and_savings() {
        let price = Price { input: 1.0, output: 2.0, cache_write_5m: None, cache_write_1h: None, cache_read: None };
        let t = Tokens { input: 1, output: 1, cache_5m: 4, cache_1h: 1, cache_read: 10 };
        assert!((price.cost(&t) - (1.0 + 2.0 + 5.0 + 2.0 + 1.0)).abs() < 1e-9);
        assert!((price.cache_savings(&t) - 9.0).abs() < 1e-9);
        assert!((t.cache_hit_rate() - 10.0 / 16.0).abs() < 1e-9);
    }
}

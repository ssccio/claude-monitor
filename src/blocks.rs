//! Five-hour session blocks, burn rate and plan limits, following the upstream
//! Python tool's rules.

use serde::Serialize;

use crate::pricing::{Pricing, Tokens};
use crate::stats::Row;

pub const BLOCK_MS: i64 = 5 * 3_600_000;
const HOUR_MS: i64 = 3_600_000;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Block {
    pub start_ms: i64,
    pub end_ms: i64,
    pub last_ms: i64,
    pub active: bool,
    pub requests: u64,
    /// Input + output, the figure compared against plan limits.
    pub limit_tokens: i64,
    pub cost_usd: f64,
    pub models: Vec<(String, i64)>,
    #[serde(skip)]
    pub tokens: Tokens,
}

/// Group time-ordered rows into blocks. A block starts at the UTC hour of its first
/// request and lasts five hours; a request after the block ends, or after five
/// idle hours, starts a new one.
pub fn build(rows: &[Row], now_ms: i64, pricing: &Pricing) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for r in rows {
        let new = match blocks.last() {
            None => true,
            Some(b) => r.ts >= b.end_ms || r.ts - b.last_ms >= BLOCK_MS,
        };
        if new {
            let start = r.ts - r.ts.rem_euclid(HOUR_MS);
            blocks.push(Block { start_ms: start, end_ms: start + BLOCK_MS, ..Default::default() });
        }
        let b = blocks.last_mut().expect("pushed above");
        b.last_ms = r.ts;
        b.requests += 1;
        b.limit_tokens += r.limit_tokens();
        b.tokens.add(&r.tokens);
        b.cost_usd += pricing.cost(&r.model, &r.tokens);
        let used = r.limit_tokens();
        match b.models.iter_mut().find(|(m, _)| *m == r.model) {
            Some((_, n)) => *n += used,
            None => b.models.push((r.model.clone(), used)),
        }
    }
    for b in &mut blocks {
        b.active = b.end_ms > now_ms;
        b.models.retain(|(m, _)| m != "<synthetic>");
        b.models.sort_by_key(|m| std::cmp::Reverse(m.1));
    }
    blocks
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
pub enum Plan {
    Pro,
    Max5,
    Max20,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Limits {
    pub tokens: i64,
    pub cost_usd: f64,
    pub messages: u64,
}

const COMMON_TOKEN_LIMITS: [i64; 4] = [19_000, 88_000, 220_000, 880_000];
const LIMIT_HIT_THRESHOLD: f64 = 0.95;
const DEFAULT_TOKEN_LIMIT: i64 = 19_000;

impl Plan {
    fn fixed(self) -> Limits {
        match self {
            Plan::Pro => Limits { tokens: 19_000, cost_usd: 18.0, messages: 250 },
            Plan::Max5 => Limits { tokens: 88_000, cost_usd: 35.0, messages: 1_000 },
            Plan::Max20 => Limits { tokens: 220_000, cost_usd: 140.0, messages: 2_000 },
            Plan::Custom => Limits { tokens: 44_000, cost_usd: 50.0, messages: 250 },
        }
    }

    /// Fixed plans use the published-by-upstream figures. Custom derives the token
    /// limit from the 90th percentile of past blocks, and cost and message limits the
    /// same way, falling back to the custom defaults with no history.
    pub fn limits(self, blocks: &[Block], custom_tokens: Option<i64>) -> Limits {
        if self != Plan::Custom {
            return self.fixed();
        }
        let done: Vec<&Block> = blocks.iter().filter(|b| !b.active).collect();
        let tokens = custom_tokens.unwrap_or_else(|| p90_tokens(&done));
        let defaults = self.fixed();
        if done.is_empty() {
            return Limits { tokens, ..defaults };
        }
        let costs: Vec<f64> = done.iter().map(|b| b.cost_usd).collect();
        let msgs: Vec<f64> = done.iter().map(|b| b.requests as f64).collect();
        Limits {
            tokens,
            cost_usd: percentile(&costs, 90.0).max(defaults.cost_usd),
            messages: (percentile(&msgs, 90.0) as u64).max(defaults.messages),
        }
    }
}

/// Upstream's P90: use completed blocks that came within 5% of a known plan limit,
/// or every completed block if none did, and never go below the Pro limit.
fn p90_tokens(done: &[&Block]) -> i64 {
    let near_limit = |t: i64| COMMON_TOKEN_LIMITS.iter().any(|l| t as f64 >= *l as f64 * LIMIT_HIT_THRESHOLD);
    let mut hits: Vec<f64> = done.iter().map(|b| b.limit_tokens).filter(|t| near_limit(*t)).map(|t| t as f64).collect();
    if hits.is_empty() {
        hits = done.iter().map(|b| b.limit_tokens as f64).collect();
    }
    if hits.is_empty() {
        return DEFAULT_TOKEN_LIMIT;
    }
    (quantile_exclusive_p90(&mut hits) as i64).max(DEFAULT_TOKEN_LIMIT)
}

/// Python's `statistics.quantiles(data, n=10)[8]` (exclusive method).
fn quantile_exclusive_p90(data: &mut [f64]) -> f64 {
    data.sort_by(f64::total_cmp);
    let ld = data.len() as i64;
    if ld == 1 {
        return data[0];
    }
    let (n, i) = (10i64, 9i64);
    let m = ld + 1;
    let j = (i * m / n).clamp(1, ld - 1);
    let delta = (i * m - j * n) as f64;
    (data[(j - 1) as usize] * (n as f64 - delta) + data[j as usize] * delta) / n as f64
}

/// numpy's default (linear) percentile.
fn percentile(data: &[f64], p: f64) -> f64 {
    let mut v = data.to_vec();
    v.sort_by(f64::total_cmp);
    let rank = p / 100.0 * (v.len() - 1) as f64;
    let (lo, hi) = (rank.floor() as usize, rank.ceil() as usize);
    v[lo] + (v[hi] - v[lo]) * (rank - lo as f64)
}

/// Limit tokens per minute over the last hour.
pub fn hourly_burn_rate(rows: &[Row], now_ms: i64) -> f64 {
    let since = now_ms - HOUR_MS;
    let used: i64 = rows.iter().rev().take_while(|r| r.ts >= since).map(Row::limit_tokens).sum();
    used as f64 / 60.0
}

/// Cost per hour since the block started.
pub fn cost_rate(b: &Block, now_ms: i64) -> f64 {
    let minutes = ((now_ms.min(b.end_ms) - b.start_ms) as f64 / 60_000.0).max(1.0);
    b.cost_usd / minutes * 60.0
}

/// When the token limit is reached at the current burn rate, if before the block resets.
pub fn runs_out_at(b: &Block, limit: i64, burn_per_min: f64, now_ms: i64) -> Option<i64> {
    if burn_per_min <= 0.0 {
        return None;
    }
    let left = (limit - b.limit_tokens).max(0) as f64;
    let at = now_ms + (left / burn_per_min * 60_000.0) as i64;
    (at < b.end_ms).then_some(at)
}

pub fn velocity(burn_per_min: f64) -> (&'static str, &'static str) {
    match burn_per_min {
        r if r < 50.0 => ("🐌", "Slow"),
        r if r < 150.0 => ("➡️", "Normal"),
        r if r < 300.0 => ("🚀", "Fast"),
        _ => ("⚡", "Very fast"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn row(ts: i64, t: i64) -> Row {
        Row { ts, model: "claude-opus-5-5".into(), tokens: Tokens { input: t, ..Default::default() }, ..Default::default() }
    }

    #[test]
    fn block_boundaries() {
        let p = Pricing::load(Path::new("/nonexistent"));
        let h = HOUR_MS;
        let rows = [row(h + 30 * 60_000, 10), row(5 * h + 59 * 60_000, 10), row(6 * h, 10), row(12 * h, 10)];
        let blocks = build(&rows, 100 * h, &p);
        let starts: Vec<i64> = blocks.iter().map(|b| b.start_ms / h).collect();
        assert_eq!(starts, [1, 6, 12]);
        assert_eq!(blocks[0].limit_tokens, 20);
        assert!(blocks.iter().all(|b| !b.active));
        assert!(build(&rows, 13 * h, &p).last().unwrap().active);
    }

    #[test]
    fn p90_matches_python() {
        // statistics.quantiles([1..=10], n=10)[8] == 9.9
        let mut d: Vec<f64> = (1..=10).map(f64::from).collect();
        assert!((quantile_exclusive_p90(&mut d) - 9.9).abs() < 1e-9);
        // statistics.quantiles([5, 1], n=10)[8] == 7.8 (the exclusive method extrapolates)
        assert!((quantile_exclusive_p90(&mut [5.0, 1.0]) - 7.8).abs() < 1e-9);
        // numpy.percentile([1, 2, 3, 4], 90) == 3.7
        assert!((percentile(&[4.0, 1.0, 3.0, 2.0], 90.0) - 3.7).abs() < 1e-9);
    }

    #[test]
    fn custom_limit_floor_and_hits() {
        let mk = |t| Block { limit_tokens: t, ..Default::default() };
        assert_eq!(p90_tokens(&[]), DEFAULT_TOKEN_LIMIT);
        assert_eq!(p90_tokens(&[&mk(100), &mk(200)]), DEFAULT_TOKEN_LIMIT);
        // Only the block near the Max5 limit counts.
        assert_eq!(p90_tokens(&[&mk(100), &mk(90_000)]), 90_000);
    }

    #[test]
    fn runs_out() {
        let b = Block { start_ms: 0, end_ms: BLOCK_MS, limit_tokens: 900, ..Default::default() };
        assert_eq!(runs_out_at(&b, 1000, 10.0, 0), Some(600_000));
        assert_eq!(runs_out_at(&b, 1_000_000, 10.0, 0), None);
        assert_eq!(velocity(10.0).1, "Slow");
        assert_eq!(velocity(500.0).1, "Very fast");
    }
}

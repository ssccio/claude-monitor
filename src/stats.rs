//! Loading filtered rows from the store and grouping them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, ToSql};
use serde::Serialize;

use crate::pricing::{Pricing, Tokens};

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub profiles: Vec<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub project: Option<String>,
}

impl Filter {
    /// SQL `WHERE` clause and its parameters. `ts`, `profile` and `cwd` exist on
    /// every table this is used against.
    fn clause(&self) -> (String, Vec<Box<dyn ToSql>>) {
        let mut parts = vec!["1=1".to_string()];
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();
        if !self.profiles.is_empty() {
            parts.push(format!("profile IN ({})", vec!["?"; self.profiles.len()].join(",")));
            params.extend(self.profiles.iter().map(|p| Box::new(p.clone()) as Box<dyn ToSql>));
        }
        if let Some(s) = self.since_ms {
            parts.push("ts >= ?".into());
            params.push(Box::new(s));
        }
        if let Some(u) = self.until_ms {
            parts.push("ts < ?".into());
            params.push(Box::new(u));
        }
        if let Some(p) = &self.project {
            parts.push("cwd LIKE ?".into());
            params.push(Box::new(format!("%{p}%")));
        }
        (parts.join(" AND "), params)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Row {
    pub ts: i64,
    pub profile: String,
    pub session_id: String,
    pub cwd: String,
    pub branch: String,
    pub entrypoint: String,
    pub sidechain: bool,
    pub agent: String,
    pub skill: String,
    pub model: String,
    pub tokens: Tokens,
    pub web_search: i64,
}

impl Row {
    /// Tokens counted against plan limits. Cache reads and writes are excluded,
    /// matching the upstream Python tool.
    pub fn limit_tokens(&self) -> i64 {
        self.tokens.input + self.tokens.output
    }
}

pub fn load_rows(conn: &Connection, f: &Filter) -> Result<Vec<Row>> {
    let (clause, params) = f.clause();
    let sql = format!(
        "SELECT ts, profile, session_id, cwd, branch, entrypoint, sidechain, agent, skill, model,
                input, output, cache_5m, cache_1h, cache_read, web_search
         FROM usage WHERE {clause} ORDER BY ts"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok(Row {
            ts: r.get(0)?,
            profile: r.get(1)?,
            session_id: r.get(2)?,
            cwd: r.get(3)?,
            branch: r.get(4)?,
            entrypoint: r.get(5)?,
            sidechain: r.get(6)?,
            agent: r.get(7)?,
            skill: r.get(8)?,
            model: r.get(9)?,
            tokens: Tokens {
                input: r.get(10)?,
                output: r.get(11)?,
                cache_5m: r.get(12)?,
                cache_1h: r.get(13)?,
                cache_read: r.get(14)?,
            },
            web_search: r.get(15)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn surface(entrypoint: &str) -> &'static str {
    match entrypoint {
        "cli" => "CLI",
        "claude-desktop" => "Desktop",
        e if e.starts_with("sdk") => "SDK",
        "" => "unknown",
        _ => "other",
    }
}

pub fn short_path(p: &str) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(h.to_str()?).map(str::to_string)) {
        Some(rest) => format!("~{rest}"),
        None if p.is_empty() => "(none)".into(),
        None => p.to_string(),
    }
}

/// Grouping key for `breakdown` and the period reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Dim {
    Day,
    Week,
    Month,
    Model,
    Project,
    Branch,
    Surface,
    Profile,
    Agent,
    Skill,
}

impl Dim {
    pub fn label(self) -> &'static str {
        match self {
            Dim::Day => "Date",
            Dim::Week => "Week of",
            Dim::Month => "Month",
            Dim::Model => "Model",
            Dim::Project => "Project",
            Dim::Branch => "Project @ branch",
            Dim::Surface => "Surface",
            Dim::Profile => "Profile",
            Dim::Agent => "Agent",
            Dim::Skill => "Skill",
        }
    }

    pub fn is_period(self) -> bool {
        matches!(self, Dim::Day | Dim::Week | Dim::Month)
    }

    pub fn key(self, r: &Row, tz: &Tz) -> String {
        let local = || tz.timestamp_millis_opt(r.ts).single().unwrap_or_else(|| Utc.timestamp_millis_opt(0).unwrap().with_timezone(tz));
        match self {
            Dim::Day => local().format("%Y-%m-%d").to_string(),
            Dim::Week => {
                let d = local().date_naive();
                (d - chrono::Days::new(d.weekday().num_days_from_monday() as u64)).format("%Y-%m-%d").to_string()
            }
            Dim::Month => local().format("%Y-%m").to_string(),
            Dim::Model => r.model.clone(),
            Dim::Project => short_path(&r.cwd),
            Dim::Branch => format!("{} @ {}", short_path(&r.cwd), if r.branch.is_empty() { "-" } else { &r.branch }),
            Dim::Surface => surface(&r.entrypoint).into(),
            Dim::Profile => r.profile.clone(),
            Dim::Agent => match (r.sidechain, r.agent.as_str()) {
                (false, "") => "main".into(),
                (_, "") => "subagent".into(),
                (_, a) => a.into(),
            },
            Dim::Skill => if r.skill.is_empty() { "(none)".into() } else { r.skill.clone() },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Agg {
    pub key: String,
    pub requests: u64,
    pub sessions: usize,
    #[serde(skip)]
    session_set: BTreeSet<String>,
    pub models: BTreeSet<String>,
    pub input: i64,
    pub output: i64,
    pub cache_write: i64,
    pub cache_read: i64,
    pub total_tokens: i64,
    pub cache_hit_rate: f64,
    pub cost_usd: f64,
    pub cache_savings_usd: f64,
    pub web_searches: i64,
    #[serde(skip)]
    tokens: Tokens,
}

impl Agg {
    fn add(&mut self, r: &Row, pricing: &Pricing) {
        self.requests += 1;
        self.session_set.insert(r.session_id.clone());
        if !r.model.is_empty() && r.model != "<synthetic>" {
            self.models.insert(r.model.clone());
        }
        self.tokens.add(&r.tokens);
        self.cost_usd += pricing.cost(&r.model, &r.tokens);
        self.cache_savings_usd += pricing.savings(&r.model, &r.tokens);
        self.web_searches += r.web_search;
    }

    fn finish(mut self) -> Self {
        self.sessions = self.session_set.len();
        self.input = self.tokens.input;
        self.output = self.tokens.output;
        self.cache_write = self.tokens.cache_write();
        self.cache_read = self.tokens.cache_read;
        self.total_tokens = self.tokens.total();
        self.cache_hit_rate = self.tokens.cache_hit_rate();
        self
    }
}

/// Group rows by `dim`. Periods sort chronologically, everything else by cost.
/// The last element is the grand total.
pub fn aggregate(rows: &[Row], dim: Dim, tz: &Tz, pricing: &Pricing) -> (Vec<Agg>, Agg) {
    let mut groups: HashMap<String, Agg> = HashMap::new();
    let mut total = Agg { key: "Total".into(), ..Default::default() };
    for r in rows {
        let key = dim.key(r, tz);
        groups.entry(key.clone()).or_insert_with(|| Agg { key, ..Default::default() }).add(r, pricing);
        total.add(r, pricing);
    }
    let mut out: Vec<Agg> = groups.into_values().map(Agg::finish).collect();
    if dim.is_period() {
        out.sort_by(|a, b| a.key.cmp(&b.key));
    } else {
        out.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
    }
    (out, total.finish())
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub session_id: String,
    pub title: String,
    pub profile: String,
    pub project: String,
    pub surface: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub wall_minutes: f64,
    pub active_minutes: f64,
    pub requests: u64,
    pub subagent_requests: u64,
    pub total_tokens: i64,
    pub cost_usd: f64,
    pub compactions: i64,
    pub interrupts: i64,
}

/// Per-session rollup, newest first.
pub fn sessions(conn: &Connection, rows: &[Row], pricing: &Pricing) -> Result<Vec<Session>> {
    let mut by_id: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        by_id.entry(&r.session_id).or_default().push(r);
    }
    let mut titles = conn.prepare_cached("SELECT title FROM titles WHERE session_id = ?1")?;
    let mut events = conn.prepare_cached("SELECT kind, SUM(value) FROM events WHERE session_id = ?1 GROUP BY kind")?;
    let mut out = Vec::new();
    for (id, rs) in by_id {
        let first = rs[0];
        let last = rs[rs.len() - 1];
        let main = rs.iter().find(|r| !r.sidechain).copied().unwrap_or(first);
        let title: String = titles.query_row([id], |r| r.get(0)).unwrap_or_default();
        let mut ev: HashMap<String, i64> = HashMap::new();
        for e in events.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (k, v) = e?;
            ev.insert(k, v);
        }
        let mut tokens = Tokens::default();
        let mut cost = 0.0;
        for r in &rs {
            tokens.add(&r.tokens);
            cost += pricing.cost(&r.model, &r.tokens);
        }
        out.push(Session {
            session_id: id.to_string(),
            title,
            profile: main.profile.clone(),
            project: short_path(&main.cwd),
            surface: surface(&main.entrypoint).into(),
            start: ms_to_utc(first.ts),
            end: ms_to_utc(last.ts),
            wall_minutes: (last.ts - first.ts) as f64 / 60_000.0,
            active_minutes: *ev.get("active_ms").unwrap_or(&0) as f64 / 60_000.0,
            requests: rs.len() as u64,
            subagent_requests: rs.iter().filter(|r| r.sidechain).count() as u64,
            total_tokens: tokens.total(),
            cost_usd: cost,
            compactions: *ev.get("compaction").unwrap_or(&0),
            interrupts: *ev.get("interrupt").unwrap_or(&0),
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.start));
    Ok(out)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolStat {
    pub name: String,
    pub calls: i64,
    pub errors: i64,
    pub error_rate: f64,
    pub lines_added: i64,
    pub lines_removed: i64,
}

/// Tool calls grouped by name. MCP tools collapse to `mcp:<server>` unless `full`.
pub fn tools(conn: &Connection, f: &Filter, full: bool) -> Result<Vec<ToolStat>> {
    let (clause, params) = f.clause();
    let sql = format!(
        "SELECT t.name, COALESCE(r.is_error, 0), t.added, t.removed
         FROM tools t LEFT JOIN tool_results r ON r.id = t.id WHERE {clause}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut map: HashMap<String, ToolStat> = HashMap::new();
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
    })?;
    for row in rows {
        let (name, err, added, removed) = row?;
        let name = match name.strip_prefix("mcp__") {
            Some(rest) if !full => format!("mcp:{}", rest.split("__").next().unwrap_or(rest)),
            _ => name,
        };
        let s = map.entry(name.clone()).or_insert_with(|| ToolStat { name, ..Default::default() });
        s.calls += 1;
        s.errors += err as i64;
        s.lines_added += added;
        s.lines_removed += removed;
    }
    let mut out: Vec<ToolStat> = map
        .into_values()
        .map(|mut s| {
            s.error_rate = s.errors as f64 / s.calls as f64;
            s
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.calls));
    Ok(out)
}

/// Sum of one event kind over the filter.
pub fn event_sum(conn: &Connection, f: &Filter, kind: &str) -> Result<i64> {
    let (clause, mut params) = f.clause();
    // events has no cwd column; a project filter applies through the session.
    let clause = clause.replace(
        "cwd LIKE ?",
        "session_id IN (SELECT session_id FROM usage WHERE cwd LIKE ?)",
    );
    params.push(Box::new(kind.to_string()));
    let sql = format!("SELECT COALESCE(SUM(value), 0) FROM events WHERE {clause} AND kind = ?");
    Ok(conn.query_row(&sql, rusqlite::params_from_iter(params.iter()), |r| r.get(0))?)
}

pub fn ms_to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn row(ts: i64, model: &str, cwd: &str, input: i64) -> Row {
        Row {
            ts,
            model: model.into(),
            cwd: cwd.into(),
            session_id: format!("s{ts}"),
            tokens: Tokens { input, output: 1, cache_read: input, ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn aggregates_by_day_in_timezone() {
        let pricing = Pricing::load(Path::new("/nonexistent"));
        let tz: Tz = "America/New_York".parse().unwrap();
        // 2026-10-19T02:00Z is still the 18th in New York.
        let rows = [row(1_792_375_200_000, "claude-opus-5-5", "/a", 10), row(1_792_418_400_000, "claude-opus-5-5", "/a", 10)];
        let (groups, total) = aggregate(&rows, Dim::Day, &tz, &pricing);
        let keys: Vec<_> = groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, ["2026-10-18", "2026-10-19"]);
        assert_eq!(total.requests, 2);
        assert_eq!(total.sessions, 2);
        assert!((total.cache_hit_rate - 0.5).abs() < 1e-9);
        assert!(total.cost_usd > 0.0);
    }

    #[test]
    fn agent_dimension() {
        let tz: Tz = "UTC".parse().unwrap();
        let mut r = Row::default();
        assert_eq!(Dim::Agent.key(&r, &tz), "main");
        r.sidechain = true;
        assert_eq!(Dim::Agent.key(&r, &tz), "subagent");
        r.agent = "grunt".into();
        assert_eq!(Dim::Agent.key(&r, &tz), "grunt");
    }
}

//! Table and JSON output for the report commands.

use chrono::TimeZone;
use chrono_tz::Tz;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};
use serde::Serialize;

use crate::blocks::Block;
use crate::stats::{Agg, Dim, Session, ToolStat};

pub fn int(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

pub fn compact(n: i64) -> String {
    let f = n as f64;
    match n.abs() {
        a if a >= 1_000_000_000 => format!("{:.1}B", f / 1e9),
        a if a >= 1_000_000 => format!("{:.1}M", f / 1e6),
        a if a >= 10_000 => format!("{:.0}k", f / 1e3),
        a if a >= 1_000 => format!("{:.1}k", f / 1e3),
        _ => n.to_string(),
    }
}

pub fn usd(x: f64) -> String {
    let cents = (x * 100.0).round() as i64;
    format!("${}.{:02}", int(cents / 100), (cents % 100).abs())
}

pub fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

pub fn minutes(m: f64) -> String {
    let m = m.round() as i64;
    match (m / 60, m % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// Replace control characters in transcript-derived text so it can't drive the terminal.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { '\u{FFFD}' } else { c }).collect()
}

pub fn short_model(m: &str) -> &str {
    m.strip_prefix("claude-").unwrap_or(m)
}

pub fn local_time(ms: i64, tz: &Tz, fmt: &str) -> String {
    tz.timestamp_millis_opt(ms).single().map(|t| t.format(fmt).to_string()).unwrap_or_default()
}

fn table(header: &[&str]) -> Table {
    let mut t = Table::new();
    t.load_style(UTF8_FULL_CONDENSED).set_content_arrangement(ContentArrangement::Dynamic).set_header(header.to_vec());
    t
}

fn right(t: &mut Table, from: usize) {
    for i in from..t.column_count() {
        if let Some(c) = t.column_mut(i) {
            c.set_cell_alignment(CellAlignment::Right);
        }
    }
}

pub fn print_json<T: Serialize>(v: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

pub fn aggregates(dim: Dim, groups: &[Agg], total: &Agg) {
    let models = dim.is_period();
    let mut header = vec![dim.label()];
    if models {
        header.push("Models");
    }
    header.extend(["Requests", "Sessions", "Input", "Output", "Cache write", "Cache read", "Cache hit", "Cost", "Cache saved"]);
    let mut t = table(&header);
    let first_num = if models { 2 } else { 1 };
    for g in groups.iter().chain(std::iter::once(total)) {
        let mut row = vec![Cell::new(clean(&g.key))];
        if models {
            row.push(Cell::new(g.models.iter().map(|m| clean(short_model(m))).collect::<Vec<_>>().join("\n")));
        }
        row.extend([
            Cell::new(int(g.requests as i64)),
            Cell::new(int(g.sessions as i64)),
            Cell::new(compact(g.input)),
            Cell::new(compact(g.output)),
            Cell::new(compact(g.cache_write)),
            Cell::new(compact(g.cache_read)),
            Cell::new(pct(g.cache_hit_rate)),
            Cell::new(usd(g.cost_usd)),
            Cell::new(usd(g.cache_savings_usd)),
        ]);
        t.add_row(row);
    }
    right(&mut t, first_num);
    println!("{t}");
}

pub fn sessions(list: &[Session], tz: &Tz) {
    let mut t = table(&["Started", "Title", "Project", "Surface", "Active", "Wall", "Requests", "Subagent", "Cost", "Compact", "Interrupt"]);
    for s in list {
        t.add_row(vec![
            Cell::new(local_time(s.start.timestamp_millis(), tz, "%Y-%m-%d %H:%M")),
            Cell::new(clean(if s.title.is_empty() { s.session_id.get(..8).unwrap_or(&s.session_id) } else { &s.title })),
            Cell::new(clean(&s.project)),
            Cell::new(clean(&s.surface)),
            Cell::new(minutes(s.active_minutes)),
            Cell::new(minutes(s.wall_minutes)),
            Cell::new(int(s.requests as i64)),
            Cell::new(pct(s.subagent_requests as f64 / s.requests.max(1) as f64)),
            Cell::new(usd(s.cost_usd)),
            Cell::new(s.compactions),
            Cell::new(s.interrupts),
        ]);
    }
    right(&mut t, 4);
    println!("{t}");
}

pub fn tools(list: &[ToolStat]) {
    let mut t = table(&["Tool", "Calls", "Errors", "Error rate", "Lines +", "Lines -"]);
    for s in list {
        t.add_row(vec![
            Cell::new(clean(&s.name)),
            Cell::new(int(s.calls)),
            Cell::new(int(s.errors)),
            Cell::new(pct(s.error_rate)),
            Cell::new(int(s.lines_added)),
            Cell::new(int(s.lines_removed)),
        ]);
    }
    right(&mut t, 1);
    println!("{t}");
}

pub fn blocks(list: &[Block], tz: &Tz) {
    let mut t = table(&["Start", "End", "Status", "Requests", "Tokens (in+out)", "Cost", "Models"]);
    for b in list {
        t.add_row(vec![
            Cell::new(local_time(b.start_ms, tz, "%Y-%m-%d %H:%M")),
            Cell::new(local_time(b.end_ms, tz, "%H:%M")),
            Cell::new(if b.active { "active" } else { "" }),
            Cell::new(int(b.requests as i64)),
            Cell::new(int(b.limit_tokens)),
            Cell::new(usd(b.cost_usd)),
            Cell::new(b.models.iter().map(|(m, _)| clean(short_model(m))).collect::<Vec<_>>().join(", ")),
        ]);
    }
    right(&mut t, 3);
    if let Some(c) = t.column_mut(6) {
        c.set_cell_alignment(CellAlignment::Left);
    }
    println!("{t}");
}

#[derive(Serialize)]
pub struct Summary {
    pub total: Agg,
    pub active_minutes: f64,
    pub compactions: i64,
    pub interrupts: i64,
    pub tool_calls: i64,
    pub tool_errors: i64,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub top: Vec<(String, Vec<(String, f64)>)>,
}

pub fn summary(s: &Summary) {
    let t = &s.total;
    let line = |k: &str, v: String| println!("{k:<16} {v}");
    line("Cost", format!("{}  (cache saved {})", usd(t.cost_usd), usd(t.cache_savings_usd)));
    line("Tokens", format!(
        "{} total: {} in, {} out, {} cache write, {} cache read",
        compact(t.total_tokens), compact(t.input), compact(t.output), compact(t.cache_write), compact(t.cache_read)
    ));
    line("Cache hit rate", pct(t.cache_hit_rate));
    line("Requests", format!("{} across {} sessions", int(t.requests as i64), int(t.sessions as i64)));
    line("Active time", minutes(s.active_minutes));
    line("Tool calls", format!(
        "{} ({} errors, {})",
        int(s.tool_calls), int(s.tool_errors), pct(s.tool_errors as f64 / s.tool_calls.max(1) as f64)
    ));
    line("Lines changed", format!("+{} / -{}", int(s.lines_added), int(s.lines_removed)));
    line("Compactions", int(s.compactions));
    line("Interrupts", int(s.interrupts));
    if t.web_searches > 0 {
        line("Web searches", int(t.web_searches));
    }
    for (dim, items) in &s.top {
        let list: Vec<String> = items.iter().map(|(k, c)| format!("{} {}", clean(k), usd(*c))).collect();
        line(&format!("Top {dim}"), list.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(int(1234567), "1,234,567");
        assert_eq!(int(-1000), "-1,000");
        assert_eq!(int(999), "999");
        assert_eq!(compact(1_250_000), "1.2M");
        assert_eq!(compact(12_345), "12k");
        assert_eq!(usd(1234.5), "$1,234.50");
        assert_eq!(usd(0.004), "$0.00");
        assert_eq!(minutes(225.0), "3h 45m");
        assert_eq!(minutes(59.6), "1h");
        assert_eq!(pct(0.1234), "12.3%");
        assert_eq!(clean("a\x1b]0;x\x07b\n"), "a\u{FFFD}]0;x\u{FFFD}b\u{FFFD}");
    }
}

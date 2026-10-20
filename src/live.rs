//! Real-time terminal view.

use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block as Panel, Borders, Gauge, Paragraph},
};
use rusqlite::Connection;

use crate::blocks::{self, Block, Limits, Plan};
use crate::db;
use crate::paths::Source;
use crate::pricing::Pricing;
use crate::report::{clean, compact, int, local_time, minutes, pct, short_model, usd};
use crate::stats::{self, Dim, Filter};

pub struct Options {
    pub plan: Plan,
    pub custom_limit_tokens: Option<i64>,
    pub refresh: Duration,
    pub tz: Tz,
    pub project: Option<String>,
}

/// History used for the custom plan's P90, as upstream.
const HISTORY_MS: i64 = 192 * 3_600_000;

struct Snapshot {
    now_ms: i64,
    block: Option<Block>,
    limits: Limits,
    burn: f64,
    today: stats::Agg,
    today_active_min: f64,
    today_tool_calls: i64,
    today_tool_errors: i64,
    today_lines: (i64, i64),
    top: Vec<(&'static str, Vec<(String, f64)>)>,
    error: Option<String>,
}

fn snapshot(conn: &mut Connection, sources: &[Source], pricing: &Pricing, o: &Options) -> Result<Snapshot> {
    db::ingest(conn, sources)?;
    let now_ms = Utc::now().timestamp_millis();
    let profiles: Vec<String> = sources.iter().map(|s| s.profile.clone()).collect();
    let base = Filter { profiles, project: o.project.clone(), ..Default::default() };
    let recent = stats::load_rows(conn, &Filter { since_ms: Some(now_ms - HISTORY_MS), ..base.clone() })?;
    let all_blocks = blocks::build(&recent, now_ms, pricing);
    let limits = o.plan.limits(&all_blocks, o.custom_limit_tokens);
    let block = all_blocks.last().filter(|b| b.active).cloned();

    let midnight = o.tz.from_utc_datetime(&Utc::now().naive_utc()).date_naive().and_hms_opt(0, 0, 0).expect("valid time");
    let midnight_ms = o.tz.from_local_datetime(&midnight).earliest().map_or(now_ms, |t| t.timestamp_millis());
    let today_f = Filter { since_ms: Some(midnight_ms), ..base };
    let today_rows: Vec<_> = recent.iter().filter(|r| r.ts >= midnight_ms).cloned().collect();
    let (_, today) = stats::aggregate(&today_rows, Dim::Day, &o.tz, pricing);
    let tools = stats::tools(conn, &today_f, false)?;
    let top = [("Projects", Dim::Project), ("Skills", Dim::Skill), ("Agents", Dim::Agent)]
        .into_iter()
        .map(|(label, dim)| {
            let (groups, _) = stats::aggregate(&today_rows, dim, &o.tz, pricing);
            (label, groups.into_iter().take(3).map(|g| (g.key, g.cost_usd)).collect())
        })
        .collect();
    Ok(Snapshot {
        now_ms,
        burn: blocks::hourly_burn_rate(&recent, now_ms),
        block,
        limits,
        today,
        today_active_min: stats::event_sum(conn, &today_f, "active_ms")? as f64 / 60_000.0,
        today_tool_calls: tools.iter().map(|t| t.calls).sum(),
        today_tool_errors: tools.iter().map(|t| t.errors).sum(),
        today_lines: tools.iter().fold((0, 0), |(a, r), t| (a + t.lines_added, r + t.lines_removed)),
        top,
        error: None,
    })
}

pub fn run(conn: &mut Connection, sources: &[Source], pricing: &Pricing, o: Options) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, conn, sources, pricing, &o);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    conn: &mut Connection,
    sources: &[Source],
    pricing: &Pricing,
    o: &Options,
) -> Result<()> {
    let mut snap: Option<Snapshot> = None;
    let mut last = Instant::now() - o.refresh;
    loop {
        if last.elapsed() >= o.refresh {
            snap = Some(match snapshot(conn, sources, pricing, o) {
                Ok(s) => s,
                // Keep showing the last good data and say why it is stale.
                Err(e) => match snap.take() {
                    Some(mut s) => {
                        s.error = Some(e.to_string());
                        s
                    }
                    None => return Err(e),
                },
            });
            last = Instant::now();
        }
        let s = snap.as_ref().expect("set above");
        terminal.draw(|f| draw(f, s, sources, o))?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Char('r') => last = Instant::now() - o.refresh,
                _ => {}
            }
        }
    }
}

fn ratio_color(r: f64) -> Color {
    match r {
        r if r < 0.5 => Color::Green,
        r if r < 0.9 => Color::Yellow,
        _ => Color::Red,
    }
}

fn gauge<'a>(title: &'a str, ratio: f64, label: String) -> Gauge<'a> {
    Gauge::default()
        .block(Panel::default().title(title))
        .gauge_style(Style::default().fg(ratio_color(ratio)))
        .ratio(ratio.clamp(0.0, 1.0))
        .label(label)
}

fn kv(k: &str, v: String) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<15}"), Style::default().add_modifier(Modifier::DIM)), Span::raw(v)])
}

fn draw(f: &mut Frame, s: &Snapshot, sources: &[Source], o: &Options) {
    let profiles: Vec<&str> = sources.iter().map(|s| s.profile.as_str()).collect();
    let [header, block_area, today_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(13),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .areas(f.area());

    f.render_widget(
        Line::from(vec![
            " CLAUDE USAGE MONITOR ".bold().reversed(),
            format!("  {}  ·  plan {:?}  ·  {}", profiles.join(", "), o.plan, o.tz).into(),
        ]),
        header,
    );

    draw_block(f, block_area, s, o);
    draw_today(f, today_area, s);

    let mut foot = vec![
        Span::raw(format!(" updated {}  ·  r refresh  ·  q quit", local_time(s.now_ms, &o.tz, "%H:%M:%S"))),
    ];
    if let Some(e) = &s.error {
        foot.push(Span::styled(format!("  ·  refresh failed: {e}"), Style::default().fg(Color::Red)));
    }
    f.render_widget(Line::from(foot).dim(), footer);
}

fn draw_block(f: &mut Frame, area: Rect, s: &Snapshot, o: &Options) {
    let outer = Panel::default().borders(Borders::ALL).title(match &s.block {
        Some(b) => format!(
            " Current 5-hour block  {} – {} ",
            local_time(b.start_ms, &o.tz, "%H:%M"),
            local_time(b.end_ms, &o.tz, "%H:%M")
        ),
        None => " No active block ".into(),
    });
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let Some(b) = &s.block else {
        f.render_widget(Paragraph::new("No requests in the last five hours."), inner);
        return;
    };
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(3),
    ])
    .split(inner);

    let l = &s.limits;
    let tok = b.limit_tokens as f64 / l.tokens as f64;
    f.render_widget(gauge("Tokens", tok, format!("{} / {}  ({})", int(b.limit_tokens), int(l.tokens), pct(tok))), rows[0]);
    let cost = b.cost_usd / l.cost_usd;
    f.render_widget(gauge("Cost", cost, format!("{} / {}  ({})", usd(b.cost_usd), usd(l.cost_usd), pct(cost))), rows[1]);
    let msg = b.requests as f64 / l.messages as f64;
    f.render_widget(gauge("Requests", msg, format!("{} / {}", int(b.requests as i64), int(l.messages as i64))), rows[2]);
    let elapsed = (s.now_ms - b.start_ms) as f64 / blocks::BLOCK_MS as f64;
    let left_min = (b.end_ms - s.now_ms) as f64 / 60_000.0;
    f.render_widget(
        Gauge::default()
            .block(Panel::default().title("Time"))
            .gauge_style(Style::default().fg(Color::Blue))
            .ratio(elapsed.clamp(0.0, 1.0))
            .label(format!("{} left, resets {}", minutes(left_min), local_time(b.end_ms, &o.tz, "%H:%M"))),
        rows[3],
    );

    let (emoji, speed) = blocks::velocity(s.burn);
    let models = b
        .models
        .iter()
        .map(|(m, n)| format!("{} {}", clean(short_model(m)), pct(*n as f64 / b.limit_tokens.max(1) as f64)))
        .collect::<Vec<_>>()
        .join(", ");
    let outlook = match blocks::runs_out_at(b, l.tokens, s.burn, s.now_ms) {
        Some(at) => Span::styled(format!("tokens run out at {}", local_time(at, &o.tz, "%H:%M")), Style::default().fg(Color::Red)),
        None => Span::styled("within limit until reset", Style::default().fg(Color::Green)),
    };
    f.render_widget(
        Paragraph::new(vec![
            kv("Models", models),
            kv("Burn rate", format!("{} tok/min {emoji} {speed}  ·  {}/hr", int(s.burn as i64), usd(blocks::cost_rate(b, s.now_ms)))),
            Line::from(vec![Span::styled(format!("{:<15}", "Outlook"), Style::default().add_modifier(Modifier::DIM)), outlook]),
        ]),
        rows[4],
    );
}

fn draw_today(f: &mut Frame, area: Rect, s: &Snapshot) {
    let outer = Panel::default().borders(Borders::ALL).title(" Today ");
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);
    let t = &s.today;
    f.render_widget(
        Paragraph::new(vec![
            kv("Cost", format!("{}  (cache saved {})", usd(t.cost_usd), usd(t.cache_savings_usd))),
            kv("Tokens", format!("{} in · {} out · {} cached", compact(t.input), compact(t.output), compact(t.cache_read + t.cache_write))),
            kv("Cache hit", pct(t.cache_hit_rate)),
            kv("Sessions", format!("{}  ·  {} requests", int(t.sessions as i64), int(t.requests as i64))),
            kv("Active time", minutes(s.today_active_min)),
            kv("Tool calls", format!(
                "{}  ·  {} errors",
                int(s.today_tool_calls),
                pct(s.today_tool_errors as f64 / s.today_tool_calls.max(1) as f64)
            )),
            kv("Lines", format!("+{} / -{}", int(s.today_lines.0), int(s.today_lines.1))),
        ]),
        left,
    );
    let mut lines = Vec::new();
    for (label, items) in &s.top {
        lines.push(Line::from(format!("Top {label}")).bold());
        for (k, c) in items {
            lines.push(Line::from(format!("  {:>9}  {}", usd(*c), clean(k))));
        }
    }
    f.render_widget(Paragraph::new(lines), right);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    #[test]
    fn renders() {
        let now = 10 * 3_600_000;
        let block = Block {
            start_ms: 8 * 3_600_000,
            end_ms: 13 * 3_600_000,
            last_ms: now,
            active: true,
            requests: 120,
            limit_tokens: 40_000,
            cost_usd: 12.5,
            models: vec![("claude-opus-5-5".into(), 40_000)],
            ..Default::default()
        };
        let s = Snapshot {
            now_ms: now,
            block: Some(block),
            limits: Limits { tokens: 44_000, cost_usd: 50.0, messages: 250 },
            burn: 320.0,
            today: { let mut a = stats::Agg::default(); a.cost_usd = 30.0; a },
            today_active_min: 95.0,
            today_tool_calls: 400,
            today_tool_errors: 12,
            today_lines: (900, 40),
            top: vec![("Projects", vec![("~/dev/x".into(), 20.0)])],
            error: Some("boom".into()),
        };
        let sources = [Source { profile: "default".into(), dir: PathBuf::from("/x") }];
        let o = Options { plan: Plan::Custom, custom_limit_tokens: None, refresh: Duration::from_secs(10), tz: chrono_tz::UTC, project: None };
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| draw(f, &s, &sources, &o)).unwrap();
        let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        for want in ["Current 5-hour block", "40,000 / 44,000", "tokens run out at", "Top Projects", "refresh failed: boom"] {
            assert!(text.contains(want), "missing {want}");
        }
    }
}

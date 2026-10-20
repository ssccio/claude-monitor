mod blocks;
mod db;
mod live;
mod parse;
mod paths;
mod pricing;
mod report;
mod stats;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use clap::{Parser, Subcommand};

use blocks::Plan;
use stats::{Dim, Filter};

/// Claude Code usage monitor: live 5-hour block view and usage reports.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Claude config dir or its `projects` dir to read. Repeatable. Overrides
    /// CLAUDE_CONFIG_DIR.
    #[arg(long, global = true)]
    data_dir: Vec<PathBuf>,

    /// Read every ~/.claude* profile.
    #[arg(long, short = 'a', global = true)]
    all_profiles: bool,

    /// Usage database.
    #[arg(long, global = true, env = "CLAUDE_MONITOR_DB")]
    db: Option<PathBuf>,

    /// IANA timezone for dates and times. Defaults to the system zone.
    #[arg(long, global = true)]
    tz: Option<String>,

    /// Start of the range: YYYY-MM-DD, or a relative age such as 7d or 12h.
    #[arg(long, global = true)]
    since: Option<String>,

    /// End of the range, inclusive: YYYY-MM-DD.
    #[arg(long, global = true)]
    until: Option<String>,

    /// Only include working directories containing this text.
    #[arg(long, short = 'p', global = true)]
    project: Option<String>,

    /// JSON output for reports.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Live view of the current 5-hour block (default).
    Live {
        #[arg(long, value_enum, default_value = "custom")]
        plan: Plan,
        /// Token limit for the custom plan instead of the P90 estimate.
        #[arg(long)]
        custom_limit_tokens: Option<i64>,
        /// Seconds between refreshes.
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=60))]
        refresh: u64,
    },
    /// Headline totals for the range.
    Summary,
    /// Usage per day.
    Daily,
    /// Usage per week, starting Monday.
    Weekly,
    /// Usage per month.
    Monthly,
    /// Usage grouped by one dimension.
    Breakdown {
        #[arg(value_enum)]
        by: Dim,
    },
    /// Sessions, newest first.
    Sessions {
        #[arg(long, short = 'n', default_value_t = 25)]
        limit: usize,
    },
    /// Tool calls, errors and lines changed.
    Tools {
        /// List each MCP tool instead of grouping by server.
        #[arg(long)]
        full: bool,
    },
    /// 5-hour blocks, newest first.
    Blocks {
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
    },
    /// Read new transcript lines into the database and report what was read.
    Ingest,
    /// Refresh model prices from LiteLLM.
    Pricing,
}

fn timezone(name: Option<&str>) -> Result<Tz> {
    let name = match name {
        Some(n) => n.to_string(),
        None => iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()),
    };
    name.parse().map_err(|_| anyhow::anyhow!("unknown timezone {name}"))
}

fn day_start(date: &str, tz: &Tz) -> Result<i64> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").with_context(|| format!("bad date {date}, want YYYY-MM-DD"))?;
    let local = d.and_hms_opt(0, 0, 0).expect("midnight");
    Ok(tz.from_local_datetime(&local).earliest().context("date not representable in timezone")?.timestamp_millis())
}

fn since_ms(s: &str, tz: &Tz) -> Result<i64> {
    let now = Utc::now().timestamp_millis();
    let rel = |suffix: char, unit: i64| s.strip_suffix(suffix).and_then(|n| n.parse::<i64>().ok()).map(|n| now - n * unit);
    if let Some(ms) = rel('d', 86_400_000).or_else(|| rel('h', 3_600_000)) {
        return Ok(ms);
    }
    day_start(s, tz)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let tz = timezone(cli.tz.as_deref())?;
    let home = dirs::home_dir().context("no home directory")?;
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
    let sources = paths::resolve(&cli.data_dir, cli.all_profiles, config_dir.as_deref(), &home);
    if sources.is_empty() {
        bail!("no Claude transcripts found; pass --data-dir or set CLAUDE_CONFIG_DIR");
    }
    for s in &sources {
        if !s.dir.is_dir() {
            bail!("{} is not a directory", s.dir.display());
        }
    }

    let db_path = cli.db.clone().unwrap_or_else(db::default_path);
    let pricing_path = db_path.with_file_name("pricing.json");
    let mut conn = db::open(&db_path)?;
    let pricing = pricing::Pricing::load(&pricing_path);

    let cmd = cli.cmd.unwrap_or(Cmd::Live { plan: Plan::Custom, custom_limit_tokens: None, refresh: 10 });
    if let Cmd::Live { plan, custom_limit_tokens, refresh } = cmd {
        return live::run(&mut conn, &sources, &pricing, live::Options {
            plan,
            custom_limit_tokens,
            refresh: Duration::from_secs(refresh),
            tz,
            project: cli.project,
        });
    }
    if let Cmd::Pricing = cmd {
        let n = pricing::update(&pricing_path)?;
        println!("{n} Claude models written to {}", pricing_path.display());
        return Ok(());
    }

    let ingested = db::ingest(&mut conn, &sources)?;
    if let Cmd::Ingest = cmd {
        println!(
            "read {:.1} MB from {} files into {}",
            ingested.bytes_read as f64 / 1e6,
            ingested.files_read,
            db_path.display()
        );
        return Ok(());
    }

    let filter = Filter {
        profiles: sources.iter().map(|s| s.profile.clone()).collect(),
        since_ms: cli.since.as_deref().map(|s| since_ms(s, &tz)).transpose()?,
        until_ms: cli.until.as_deref().map(|u| day_start(u, &tz).map(|ms| ms + 86_400_000)).transpose()?,
        project: cli.project.clone(),
    };
    let rows = stats::load_rows(&conn, &filter)?;

    match cmd {
        Cmd::Daily | Cmd::Weekly | Cmd::Monthly | Cmd::Breakdown { .. } => {
            let dim = match cmd {
                Cmd::Daily => Dim::Day,
                Cmd::Weekly => Dim::Week,
                Cmd::Monthly => Dim::Month,
                Cmd::Breakdown { by } => by,
                _ => unreachable!(),
            };
            let (groups, total) = stats::aggregate(&rows, dim, &tz, &pricing);
            if cli.json {
                report::print_json(&serde_json::json!({ "groups": groups, "total": total }))?;
            } else {
                report::aggregates(dim, &groups, &total);
            }
        }
        Cmd::Sessions { limit } => {
            let mut list = stats::sessions(&conn, &rows, &pricing)?;
            list.truncate(limit);
            if cli.json { report::print_json(&list)? } else { report::sessions(&list, &tz) }
        }
        Cmd::Tools { full } => {
            let list = stats::tools(&conn, &filter, full)?;
            if cli.json { report::print_json(&list)? } else { report::tools(&list) }
        }
        Cmd::Blocks { limit } => {
            let mut list = blocks::build(&rows, Utc::now().timestamp_millis(), &pricing);
            list.reverse();
            list.truncate(limit);
            if cli.json { report::print_json(&list)? } else { report::blocks(&list, &tz) }
        }
        Cmd::Summary => {
            let (_, total) = stats::aggregate(&rows, Dim::Profile, &tz, &pricing);
            let tools = stats::tools(&conn, &filter, false)?;
            let top = [("projects", Dim::Project), ("models", Dim::Model), ("skills", Dim::Skill), ("agents", Dim::Agent)]
                .into_iter()
                .map(|(label, dim)| {
                    let (groups, _) = stats::aggregate(&rows, dim, &tz, &pricing);
                    (label.to_string(), groups.into_iter().take(3).map(|g| (g.key, g.cost_usd)).collect())
                })
                .collect();
            let s = report::Summary {
                total,
                active_minutes: stats::event_sum(&conn, &filter, "active_ms")? as f64 / 60_000.0,
                compactions: stats::event_sum(&conn, &filter, "compaction")?,
                interrupts: stats::event_sum(&conn, &filter, "interrupt")?,
                tool_calls: tools.iter().map(|t| t.calls).sum(),
                tool_errors: tools.iter().map(|t| t.errors).sum(),
                lines_added: tools.iter().map(|t| t.lines_added).sum(),
                lines_removed: tools.iter().map(|t| t.lines_removed).sum(),
                top,
            };
            if cli.json { report::print_json(&s)? } else { report::summary(&s) }
        }
        Cmd::Live { .. } | Cmd::Pricing | Cmd::Ingest => unreachable!("handled above"),
    }
    Ok(())
}

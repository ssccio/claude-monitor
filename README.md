# claude-monitor

A Rust port of [Claude Code Usage Monitor](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor)
(the Python `claude-monitor` 3.1.0), with support for `CLAUDE_CONFIG_DIR`, multiple
profiles, and reports the original doesn't have.

It reads the JSONL transcripts Claude Code writes under `<config dir>/projects`,
stores them in a local SQLite database, and works from there. The first run reads
everything (about 5 seconds for 3 GB of transcripts). Later runs read only what
was appended, usually in tens of milliseconds.

## Install

Download a build for macOS, Linux or Windows from
[Releases](https://github.com/ssccio/claude-monitor/releases), or build from source:

```bash
cargo install --git https://github.com/ssccio/claude-monitor
```

## Which transcripts it reads

In order of precedence:

1. `--data-dir <dir>`, repeatable. Either a config dir (`~/.claude-work`) or its `projects` dir.
2. `--all-profiles` / `-a`: every `~/.claude*` directory that has a `projects` dir, plus `~/.config/claude`.
3. `$CLAUDE_CONFIG_DIR`.
4. `~/.claude` and `~/.config/claude`.

Each source is labelled as a profile (`~/.claude` is `default`, `~/.claude-work` is
`work`) so reports can be split with `breakdown profile`.

Sessions started from Claude Desktop's Code tab write the same transcripts and show
up with surface `Desktop`. Desktop chat conversations are not stored locally, so
they are not counted.

## Commands

```
claude-monitor [live]        live view of the current 5-hour block (default)
claude-monitor summary       headline totals
claude-monitor daily         per day (also weekly, monthly)
claude-monitor breakdown X   group by day, week, month, model, project, branch,
                             surface, profile, agent or skill
claude-monitor sessions      sessions with title, active time, compactions, interrupts
claude-monitor tools         tool calls, error rate, lines added and removed
claude-monitor blocks        past 5-hour blocks
claude-monitor ingest        read new transcript lines and stop
claude-monitor pricing       refresh model prices from LiteLLM
```

Global options: `--since 7d` (or `12h`, or `2026-10-01`), `--until 2026-10-19`,
`-p/--project <text>` to filter by working directory, `--tz America/New_York`,
`--json`, and `--db <path>` (or `CLAUDE_MONITOR_DB`).

The live view takes `--plan pro|max5|max20|custom` (default `custom`),
`--custom-limit-tokens <n>` and `--refresh <seconds>`. Keys: `r` refreshes, `q` quits.

## What it reports beyond the original

Cost per agent type and per skill, from the `attributionAgent` and `attributionSkill`
fields on each request. Subagent requests are counted, not just the main thread.

Split by surface (CLI, Desktop, SDK), git branch, profile and project.

Cache writes priced at the 5-minute and 1-hour rates separately, cache hit rate,
and the dollar amount caching saved.

Session titles, active time (from Claude Code's turn durations), compactions and
user interrupts.

Tool calls with error rates, MCP tools grouped by server (`--full` lists each
tool), and lines added and removed by Edit, MultiEdit and Write.

Web search and fetch counts.

## Pricing

Prices come from a bundled snapshot of LiteLLM's model price list. The original's
hardcoded prices are well out of date for current models. `claude-monitor pricing`
fetches the current list into `pricing.json` next to the database, and that file
takes precedence over the bundled one. A model missing from the list falls back to
the newest model of the same family.

## Plan limits

The block, limit and burn-rate rules match the original:

A block starts at the UTC hour of its first request and lasts five hours. A
request after the block ends, or after five idle hours, starts a new one.

Usage against the limit is input plus output tokens. Cache reads and writes are
excluded, as in the original, so the token gauge understates what Anthropic counts.
The real limit percentage is only available from Anthropic's API, which this tool
doesn't call.

The `custom` plan's token limit is the 90th percentile of completed blocks from the
last 8 days, using only blocks that came within 5% of a known plan limit if there
are any, and never below 19,000. Cost and message limits use the same history.

Not ported: saved settings between runs, notifications, theme detection and the
12-hour clock option.

## License

MIT. Includes the original project's copyright notice.

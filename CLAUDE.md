# claude-monitor

Rust CLI that reads Claude Code transcripts (`<config dir>/projects/**/*.jsonl`)
into a local SQLite database and reports usage, plus a live view of the current
5-hour block. Port of the Python `claude-monitor` 3.1.0.

## Layout

`parse.rs` turns one JSONL line into records. `db.rs` ingests files incrementally
by byte offset. `paths.rs` resolves which config dirs to read. `pricing.rs` holds
model prices (`src/pricing.json`, a LiteLLM snapshot). `stats.rs` aggregates,
`blocks.rs` has the 5-hour block and plan-limit math, `report.rs` prints tables,
`live.rs` is the ratatui view.

## Checks

```bash
cargo clippy --all-targets -- -D warnings
cargo test
```

Block, P90 and burn-rate rules follow upstream and have tests pinning them to
Python's results. Change them only on purpose.

## Git

Work on a branch; a pre-commit hook blocks commits to `main`. `.envrc`,
`.env.local` and `.mcp.json` are gitignored and machine-local.

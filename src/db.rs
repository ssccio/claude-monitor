//! SQLite store. Transcripts are read incrementally from the last offset seen, so
//! history survives Claude Code's transcript cleanup and refreshes stay cheap.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use walkdir::WalkDir;

use crate::parse::{Record, parse_line};
use crate::paths::Source;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL, offset INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS usage (
    key TEXT PRIMARY KEY, ts INTEGER NOT NULL, profile TEXT NOT NULL, session_id TEXT NOT NULL,
    cwd TEXT NOT NULL, branch TEXT NOT NULL, entrypoint TEXT NOT NULL, sidechain INTEGER NOT NULL,
    agent TEXT NOT NULL, skill TEXT NOT NULL, model TEXT NOT NULL,
    input INTEGER NOT NULL, output INTEGER NOT NULL, cache_5m INTEGER NOT NULL,
    cache_1h INTEGER NOT NULL, cache_read INTEGER NOT NULL,
    web_search INTEGER NOT NULL, web_fetch INTEGER NOT NULL, speed TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_ts ON usage(ts);
CREATE INDEX IF NOT EXISTS usage_session ON usage(session_id);
CREATE TABLE IF NOT EXISTS tools (
    id TEXT PRIMARY KEY, ts INTEGER NOT NULL, profile TEXT NOT NULL, session_id TEXT NOT NULL,
    cwd TEXT NOT NULL, name TEXT NOT NULL, agent TEXT NOT NULL, skill TEXT NOT NULL,
    added INTEGER NOT NULL, removed INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS tools_ts ON tools(ts);
CREATE TABLE IF NOT EXISTS tool_results (id TEXT PRIMARY KEY, is_error INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS events (
    key TEXT PRIMARY KEY, ts INTEGER NOT NULL, profile TEXT NOT NULL, session_id TEXT NOT NULL,
    kind TEXT NOT NULL, value INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS events_session ON events(session_id);
CREATE TABLE IF NOT EXISTS titles (session_id TEXT PRIMARY KEY, title TEXT NOT NULL);
";

pub fn default_path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("claude-monitor").join("usage.db")
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

#[derive(Debug, Default)]
pub struct IngestStats {
    pub files_read: usize,
    pub bytes_read: u64,
}

/// Read every new line under each source into the store.
pub fn ingest(conn: &mut Connection, sources: &[Source]) -> Result<IngestStats> {
    let mut stats = IngestStats::default();
    let tx = conn.transaction()?;
    for source in sources {
        let files = WalkDir::new(&source.dir)
            .follow_links(true)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"));
        for entry in files {
            let bytes = ingest_file(&tx, &source.profile, entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            if bytes > 0 {
                stats.files_read += 1;
                stats.bytes_read += bytes;
            }
        }
    }
    tx.commit()?;
    Ok(stats)
}

fn ingest_file(conn: &Connection, profile: &str, path: &Path) -> Result<u64> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64);
    let key = path.to_string_lossy();
    let prev: Option<(i64, i64, i64)> = conn
        .query_row("SELECT size, mtime, offset FROM files WHERE path = ?1", [&key], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let start = match prev {
        Some((s, m, _)) if s == size && m == mtime => return Ok(0),
        // Appended to: pick up where we stopped. Shrunk or rewritten: start over;
        // primary keys make re-reading idempotent.
        Some((_, _, off)) if off <= size => off,
        _ => 0,
    };

    let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
    reader.seek(SeekFrom::Start(start as u64))?;
    let mut offset = start;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        // Stop at a partial trailing line; it is re-read once Claude finishes writing it.
        if n == 0 || !buf.ends_with('\n') {
            break;
        }
        offset += n as i64;
        for record in parse_line(&buf) {
            store(conn, profile, record)?;
        }
    }
    conn.execute(
        "INSERT OR REPLACE INTO files (path, size, mtime, offset) VALUES (?1, ?2, ?3, ?4)",
        params![key, size, mtime, offset],
    )?;
    Ok((offset - start) as u64)
}

fn store(conn: &Connection, profile: &str, record: Record) -> Result<()> {
    match record {
        Record::Usage(u) => {
            conn.prepare_cached(
                "INSERT OR IGNORE INTO usage VALUES
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            )?
            .execute(params![
                u.key, u.ts, profile, u.session_id, u.cwd, u.branch, u.entrypoint, u.sidechain,
                u.agent, u.skill, u.model, u.input, u.output, u.cache_5m, u.cache_1h, u.cache_read,
                u.web_search, u.web_fetch, u.speed
            ])?;
        }
        Record::Tool(t) => {
            conn.prepare_cached("INSERT OR IGNORE INTO tools VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)")?
                .execute(params![
                    t.id, t.ts, profile, t.session_id, t.cwd, t.name, t.agent, t.skill, t.added, t.removed
                ])?;
        }
        Record::ToolResult { id, is_error } => {
            conn.prepare_cached("INSERT OR REPLACE INTO tool_results VALUES (?1, ?2)")?
                .execute(params![id, is_error])?;
        }
        Record::Event { key, ts, session_id, kind, value } => {
            conn.prepare_cached("INSERT OR IGNORE INTO events VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
                .execute(params![key, ts, profile, session_id, kind, value])?;
        }
        Record::Title { session_id, title } => {
            conn.prepare_cached("INSERT OR REPLACE INTO titles VALUES (?1, ?2)")?
                .execute(params![session_id, title])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const A: &str = r#"{"type":"assistant","uuid":"u1","timestamp":"2026-10-19T12:00:00Z","sessionId":"s","requestId":"r1","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":2}}}"#;
    const B: &str = r#"{"type":"assistant","uuid":"u2","timestamp":"2026-10-19T12:01:00Z","sessionId":"s","requestId":"r2","message":{"id":"m2","model":"claude-opus-5-5","usage":{"input_tokens":3,"output_tokens":4}}}"#;

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn incremental_and_dedup() {
        let dir = std::env::temp_dir().join(format!("cm-db-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let file = dir.join("proj/s.jsonl");
        let mut conn = open(&dir.join("t.db")).unwrap();
        let src = [Source { profile: "p".into(), dir: dir.clone() }];

        // Duplicate line plus a partial trailing line.
        std::fs::write(&file, format!("{A}\n{A}\n{}", &B[..20])).unwrap();
        ingest(&mut conn, &src).unwrap();
        assert_eq!(count(&conn), 1);

        // Finish the partial line; only the new bytes are read.
        let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(f, "{}", &B[20..]).unwrap();
        let stats = ingest(&mut conn, &src).unwrap();
        assert_eq!(count(&conn), 2);
        assert_eq!(stats.bytes_read, B.len() as u64 + 1);

        // Unchanged file is skipped.
        assert_eq!(ingest(&mut conn, &src).unwrap().files_read, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

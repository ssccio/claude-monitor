//! Line-level parsing of Claude Code JSONL transcripts.

use chrono::DateTime;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub subtype: Option<String>,
    pub uuid: Option<String>,
    pub timestamp: Option<String>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub is_sidechain: Option<bool>,
    pub attribution_agent: Option<String>,
    pub attribution_skill: Option<String>,
    pub request_id: Option<String>,
    pub duration_ms: Option<i64>,
    pub ai_title: Option<String>,
    pub message: Option<Message>,
}

#[derive(Deserialize)]
pub struct Message {
    pub id: Option<String>,
    pub model: Option<String>,
    pub usage: Option<Usage>,
    pub content: Option<Content>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

#[derive(Deserialize)]
pub struct Block {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub input: Option<Value>,
    pub tool_use_id: Option<String>,
    pub is_error: Option<bool>,
    pub text: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_creation_input_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: i64,
    pub cache_creation: Option<CacheCreation>,
    pub server_tool_use: Option<ServerToolUse>,
    pub speed: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct CacheCreation {
    #[serde(default)]
    pub ephemeral_5m_input_tokens: i64,
    #[serde(default)]
    pub ephemeral_1h_input_tokens: i64,
}

#[derive(Deserialize, Default)]
pub struct ServerToolUse {
    #[serde(default)]
    pub web_search_requests: i64,
    #[serde(default)]
    pub web_fetch_requests: i64,
}

/// One priced API response.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UsageRow {
    pub key: String,
    pub ts: i64,
    pub session_id: String,
    pub cwd: String,
    pub branch: String,
    pub entrypoint: String,
    pub sidechain: bool,
    pub agent: String,
    pub skill: String,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub cache_5m: i64,
    pub cache_1h: i64,
    pub cache_read: i64,
    pub web_search: i64,
    pub web_fetch: i64,
    pub speed: String,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ToolRow {
    pub id: String,
    pub ts: i64,
    pub session_id: String,
    pub cwd: String,
    pub name: String,
    pub agent: String,
    pub skill: String,
    pub added: i64,
    pub removed: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Usage(UsageRow),
    Tool(ToolRow),
    ToolResult { id: String, is_error: bool },
    /// Session-level event: compaction, interrupt, or active turn time.
    Event { key: String, ts: i64, session_id: String, kind: &'static str, value: i64 },
    Title { session_id: String, title: String },
}

pub fn parse_ts(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp_millis())
}

fn lines_of(v: Option<&Value>) -> i64 {
    match v.and_then(Value::as_str) {
        Some("") | None => 0,
        Some(s) => s.lines().count() as i64,
    }
}

/// Lines added and removed by a file-editing tool call.
fn line_delta(name: &str, input: Option<&Value>) -> (i64, i64) {
    let Some(input) = input else { return (0, 0) };
    match name {
        "Edit" => (lines_of(input.get("new_string")), lines_of(input.get("old_string"))),
        "MultiEdit" => input
            .get("edits")
            .and_then(Value::as_array)
            .map(|edits| {
                edits.iter().fold((0, 0), |(a, r), e| {
                    (a + lines_of(e.get("new_string")), r + lines_of(e.get("old_string")))
                })
            })
            .unwrap_or((0, 0)),
        "Write" => (lines_of(input.get("content")), 0),
        _ => (0, 0),
    }
}

/// Parse one JSONL line into zero or more records.
pub fn parse_line(raw: &str) -> Vec<Record> {
    let Ok(line) = serde_json::from_str::<Line>(raw) else { return Vec::new() };
    let mut out = Vec::new();
    let ts = line.timestamp.as_deref().and_then(parse_ts).unwrap_or(0);
    let session_id = line.session_id.clone().unwrap_or_default();
    let uuid = line.uuid.clone().unwrap_or_default();

    match line.kind.as_deref() {
        Some("assistant") => {
            let Some(msg) = &line.message else { return out };
            let agent = line.attribution_agent.clone().unwrap_or_default();
            let skill = line.attribution_skill.clone().unwrap_or_default();
            let cwd = line.cwd.clone().unwrap_or_default();
            if let Some(u) = &msg.usage
                && ts > 0
            {
                let (c5, c1) = match &u.cache_creation {
                    Some(c) if c.ephemeral_5m_input_tokens + c.ephemeral_1h_input_tokens > 0 => {
                        (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens)
                    }
                    _ => (u.cache_creation_input_tokens, 0),
                };
                let key = match (&msg.id, &line.request_id) {
                    (Some(m), Some(r)) => format!("{m}:{r}"),
                    _ => uuid.clone(),
                };
                let stu = u.server_tool_use.as_ref();
                out.push(Record::Usage(UsageRow {
                    key,
                    ts,
                    session_id: session_id.clone(),
                    cwd: cwd.clone(),
                    branch: line.git_branch.clone().unwrap_or_default(),
                    entrypoint: line.entrypoint.clone().unwrap_or_default(),
                    sidechain: line.is_sidechain.unwrap_or(false),
                    agent: agent.clone(),
                    skill: skill.clone(),
                    model: msg.model.clone().unwrap_or_default(),
                    input: u.input_tokens,
                    output: u.output_tokens,
                    cache_5m: c5,
                    cache_1h: c1,
                    cache_read: u.cache_read_input_tokens,
                    web_search: stu.map_or(0, |s| s.web_search_requests),
                    web_fetch: stu.map_or(0, |s| s.web_fetch_requests),
                    speed: u.speed.clone().unwrap_or_default(),
                }));
            }
            if let Some(Content::Blocks(blocks)) = &msg.content {
                for b in blocks {
                    if b.kind.as_deref() != Some("tool_use") {
                        continue;
                    }
                    let (Some(id), Some(name)) = (&b.id, &b.name) else { continue };
                    let (added, removed) = line_delta(name, b.input.as_ref());
                    out.push(Record::Tool(ToolRow {
                        id: id.clone(),
                        ts,
                        session_id: session_id.clone(),
                        cwd: cwd.clone(),
                        name: name.clone(),
                        agent: agent.clone(),
                        skill: skill.clone(),
                        added,
                        removed,
                    }));
                }
            }
        }
        Some("user") => {
            let Some(msg) = &line.message else { return out };
            let interrupted = |t: &str| t.starts_with("[Request interrupted by user");
            match &msg.content {
                Some(Content::Text(t)) if interrupted(t) => {
                    out.push(Record::Event { key: uuid, ts, session_id, kind: "interrupt", value: 1 });
                }
                Some(Content::Blocks(blocks)) => {
                    let mut interrupt = false;
                    for b in blocks {
                        match b.kind.as_deref() {
                            Some("tool_result") => {
                                if let Some(id) = &b.tool_use_id {
                                    out.push(Record::ToolResult {
                                        id: id.clone(),
                                        is_error: b.is_error.unwrap_or(false),
                                    });
                                }
                            }
                            Some("text") => interrupt |= b.text.as_deref().is_some_and(interrupted),
                            _ => {}
                        }
                    }
                    if interrupt {
                        out.push(Record::Event { key: uuid, ts, session_id, kind: "interrupt", value: 1 });
                    }
                }
                _ => {}
            }
        }
        Some("system") => match line.subtype.as_deref() {
            Some("compact_boundary") => {
                out.push(Record::Event { key: uuid, ts, session_id, kind: "compaction", value: 1 });
            }
            Some("turn_duration") => {
                let ms = line.duration_ms.unwrap_or(0);
                out.push(Record::Event { key: uuid, ts, session_id, kind: "active_ms", value: ms });
            }
            _ => {}
        },
        Some("ai-title") => {
            if let Some(title) = line.ai_title {
                out.push(Record::Title { session_id, title });
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_usage_with_cache_split_and_tool() {
        let raw = r#"{"type":"assistant","uuid":"u1","timestamp":"2026-10-19T12:00:00.000Z","sessionId":"s1","cwd":"/x","gitBranch":"main","entrypoint":"cli","isSidechain":true,"attributionAgent":"grunt","requestId":"r1","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":2,"output_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":50,"cache_creation":{"ephemeral_5m_input_tokens":40,"ephemeral_1h_input_tokens":60},"server_tool_use":{"web_search_requests":1}},"content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"old_string":"a\nb","new_string":"a\nb\nc"}}]}}"#;
        let recs = parse_line(raw);
        assert_eq!(recs.len(), 2);
        let Record::Usage(u) = &recs[0] else { panic!() };
        assert_eq!(u.key, "m1:r1");
        assert_eq!((u.cache_5m, u.cache_1h, u.cache_read), (40, 60, 50));
        assert_eq!(u.web_search, 1);
        assert!(u.sidechain);
        assert_eq!(u.agent, "grunt");
        let Record::Tool(t) = &recs[1] else { panic!() };
        assert_eq!((t.added, t.removed), (3, 2));
    }

    #[test]
    fn cache_creation_without_split_counts_as_5m() {
        let raw = r#"{"type":"assistant","uuid":"u1","timestamp":"2026-10-19T12:00:00Z","message":{"id":"m1","model":"m","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":7}}}"#;
        let Record::Usage(u) = &parse_line(raw)[0] else { panic!() };
        assert_eq!(u.key, "u1");
        assert_eq!((u.cache_5m, u.cache_1h), (7, 0));
    }

    #[test]
    fn user_tool_result_and_interrupt() {
        let raw = r#"{"type":"user","uuid":"u2","timestamp":"2026-10-19T12:00:00Z","sessionId":"s1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"boom"},{"type":"text","text":"[Request interrupted by user]"}]}}"#;
        let recs = parse_line(raw);
        assert_eq!(recs[0], Record::ToolResult { id: "t1".into(), is_error: true });
        assert!(matches!(recs[1], Record::Event { kind: "interrupt", .. }));
    }

    #[test]
    fn system_and_title() {
        let c = parse_line(r#"{"type":"system","subtype":"compact_boundary","uuid":"c","timestamp":"2026-10-19T12:00:00Z","sessionId":"s"}"#);
        assert!(matches!(c[0], Record::Event { kind: "compaction", .. }));
        let d = parse_line(r#"{"type":"system","subtype":"turn_duration","durationMs":1500,"uuid":"d","timestamp":"2026-10-19T12:00:00Z","sessionId":"s"}"#);
        assert!(matches!(d[0], Record::Event { kind: "active_ms", value: 1500, .. }));
        let t = parse_line(r#"{"type":"ai-title","aiTitle":"Hello","sessionId":"s"}"#);
        assert_eq!(t[0], Record::Title { session_id: "s".into(), title: "Hello".into() });
    }

    #[test]
    fn garbage_is_ignored() {
        assert!(parse_line("not json").is_empty());
        assert!(parse_line(r#"{"type":"api-request-blob"}"#).is_empty());
    }
}

//! Full-text search over conversation content across sessions.

use std::collections::HashMap;

use rusqlite::params;
use tracing::warn;

use super::{decode, EventStore};
use crate::acp::state::Event;

const MIN_SEARCH_CHARS: usize = 2;
/// Cap on query length so a pathological input can't build a huge LIKE pattern.
const MAX_SEARCH_CHARS: usize = 128;
const MAX_SEARCH_RESULTS: usize = 20;
/// Rows the LIKE prefilter may scan, so a no-match query cannot walk a huge table.
const SEARCH_ROW_SCAN_CAP: usize = 2000;
const SNIPPET_RADIUS_CHARS: usize = 60;

/// One session's newest matching event plus how many of its events matched.
#[derive(Debug, Clone)]
pub struct ContentHit {
    pub session_id: String,
    pub seq: u64,
    pub kind: &'static str,
    pub snippet: String,
    pub match_count: usize,
}

impl EventStore {
    /// Search agent messages, user prompts, and tool output, newest match
    /// first, one hit per session. A LIKE over the raw JSON prefilters; each
    /// candidate is decoded and matched against its prose.
    pub fn search_content(&self, query: &str, limit: usize) -> Vec<ContentHit> {
        let trimmed = query.trim();
        if trimmed.chars().count() < MIN_SEARCH_CHARS {
            return Vec::new();
        }
        let needle = trimmed
            .chars()
            .take(MAX_SEARCH_CHARS)
            .collect::<String>()
            .to_lowercase();
        let limit = limit.clamp(1, MAX_SEARCH_RESULTS);
        let pattern = format!("%{}%", escape_like(&needle));

        let conn = self.search_conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = match conn.prepare(
            "SELECT session_id, seq, event_json FROM acp_events
             WHERE event_json LIKE ?1 ESCAPE '\\'
             ORDER BY created_at DESC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "acp.event_store", "prepare search query: {e}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(params![pattern, SEARCH_ROW_SCAN_CAP as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "acp.event_store", "run search query: {e}");
                return Vec::new();
            }
        };

        let mut order: Vec<String> = Vec::new();
        let mut hits: HashMap<String, ContentHit> = HashMap::new();
        for (session_id, seq, json) in rows.flatten() {
            let Some((kind, text)) = decode(&json).as_ref().and_then(event_search_text) else {
                continue;
            };
            // The LIKE matched raw JSON; require the match in the prose itself.
            if !text.to_lowercase().contains(&needle) {
                continue;
            }
            if let Some(hit) = hits.get_mut(&session_id) {
                hit.match_count += 1;
                continue;
            }
            if order.len() >= limit {
                break;
            }
            order.push(session_id.clone());
            hits.insert(
                session_id.clone(),
                ContentHit {
                    session_id,
                    seq: seq.max(0) as u64,
                    kind,
                    snippet: make_snippet(&text, &needle),
                    match_count: 1,
                },
            );
        }
        order
            .into_iter()
            .filter_map(|id| hits.remove(&id))
            .collect()
    }
}

/// Escape LIKE metacharacters so user input matches literally.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The searchable prose of an event, with a short kind label.
fn event_search_text(event: &Event) -> Option<(&'static str, String)> {
    let (kind, text) = match event {
        Event::AgentMessageChunk { text, .. } => ("agent", text),
        Event::UserPromptSent { text, .. } => ("user", text),
        Event::UserDiffCommentsPrompt {
            assembled_markdown, ..
        } => ("user", assembled_markdown),
        Event::ToolCallContent { content, .. } | Event::ToolCallCompleted { content, .. } => {
            ("tool", content)
        }
        _ => return None,
    };
    (!text.trim().is_empty()).then(|| (kind, text.clone()))
}

fn make_snippet(text: &str, needle: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = collapsed.chars().collect();
    let lower = collapsed.to_lowercase();
    let match_char = lower
        .find(needle)
        .map(|byte_idx| lower[..byte_idx].chars().count())
        .unwrap_or(0);
    let start = match_char.saturating_sub(SNIPPET_RADIUS_CHARS);
    let end = (match_char + needle.chars().count() + SNIPPET_RADIUS_CHARS).min(chars.len());
    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(&chars[start..end]);
    if end < chars.len() {
        snippet.push('…');
    }
    snippet
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    fn ids(hits: &[ContentHit]) -> Vec<&str> {
        hits.iter().map(|h| h.session_id.as_str()).collect()
    }

    #[test]
    fn search_matches_prose_literally_and_groups_by_session() {
        let (_tmp, store) = open_store(1000);
        store
            .record("s1", 1, &user_prompt("please refactor the reconciler"))
            .unwrap();
        store
            .record("s2", 1, &agent_chunk("I updated the supervisor"))
            .unwrap();
        store
            .record(
                "s3",
                1,
                &Event::ToolCallContent {
                    tool_call_id: "t1".into(),
                    content: "grep matched reconciler in supervisor.rs".into(),
                },
            )
            .unwrap();
        record_from(
            &store,
            "s4",
            1,
            [
                agent_chunk("The Quick Brown Fox"),
                agent_chunk("quick again"),
            ],
        );

        let hits = store.search_content("reconciler", 10);
        assert!(ids(&hits).contains(&"s1") && ids(&hits).contains(&"s3"));
        assert!(!ids(&hits).contains(&"s2"));
        assert!(
            store.search_content("text", 10).is_empty(),
            "JSON keys are not prose"
        );
        assert!(store.search_content("e%", 10).is_empty(), "% is literal");

        let quick = store.search_content("QUICK", 10);
        assert_eq!(ids(&quick), ["s4"], "case-insensitive, one hit per session");
        assert_eq!(quick[0].match_count, 2);
    }

    #[test]
    fn search_limit_caps_distinct_sessions_not_raw_rows() {
        let (_tmp, store) = open_store(1000);
        record_from(
            &store,
            "s_busy",
            1,
            (1..=20).map(|_| agent_chunk("needle again")),
        );
        store.record("s_quiet", 1, &agent_chunk("needle")).unwrap();
        let all = store.search_content("needle", 10);
        assert!(ids(&all).contains(&"s_busy") && ids(&all).contains(&"s_quiet"));
        assert_eq!(store.search_content("needle", 1).len(), 1);
    }
}

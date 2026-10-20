//! `GET /api/system/logs`: the redacted tail of the active log file for the
//! Settings Logs tab. Secrets are stripped here, so raw values never reach the
//! browser.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, LazyLock};

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::logging;

use super::{api_error, cityhall_block, read_only_response, AppState};

const DEFAULT_LINES: usize = 500;
const MAX_LINES: usize = 2000;
/// Bounds the read when a single line is huge or rotation is disabled.
const MAX_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const REDACTED: &str = "[REDACTED]";

#[derive(Deserialize)]
pub struct LogsQuery {
    lines: Option<usize>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum LogsStatus {
    Available,
    Missing,
    Stdout,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogsResponse {
    status: LogsStatus,
    content: String,
    line_count: usize,
    truncated: bool,
}

impl LogsResponse {
    fn empty(status: LogsStatus) -> Self {
        Self {
            status,
            content: String::new(),
            line_count: 0,
            truncated: false,
        }
    }
}

pub async fn get_logs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LogsQuery>,
) -> Response {
    if state.read_only {
        return read_only_response();
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    let lines = query.lines.unwrap_or(DEFAULT_LINES);
    if lines == 0 || lines > MAX_LINES {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_lines",
            format!("lines must be between 1 and {MAX_LINES}"),
        );
    }

    let result = tokio::task::spawn_blocking(move || snapshot(lines)).await;
    let body = match result {
        Ok(Ok(body)) => body,
        _ => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "log_read_failed",
                "Could not read the log file",
            )
        }
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn snapshot(lines: usize) -> std::io::Result<LogsResponse> {
    let Some(path) = logging::active_log_path() else {
        return Ok(LogsResponse::empty(LogsStatus::Stdout));
    };
    let (tail, line_count, truncated) = match read_tail(&path, lines) {
        Ok(tail) => tail,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LogsResponse::empty(LogsStatus::Missing))
        }
        Err(e) => return Err(e),
    };
    Ok(LogsResponse {
        status: LogsStatus::Available,
        content: redact(&tail),
        line_count,
        truncated,
    })
}

/// Last `lines` lines of the file, reading at most `MAX_TAIL_BYTES`. Returns
/// the text, its line count, and whether older content was omitted.
fn read_tail(path: &Path, lines: usize) -> std::io::Result<(String, usize, bool)> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(len - start).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);

    let mut all: Vec<&str> = text.lines().collect();
    // A mid-file start lands inside a line; drop the fragment.
    if start > 0 && !all.is_empty() {
        all.remove(0);
    }
    let truncated = start > 0 || all.len() > lines;
    let kept = &all[all.len().saturating_sub(lines)..];
    let mut out = kept.join("\n");
    if !kept.is_empty() {
        out.push('\n');
    }
    Ok((out, kept.len(), truncated))
}

/// Each pattern keeps capture group 1 (the key) and replaces the value.
static REDACTORS: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        r#"(?i)((?:set-)?cookie["']?\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\r\n]+)"#,
        r#"(?i)(authorization["']?\s*[:=]\s*["']?)[^\r\n"']+"#,
        r#"(?i)((?:\w*token|passphrase|api[_-]?key)["']?\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s&"',;]+)"#,
    ]
    .map(|p| Regex::new(p).expect("static redaction pattern"))
});

fn redact(text: &str) -> String {
    let replacement = format!("${{1}}{REDACTED}");
    REDACTORS.iter().fold(text.to_string(), |acc, re| {
        re.replace_all(&acc, replacement.as_str()).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_secret_values_and_keeps_keys() {
        let cases = [
            (
                "url https://h/?token=abc123&x=1",
                "url https://h/?token=[REDACTED]&x=1",
            ),
            ("TOKEN=abc", "TOKEN=[REDACTED]"),
            ("passphrase=hunter2 next", "passphrase=[REDACTED] next"),
            (
                "passphrase = \"two words\" next",
                "passphrase = [REDACTED] next",
            ),
            (
                "Authorization: Bearer eyJ.abc.def",
                "Authorization: [REDACTED]",
            ),
            (
                "authorization: basic dXNlcjpwdw==",
                "authorization: [REDACTED]",
            ),
            ("Authorization: Token abc", "Authorization: [REDACTED]"),
            (
                "Authorization: Digest a=1, b=2",
                "Authorization: [REDACTED]",
            ),
            ("Authorization: abc123 next", "Authorization: [REDACTED]"),
            (
                r#"{"access_token":"s3cret","a":1}"#,
                r#"{"access_token":[REDACTED],"a":1}"#,
            ),
            ("api_key='k 1'", "api_key=[REDACTED]"),
            ("Cookie: sid=1; t=2", "Cookie: [REDACTED]"),
            (
                r#"{"authorization": "Bearer x", "cookie": "sid=1"}"#,
                r#"{"authorization": "[REDACTED]", "cookie": [REDACTED]}"#,
            ),
            (
                r#"authorization="Bearer x" ok"#,
                r#"authorization="[REDACTED]" ok"#,
            ),
            (
                "a token=1 and passphrase=2",
                "a token=[REDACTED] and passphrase=[REDACTED]",
            ),
            (
                "tokens=5 token_count=3 token=",
                "tokens=5 token_count=3 token=",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact(input), expected, "input: {input}");
        }
    }

    #[test]
    fn read_tail_keeps_last_lines_and_flags_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");

        std::fs::write(&path, "a\nb\nc").unwrap();
        assert_eq!(read_tail(&path, 2).unwrap(), ("b\nc\n".into(), 2, true));
        assert_eq!(read_tail(&path, 5).unwrap(), ("a\nb\nc\n".into(), 3, false));

        std::fs::write(&path, "").unwrap();
        assert_eq!(read_tail(&path, 5).unwrap(), (String::new(), 0, false));

        // Larger than the byte ceiling: the cut-through first line is dropped.
        let line = format!("{}\n", "x".repeat(99));
        let n = (MAX_TAIL_BYTES as usize / line.len()) + 10;
        std::fs::write(&path, line.repeat(n)).unwrap();
        let (text, count, truncated) = read_tail(&path, MAX_LINES).unwrap();
        assert!(truncated);
        assert_eq!(count, MAX_LINES);
        assert!(text.lines().all(|l| l.len() == 99));

        assert_eq!(
            read_tail(&dir.path().join("missing.log"), 1)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
}

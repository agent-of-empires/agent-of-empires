//! `GET /api/system/logs`: the redacted tail of the active log file for the
//! Settings Logs tab. Secrets are stripped here, so raw values never reach the
//! browser.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::logging::{self, ActiveSink};

use super::{api_error, cityhall_block, read_only_response, AppState};

const DEFAULT_LINES: usize = 500;
const MAX_LINES: usize = 2000;
/// Bounds the read when a single line is huge or rotation is disabled.
const MAX_TAIL_BYTES: u64 = 4 * 1024 * 1024;

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
    Unavailable,
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
    let path = match logging::active_sink() {
        Some(ActiveSink::File(path)) => path,
        Some(ActiveSink::Stdout) => return Ok(LogsResponse::empty(LogsStatus::Stdout)),
        None => return Ok(LogsResponse::empty(LogsStatus::Unavailable)),
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
        content: logging::redact(&tail),
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
    // Read one byte early so a cut that lands just after a newline keeps its first line.
    let seek = start.saturating_sub(1);
    file.seek(SeekFrom::Start(seek))?;
    let mut buf = Vec::new();
    file.take(len - seek).read_to_end(&mut buf)?;
    let at_line_start = start == 0 || buf[0] == b'\n';
    let body = if start == 0 { &buf[..] } else { &buf[1..] };
    let text = String::from_utf8_lossy(body);

    let mut all: Vec<&str> = text.lines().collect();
    if !at_line_start && !all.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;

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

        // A cut exactly on a line boundary keeps every line in the window.
        let line = format!("{}\n", "y".repeat(63));
        assert_eq!(MAX_TAIL_BYTES as usize % line.len(), 0);
        let n = MAX_TAIL_BYTES as usize / line.len() + 10;
        std::fs::write(&path, line.repeat(n)).unwrap();
        let (_, count, truncated) = read_tail(&path, usize::MAX).unwrap();
        assert!(truncated);
        assert_eq!(count, MAX_TAIL_BYTES as usize / line.len());

        assert_eq!(
            read_tail(&dir.path().join("missing.log"), 1)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
}

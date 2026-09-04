//! Session search.

use super::*;

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub seq: u64,
    pub kind: String,
    pub snippet: String,
    pub match_count: usize,
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchHit>,
}

/// Full-text search over session conversation content (#2515). Scans the
/// structured-view event store on its read-only connection and returns
/// one hit per matching session, newest first. The response carries only
/// the session id; the web client already holds the session list and
/// resolves the title and state from it. Read-only; allowed in
/// `--read-only` mode. Closed in CityHall mode: conversation content is
/// the same inspection surface as the gated pane and diff reads.
pub async fn search_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<SearchQuery>,
) -> axum::response::Response {
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    // Clamp like `read_output` does: an unbounded `?limit=` would drive an
    // unbounded SQLite scan and JSON decode on every keystroke.
    let limit = q.limit.unwrap_or(10).clamp(1, 100);
    // search_content does synchronous SQLite I/O plus JSON decoding; the
    // palette fires it repeatedly as the user types, so run it on the
    // blocking pool to keep slow scans off the Tokio worker threads.
    let store = Arc::clone(&state.acp_event_store);
    let query = q.q.clone();
    let results = tokio::task::spawn_blocking(move || {
        store
            .search_content(&query, limit)
            .into_iter()
            .map(|h| SearchHit {
                session_id: h.session_id,
                seq: h.seq,
                kind: h.kind.to_string(),
                snippet: h.snippet,
                match_count: h.match_count,
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    Json(SearchResponse { results }).into_response()
}

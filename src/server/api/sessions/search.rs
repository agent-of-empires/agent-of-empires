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

/// Full-text search over session conversation content (#2515), one hit per
/// matching session, newest first. The response carries only the session id; the
/// client resolves title and state from the list it already holds. Allowed in
/// `--read-only` mode, closed in CityHall: conversation content is the same
/// inspection surface as the gated pane and diff reads.
pub async fn search_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<SearchQuery>,
) -> axum::response::Response {
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    // Clamp like `read_output`: an unbounded `?limit=` would drive an
    // unbounded SQLite scan and JSON decode on every keystroke.
    let limit = q.limit.unwrap_or(10).clamp(1, 100);
    // search_content does synchronous SQLite I/O plus JSON decoding and the
    // palette fires it on every keystroke, so it runs on the blocking pool.
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

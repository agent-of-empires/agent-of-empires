//! Auto start: after a publish, starts the items of opted-in criteria through
//! `start_suggestion`, the same path a click takes.
//!
//! An item is started only when the Start button would need nothing from the
//! user: one linked project, no hooks review, no YOLO preset. Anything else stays a suggestion
//! with a note. Each item is attempted at most once; see `plugin::suggestions::auto`.
use super::*;

/// How the sweep logs and the note it saves begin.
const SKIPPED: &str = "Not auto started: ";

pub(crate) fn spawn(state: &Arc<AppState>) {
    let Some(host) = state.plugin_host.clone() else {
        return;
    };
    let state = state.clone();
    let shutdown = state.shutdown.clone();
    let runner = state.clone();
    runner
        .runtime
        .work
        .spawn("server.suggestions_auto_start", async move {
            loop {
                tokio::select! {
                    () = host.wait_suggestions_published() => sweep(&state).await,
                    () = shutdown.cancelled() => return,
                }
            }
        });
}

/// What Start would need from the user, as the note saying so; `Ok` when nothing.
fn needs_nothing(
    host: &crate::plugin::host::PluginHost,
    provider: &Provider,
    id: &str,
    profile: &str,
) -> Result<(), String> {
    let prefill = match prefill_for(host, provider, id) {
        Ok(Some(Ok(prefill))) => prefill,
        Ok(Some(Err(PrepareRefusal::Stale))) | Ok(None) => {
            return Err("it is no longer open".into())
        }
        Ok(Some(Err(PrepareRefusal::PromptTooLong))) => {
            return Err(format!(
                "its prompt exceeds {} bytes",
                crate::session::MAX_INITIAL_PROMPT_BYTES
            ))
        }
        Ok(Some(Err(PrepareRefusal::Registry(error)))) => {
            return Err(format!("the projects could not be read ({error})"))
        }
        Err(error) => return Err(error.to_string()),
    };
    let Some(path) = prefill["project"]["path"].as_str() else {
        return Err("its repo is not linked to a project".into());
    };
    if prefill["yolo"].as_bool().unwrap_or(false) {
        return Err("its preset runs the agent in YOLO mode, which needs your approval".into());
    }
    if prefill["extra_projects"]
        .as_array()
        .is_some_and(|extras| !extras.is_empty())
    {
        return Err("more than one project is linked to its repo".into());
    }
    let review = super::super::sessions::review_start_trust(profile, std::path::Path::new(path))
        .map_err(|error| error.to_string())?;
    if review.hooks_need_trust || review.mcp_need_trust {
        return Err("the project's hooks need a review first".into());
    }
    Ok(())
}

/// The error code and message of a refused or failed start.
async fn failure(response: axum::response::Response) -> (String, String) {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    (
        body["error"].as_str().unwrap_or_default().to_string(),
        body["message"].as_str().map_or_else(
            || format!("the Session could not be created ({status})"),
            String::from,
        ),
    )
}

async fn run<T: Send + 'static>(
    host: &Arc<crate::plugin::host::PluginHost>,
    f: impl FnOnce(&rusqlite::Connection, &str) -> T + Send + 'static,
) -> Option<T> {
    let host = host.clone();
    match tokio::task::spawn_blocking(move || host.with_suggestions_db(f)).await {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(target: "plugin.suggestions", %error, "auto start task failed");
            None
        }
    }
}

/// One pass over every provider with a criterion on auto start.
pub(crate) async fn sweep(state: &Arc<AppState>) {
    if state.read_only {
        return;
    }
    let Some(host) = state.plugin_host.clone() else {
        return;
    };
    let profile = host.suggestions_profile();
    let Some(Ok(enabled)) = run(&host, suggestions::auto_starts).await else {
        return;
    };
    for provider in suggestions::providers() {
        // Every push wakes a sweep and a started session can push again, so no cap would bind.
        if provider.id == suggestions::AGENT_PROVIDER_ID {
            continue;
        }
        let mut criteria: Vec<(String, u32)> = enabled
            .iter()
            .filter(|((provider_id, _), _)| *provider_id == provider.id)
            .map(|((_, criterion), cap)| (criterion.clone(), *cap))
            .collect();
        criteria.sort();
        let mut skips: Vec<(String, String)> = Vec::new();
        for (criterion, cap) in criteria {
            sweep_criterion(
                state,
                &host,
                &provider,
                &profile,
                (&criterion, cap),
                &mut skips,
            )
            .await;
        }
        let (provider_id, skips) = (provider.id.clone(), skips);
        run(&host, move |conn, profile| {
            suggestions::set_auto_skips(conn, profile, &provider_id, &skips)
        })
        .await;
    }
}

async fn sweep_criterion(
    state: &Arc<AppState>,
    host: &Arc<crate::plugin::host::PluginHost>,
    provider: &Provider,
    profile: &str,
    (criterion, cap): (&str, u32),
    skips: &mut Vec<(String, String)>,
) {
    let candidates = {
        let (provider_id, criterion) = (provider.id.clone(), criterion.to_string());
        run(host, move |conn, profile| {
            let provider = find_provider(&provider_id)?;
            suggestions::auto_candidates(conn, profile, &provider, &criterion).ok()
        })
        .await
        .flatten()
        .unwrap_or_default()
    };
    let mut attempts = 0;
    for item in candidates {
        let checked = {
            let (host, provider, id, profile) = (
                host.clone(),
                find_provider(&provider.id),
                item.id.clone(),
                profile.to_string(),
            );
            tokio::task::spawn_blocking(move || match provider {
                Some(provider) => needs_nothing(&host, &provider, &id, &profile),
                None => Err("its plugin is no longer active".into()),
            })
            .await
        };
        let skip = match checked {
            Ok(Ok(())) if attempts >= cap => {
                Some(format!("the limit of {cap} per refresh was reached"))
            }
            Ok(Ok(())) => None,
            Ok(Err(reason)) => Some(reason),
            Err(error) => Some(error.to_string()),
        };
        if let Some(reason) = skip {
            tracing::debug!(
                target: "plugin.suggestions",
                provider = %provider.id, item = %item.id, %reason, "auto start skipped"
            );
            skips.push((item.id, format!("{SKIPPED}{reason}")));
            continue;
        }
        // Recorded before the create: a crash or a failed create must not start it twice.
        let (provider_id, id) = (provider.id.clone(), item.id.clone());
        let claimed = run(host, move |conn, profile| {
            suggestions::claim_auto(conn, profile, &provider_id, &id).unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        if !claimed {
            continue;
        }
        attempts += 1;
        let response = start_suggestion(
            State(state.clone()),
            Ok(Json(StartBody {
                expected_profile: None,
                provider_id: provider.id.clone(),
                id: item.id.clone(),
                project: None,
                approve_hooks: false,
                review_fingerprint: None,
            })),
        )
        .await;
        if response.status() == StatusCode::CREATED {
            tracing::debug!(
                target: "plugin.suggestions",
                provider = %provider.id, item = %item.id, "auto started"
            );
            continue;
        }
        let (code, reason) = failure(response).await;
        // Started by hand between the check and the create.
        if matches!(code.as_str(), "already_started" | "stale_suggestion") {
            continue;
        }
        tracing::debug!(
            target: "plugin.suggestions",
            provider = %provider.id, item = %item.id, %reason, "auto start failed"
        );
        let (provider_id, id) = (provider.id.clone(), item.id.clone());
        let note = format!("Auto start failed: {reason}. Start it by hand.");
        run(host, move |conn, profile| {
            suggestions::fail_auto(conn, profile, &provider_id, &id, &note)
        })
        .await;
    }
}

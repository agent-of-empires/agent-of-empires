use super::*;

pub(crate) fn find_by_source_ref<'a>(
    instances: &'a [Instance],
    profile: &str,
    source_ref: &str,
) -> Option<&'a Instance> {
    instances.iter().find(|instance| {
        instance.source_profile == profile
            && instance
                .source_refs
                .iter()
                .any(|source| source == source_ref)
    })
}

fn suggestion_source_key(profile: &str, source_ref: &str) -> String {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    hash.update(profile.as_bytes());
    hash.update([0]);
    hash.update(source_ref.as_bytes());
    use std::fmt::Write;
    let mut key = String::with_capacity("suggestion-source:".len() + 64);
    key.push_str("suggestion-source:");
    for byte in hash.finalize() {
        write!(key, "{byte:02x}").expect("write to String");
    }
    key
}

fn suggestion_project_path_is_registered(
    requested: &std::path::Path,
    candidates: impl Iterator<Item = String>,
) -> bool {
    candidates.into_iter().any(|candidate| {
        std::fs::canonicalize(candidate).is_ok_and(|registered| registered == requested)
    })
}

pub(crate) async fn resolve_registered_suggestion_project(
    profile: &str,
    raw: &str,
) -> Result<std::path::PathBuf, String> {
    let path = super::rename::resolve_project_input(profile, raw).await?;
    let path = tokio::fs::canonicalize(path)
        .await
        .map_err(|error| format!("Suggestion project does not exist: {error}"))?;
    let profile = profile.to_owned();
    let requested = path.clone();
    let registered = tokio::task::spawn_blocking(move || {
        crate::session::projects::load_merged(&profile).map(|projects| {
            suggestion_project_path_is_registered(
                &requested,
                projects.into_iter().map(|project| project.path),
            )
        })
    })
    .await
    .map_err(|error| format!("Project registry lookup failed: {error}"))?
    .map_err(|error| format!("Project registry lookup failed: {error:#}"))?;
    if registered {
        Ok(path)
    } else {
        Err("Suggestion project is not an exact registered project".into())
    }
}

pub(super) async fn normalize_suggestion(
    state: &AppState,
    body: &mut CreateSessionBody,
) -> Result<(), axum::response::Response> {
    let Some(identity) = &body.suggestion else {
        return Ok(());
    };
    if state.cityhall_mode {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "cityhall_suggestions_disabled",
            "Suggestion sessions are unavailable in CityHall mode",
        ));
    }
    if body.scratch {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "suggestion_project_required",
            "A suggestion cannot create a scratch session",
        ));
    }
    let profile = body.profile.as_deref().unwrap_or(&state.profile);
    if let Some(response) = crate::server::api::unknown_profile_response(profile) {
        return Err(response);
    }
    let prepared = state
        .plugin_host
        .as_ref()
        .ok_or_else(|| {
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "Plugin host is unavailable",
            )
        })?
        .prepare_suggestion(profile, identity)
        .map_err(|error| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                error.to_string(),
            )
        })?
        .ok_or_else(|| {
            api_error(
                StatusCode::CONFLICT,
                "stale_suggestion",
                "Suggestion was retracted or changed; refresh before starting it",
            )
        })?;
    let project = if body.path.trim().is_empty() {
        prepared["project_path"].as_str()
    } else {
        Some(body.path.as_str())
    }
    .ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "suggestion_project_required",
            "Suggestion must map to an exact registered project",
        )
    })?;
    body.path = resolve_registered_suggestion_project(profile, project)
        .await
        .map_err(|message| {
            api_error(
                StatusCode::BAD_REQUEST,
                "suggestion_project_invalid",
                message,
            )
        })?
        .to_string_lossy()
        .into_owned();
    if body.title.as_deref().is_none_or(str::is_empty) {
        body.title = prepared["title"].as_str().map(str::to_owned);
    }
    if body.prompt.as_deref().is_none_or(str::is_empty) {
        body.prompt = prepared["prompt"].as_str().map(str::to_owned);
    }
    let source_ref = prepared["source_ref"].as_str().ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_suggestion",
            "Suggestion has no source reference",
        )
    })?;
    body.source_refs = vec![source_ref.to_owned()];
    if !body.create_another {
        body.idempotency_key = Some(suggestion_source_key(profile, source_ref));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestion_source_identity_is_exact_and_profile_scoped() {
        let mut personal = Instance::new("personal", "/tmp/project");
        personal.source_profile = "personal".into();
        personal.source_refs = vec!["https://github.com/org/repo/pull/1".into()];
        let mut work = personal.clone();
        work.id = "work-session".into();
        work.source_profile = "work".into();
        let instances = vec![personal, work];
        let source = "https://github.com/org/repo/pull/1";
        assert_eq!(
            find_by_source_ref(&instances, "work", source).unwrap().id,
            "work-session"
        );
        assert!(find_by_source_ref(&instances, "missing", source).is_none());
        assert!(
            find_by_source_ref(&instances, "work", "https://github.com/org/repo/pull/10").is_none()
        );
        assert_ne!(
            suggestion_source_key("personal", source),
            suggestion_source_key("work", source)
        );
    }

    #[test]
    fn suggestion_projects_require_exact_canonical_registered_paths() {
        let dir = tempfile::tempdir().unwrap();
        let registered = dir.path().join("registered");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&registered).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let requested = std::fs::canonicalize(&registered).unwrap();
        assert!(suggestion_project_path_is_registered(
            &requested,
            vec![registered.to_string_lossy().into_owned()].into_iter()
        ));
        assert!(!suggestion_project_path_is_registered(
            &requested,
            vec![other.to_string_lossy().into_owned()].into_iter()
        ));
    }
}

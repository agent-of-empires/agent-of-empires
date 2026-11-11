//! Worker RPC intake: validation of published items, checks and sources.

use super::*;

pub(crate) fn sources(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    spec: &SuggestionsSpec,
) -> Result<Value, DispatchError> {
    let overrides = overrides(conn, profile, provider_id).map_err(db_error)?;
    let sources: Vec<Value> = spec
        .sources
        .iter()
        .map(|s| json!({ "criterion": s.criterion, "enabled": effective_enabled(spec, &overrides, &s.criterion) }))
        .collect();
    Ok(json!({ "sources": sources }))
}

pub(crate) fn set(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    spec: &SuggestionsSpec,
    params: &Value,
) -> Result<Value, DispatchError> {
    set_at(
        conn,
        profile,
        provider_id,
        spec,
        params,
        chrono::Utc::now().timestamp(),
    )
}

fn set_at(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    spec: &SuggestionsSpec,
    params: &Value,
    now: i64,
) -> Result<Value, DispatchError> {
    let request: SetSuggestions = serde_json::from_value(params.clone()).map_err(|e| {
        DispatchError::invalid_params(format!("invalid suggestions.set params: {e}"))
    })?;
    let total = request.items.len();
    let overrides = overrides(conn, profile, provider_id).map_err(db_error)?;
    let accepted =
        validate_items(spec, &overrides, request.items).map_err(DispatchError::invalid_params)?;
    let first_stale: HashMap<String, i64> = conn
        .prepare(
            "SELECT item_id, since FROM suggestion_stale WHERE profile = ?1 AND provider_id = ?2",
        )
        .and_then(|mut q| {
            q.query_map(params![profile, provider_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect()
        })
        .map_err(db_error)?;
    // A stale item keeps the time it first went stale and expires after the TTL.
    // An expired item's time is kept too, so republishing it stale cannot start a new day.
    let mut stale_since: Vec<(&str, i64)> = Vec::new();
    let mut kept = Vec::with_capacity(accepted.len());
    for item in &accepted {
        if item.stale {
            let since = first_stale.get(&item.id).copied().unwrap_or(now);
            stale_since.push((item.id.as_str(), since));
            if now - since > STALE_TTL_SECS {
                continue;
            }
        }
        kept.push(item);
    }
    // `unchecked_`: the connection is only reachable as `&Connection` behind the host mutex.
    let tx = conn.unchecked_transaction().map_err(db_error)?;
    tx.execute(
        "DELETE FROM suggestion_stale WHERE profile = ?1 AND provider_id = ?2",
        params![profile, provider_id],
    )
    .map_err(db_error)?;
    for (id, since) in &stale_since {
        tx.execute(
            "INSERT INTO suggestion_stale (profile, provider_id, item_id, since) VALUES (?1, ?2, ?3, ?4)",
            params![profile, provider_id, id, since],
        )
        .map_err(db_error)?;
    }
    tx.execute(
        "DELETE FROM suggestion_items WHERE profile = ?1 AND provider_id = ?2",
        params![profile, provider_id],
    )
    .map_err(db_error)?;
    for item in &kept {
        let json = serde_json::to_string(item)
            .map_err(|e| DispatchError::internal(format!("encode suggestion: {e}")))?;
        tx.execute(
            "INSERT INTO suggestion_items (profile, provider_id, item_id, item_json) VALUES (?1, ?2, ?3, ?4)",
            params![profile, provider_id, item.id, json],
        )
        .map_err(db_error)?;
    }
    record_status(&tx, profile, provider_id, None).map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(json!({ "ok": true, "accepted": kept.len(), "dropped": total - kept.len() }))
}

pub(crate) fn error(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    params: &Value,
) -> Result<Value, DispatchError> {
    let request: SuggestionsError = serde_json::from_value(params.clone()).map_err(|e| {
        DispatchError::invalid_params(format!("invalid suggestions.error params: {e}"))
    })?;
    if request.message.len() > MAX_ERROR_BYTES {
        return Err(DispatchError::invalid_params(format!(
            "suggestions.error message is limited to {MAX_ERROR_BYTES} bytes"
        )));
    }
    if let Some(fix) = &request.fix {
        validate_fix(fix).map_err(DispatchError::invalid_params)?;
    }
    let tx = conn.unchecked_transaction().map_err(db_error)?;
    record_status(&tx, profile, provider_id, Some(&request.message)).map_err(db_error)?;
    if let Some(fix) = &request.fix {
        tx.execute(
            "INSERT INTO suggestion_fixes (profile, provider_id, label, method) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (profile, provider_id) DO UPDATE SET label = ?3, method = ?4",
            params![profile, provider_id, fix.label, fix.method],
        )
        .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)?;
    Ok(json!({ "ok": true }))
}

/// Replaces the provider's whole check list.
pub(crate) fn set_checks(
    conn: &Connection,
    profile: &str,
    provider_id: &str,
    params: &Value,
) -> Result<Value, DispatchError> {
    let request: SetChecks = serde_json::from_value(params.clone()).map_err(|e| {
        DispatchError::invalid_params(format!("invalid suggestions.checks.set params: {e}"))
    })?;
    validate_checks(&request.checks).map_err(DispatchError::invalid_params)?;
    let json = serde_json::to_string(&request.checks)
        .map_err(|e| DispatchError::internal(format!("encode checks: {e}")))?;
    conn.execute(
        "INSERT INTO suggestion_checks (profile, provider_id, checks_json) VALUES (?1, ?2, ?3)
         ON CONFLICT (profile, provider_id) DO UPDATE SET checks_json = ?3",
        params![profile, provider_id, json],
    )
    .map_err(db_error)?;
    Ok(json!({ "ok": true, "stored": request.checks.len() }))
}

fn validate_fix(fix: &SuggestionFix) -> Result<(), String> {
    if fix.label.trim().is_empty() || fix.label.len() > MAX_FIX_BYTES {
        return Err(format!(
            "fix label must be non-empty and at most {MAX_FIX_BYTES} bytes"
        ));
    }
    let name = fix.method.strip_prefix(SUGGESTIONS_FIX_PREFIX);
    let valid = fix.method.len() <= MAX_FIX_BYTES
        && name.is_some_and(|n| {
            !n.is_empty()
                && n.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        });
    if !valid {
        return Err(format!(
            "fix method must be {SUGGESTIONS_FIX_PREFIX}<name> of at most {MAX_FIX_BYTES} bytes, using a-z 0-9 . _ -"
        ));
    }
    Ok(())
}

fn validate_checks(checks: &[SetupCheck]) -> Result<(), String> {
    if checks.len() > MAX_CHECKS {
        return Err(format!(
            "suggestions.checks.set accepts at most {MAX_CHECKS} checks"
        ));
    }
    let mut ids = HashSet::new();
    for check in checks {
        let id = &check.id;
        if id.trim().is_empty() || check.label.trim().is_empty() {
            return Err(format!("check {id:?}: id and label must not be empty"));
        }
        let bounded = [
            ("id", id.len(), MAX_KEY_BYTES),
            ("label", check.label.len(), MAX_TITLE_BYTES),
            ("detail", check.detail.len(), MAX_CHECK_TEXT_BYTES),
            ("fix", check.fix.len(), MAX_CHECK_TEXT_BYTES),
        ];
        if let Some((name, _, max)) = bounded.iter().find(|(_, len, max)| len > max) {
            return Err(format!("check {id:?}: {name} is limited to {max} bytes"));
        }
        if !ids.insert(id.as_str()) {
            return Err(format!("duplicate check id {id:?}"));
        }
    }
    Ok(())
}

/// Returns the items of enabled criteria, or the first contract violation.
fn validate_items(
    spec: &SuggestionsSpec,
    overrides: &HashMap<String, bool>,
    items: Vec<Suggestion>,
) -> Result<Vec<Suggestion>, String> {
    if items.len() > MAX_ITEMS {
        return Err(format!("suggestions.set accepts at most {MAX_ITEMS} items"));
    }
    let mut ids = HashSet::new();
    for item in &items {
        validate_item(spec, item)?;
        if !ids.insert(item.id.as_str()) {
            return Err(format!("duplicate suggestion id {:?}", item.id));
        }
    }
    Ok(items
        .into_iter()
        .filter(|item| effective_enabled(spec, overrides, &item.criterion))
        .collect())
}

fn validate_item(spec: &SuggestionsSpec, item: &Suggestion) -> Result<(), String> {
    let id = &item.id;
    let source = spec
        .sources
        .iter()
        .find(|s| s.criterion == item.criterion)
        .ok_or_else(|| format!("undeclared suggestion criterion {:?}", item.criterion))?;
    for (name, value) in [
        ("id", id),
        ("title", &item.title),
        ("source_ref", &item.source_ref),
    ] {
        if value.trim().is_empty() {
            return Err(format!("suggestion {id:?}: {name} must not be empty"));
        }
    }
    let bounded = [
        ("id", id.len(), MAX_KEY_BYTES),
        ("title", item.title.len(), MAX_TITLE_BYTES),
        ("reason", item.reason.len(), MAX_REASON_BYTES),
        ("source_url", item.source_url.len(), MAX_URL_BYTES),
        ("source_ref", item.source_ref.len(), MAX_URL_BYTES),
        ("preview", item.preview.len(), MAX_PREVIEW_BYTES),
        ("draft.title", item.draft.title.len(), MAX_TITLE_BYTES),
        (
            "draft.prompt",
            item.draft.prompt.len(),
            MAX_INITIAL_PROMPT_BYTES,
        ),
        (
            "project",
            item.project.as_ref().map_or(0, String::len),
            MAX_PROJECT_BYTES,
        ),
    ];
    if let Some((name, _, max)) = bounded.iter().find(|(_, len, max)| len > max) {
        return Err(format!(
            "suggestion {id:?}: {name} is limited to {max} bytes"
        ));
    }
    if let Some(key) = &item.mapping_key {
        check_mapping_key(key).map_err(|e| format!("suggestion {id:?}: {e}"))?;
    }
    if !is_http_url(&item.source_url) {
        return Err(format!(
            "suggestion {id:?}: source_url must be an http(s) URL"
        ));
    }
    let declared: BTreeSet<&str> = source.variables.iter().map(String::as_str).collect();
    let given: BTreeSet<&str> = item.variables.keys().map(String::as_str).collect();
    if declared != given {
        return Err(format!(
            "suggestion {id:?}: variables must be exactly the ones source {:?} declares",
            source.criterion
        ));
    }
    if item
        .variables
        .values()
        .any(|v| v.len() > MAX_VARIABLE_VALUE_BYTES)
    {
        return Err(format!(
            "suggestion {id:?}: variable values are limited to {MAX_VARIABLE_VALUE_BYTES} bytes"
        ));
    }
    Ok(())
}

/// An http(s) URL whose displayed host is the host a browser opens: ASCII
/// only, and an authority that is non-empty, has no userinfo and no
/// percent-encoding (browsers decode it before IDNA), and names a host.
fn is_http_url(url: &str) -> bool {
    if !url.is_ascii() || url.contains(|c: char| c.is_whitespace() || c.is_control() || c == '\\') {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty() && !authority.contains(['@', '%']) && !authority.starts_with(':')
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn disabled_criteria_are_dropped_and_enabled_ones_kept() {
        let kept = validate_items(
            &spec(),
            &HashMap::new(),
            vec![item("a", "assigned"), item("q", "quiet")],
        )
        .unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "a");
    }

    #[test]
    fn contract_violations_are_rejected() {
        type Mutate = fn(&mut Suggestion);
        let cases: [(&str, Mutate); 22] = [
            ("undeclared", |i| i.criterion = "nope".into()),
            ("must not be empty", |i| i.title = " ".into()),
            ("http(s)", |i| i.source_url = "javascript:alert(1)".into()),
            ("http(s)", |i| i.source_url = "https://".into()),
            ("http(s)", |i| i.source_url = "https:///x".into()),
            ("http(s)", |i| i.source_url = "https://#f".into()),
            ("http(s)", |i| i.source_url = "https://:80/x".into()),
            ("http(s)", |i| {
                i.source_url = "https://real.com@evil.com/".into()
            }),
            ("http(s)", |i| {
                i.source_url = "https://exam\u{200b}ple.com/".into()
            }),
            ("http(s)", |i| {
                i.source_url = "https://%65xample.com/".into()
            }),
            ("http(s)", |i| {
                i.source_url = "https://exam%E2%80%8Bple.com/".into()
            }),
            ("http(s)", |i| i.source_url = "data:text/html,x".into()),
            ("http(s)", |i| i.source_url = "file:///etc/passwd".into()),
            ("http(s)", |i| i.source_url = "ftp://h/".into()),
            ("limited to 2048", |i| {
                i.source_url = format!("https://h/{}", "x".repeat(2040))
            }),
            ("limited to 256", |i| i.id = "x".repeat(257)),
            ("limited to 16384", |i| {
                i.preview = "x".repeat(16 * 1024 + 1)
            }),
            ("variables must be exactly", |i| i.variables.clear()),
            ("variables must be exactly", |i| {
                i.variables.insert("extra".into(), "v".into());
            }),
            ("mapping_key must not be empty", |i| {
                i.mapping_key = Some(" ".into())
            }),
            ("mapping_key is limited", |i| {
                i.mapping_key = Some("k".repeat(257))
            }),
            ("variable values", |i| {
                i.variables.insert("repo".into(), "x".repeat(2049));
            }),
        ];
        for (expected, mutate) in cases {
            let mut bad = item("a", "assigned");
            mutate(&mut bad);
            let err = validate_items(&spec(), &HashMap::new(), vec![bad]).unwrap_err();
            assert!(err.contains(expected), "{expected:?} not in {err:?}");
        }
        for url in ["HTTPS://example.com/x", "http://host:8080/p?q#f"] {
            let mut ok = item("a", "assigned");
            ok.source_url = url.into();
            assert!(validate_items(&spec(), &HashMap::new(), vec![ok]).is_ok());
        }
        let dup = validate_items(
            &spec(),
            &HashMap::new(),
            vec![item("a", "assigned"), item("a", "assigned")],
        );
        assert!(dup.unwrap_err().contains("duplicate"));
        let many = (0..=MAX_ITEMS)
            .map(|n| item(&n.to_string(), "assigned"))
            .collect();
        assert!(validate_items(&spec(), &HashMap::new(), many)
            .unwrap_err()
            .contains("at most 500"));
    }

    #[test]
    fn set_reports_counts_and_sources_report_defaults() {
        let conn = store();
        assert_eq!(
            publish(&conn, "p", &[item("a", "assigned"), item("q", "quiet")]),
            json!({ "ok": true, "accepted": 1, "dropped": 1 })
        );
        let bad = json!({ "items": [], "provider_id": "x" });
        assert!(set(&conn, "p", "acme.github", &spec(), &bad).is_err());
        assert_eq!(
            sources(&conn, "p", "acme.github", &spec()).unwrap(),
            json!({ "sources": [
                { "criterion": "assigned", "enabled": true },
                { "criterion": "quiet", "enabled": false },
            ]})
        );
    }

    #[test]
    fn profiles_are_isolated_and_status_tracks_the_last_report() {
        let conn = store();
        assert_eq!(snap(&conn, "p")["providers"][0]["status"], "pending");
        publish(&conn, "p", &[item("a", "assigned")]);
        assert_eq!(ids(&snap(&conn, "p")), ["a"]);
        assert!(ids(&snap(&conn, "other")).is_empty());
        assert_eq!(snap(&conn, "p")["providers"][0]["status"], "ok");

        error(
            &conn,
            "p",
            "acme.github",
            &json!({ "message": "rate limited" }),
        )
        .unwrap();
        let failed = snap(&conn, "p");
        assert_eq!(failed["providers"][0]["status"], "error");
        assert_eq!(failed["providers"][0]["error"], "rate limited");
        assert_eq!(ids(&failed), ["a"], "last-good items survive an error");

        publish(&conn, "p", &[]);
        let empty = snap(&conn, "p");
        assert_eq!(empty["providers"][0]["status"], "ok");
        assert!(empty["providers"][0].get("error").is_none());
        assert!(ids(&empty).is_empty(), "an empty publish clears items");
    }

    #[test]
    fn source_override_beats_default_and_hides_items_at_read() {
        let conn = store();
        publish(&conn, "p", &[item("a", "assigned")]);
        set_source(&conn, "p", "acme.github", "assigned", false).unwrap();
        assert!(ids(&snap(&conn, "p")).is_empty());
        assert_eq!(
            snap(&conn, "p")["providers"][0]["sources"][0]["enabled"],
            false
        );

        set_source(&conn, "p", "acme.github", "quiet", true).unwrap();
        let dropped = publish(&conn, "p", &[item("a", "assigned"), item("q", "quiet")]);
        assert_eq!(dropped["accepted"], 1);
        assert_eq!(ids(&snap(&conn, "p")), ["q"]);
    }

    #[test]
    fn checks_replace_per_provider_and_profile_and_default_to_empty() {
        let conn = store();
        assert_eq!(snap(&conn, "p")["providers"][0]["checks"], json!([]));
        let first = json!({ "checks": [check("cli", "ok"), check("auth", "fail")] });
        assert_eq!(
            set_checks(&conn, "p", "acme.github", &first).unwrap(),
            json!({ "ok": true, "stored": 2 })
        );
        assert_eq!(
            snap(&conn, "p")["providers"][0]["checks"],
            json!([check("cli", "ok"), check("auth", "fail")])
        );
        assert_eq!(snap(&conn, "other")["providers"][0]["checks"], json!([]));
        set_checks(
            &conn,
            "p",
            "acme.github",
            &json!({ "checks": [check("auth", "warn")] }),
        )
        .unwrap();
        assert_eq!(
            snap(&conn, "p")["providers"][0]["checks"],
            json!([check("auth", "warn")])
        );
        set_checks(&conn, "p", "acme.github", &json!({ "checks": [] })).unwrap();
        assert_eq!(snap(&conn, "p")["providers"][0]["checks"], json!([]));
    }

    #[test]
    fn undecodable_stored_checks_read_as_empty_instead_of_failing_the_snapshot() {
        let conn = store();
        conn.execute(
            "INSERT INTO suggestion_checks (profile, provider_id, checks_json) VALUES ('p', 'acme.github', 'not json')",
            [],
        )
        .unwrap();
        assert_eq!(snap(&conn, "p")["providers"][0]["checks"], json!([]));
    }

    #[test]
    fn invalid_checks_are_rejected_and_keep_the_previous_list() {
        let conn = store();
        let good = json!({ "checks": [check("cli", "ok")] });
        set_checks(&conn, "p", "acme.github", &good).unwrap();
        let long = |n| "x".repeat(n);
        let many: Vec<Value> = (0..=MAX_CHECKS)
            .map(|n| check(&n.to_string(), "ok"))
            .collect();
        let cases = [
            ("at most 32", json!({ "checks": many })),
            ("must not be empty", json!({ "checks": [check(" ", "ok")] })),
            (
                "duplicate",
                json!({ "checks": [check("a", "ok"), check("a", "ok")] }),
            ),
            (
                "limited to 256",
                json!({ "checks": [check(&long(257), "ok")] }),
            ),
            (
                "fix is limited to 2048",
                json!({ "checks": [{ "id": "a", "label": "A", "status": "ok", "fix": long(2049) }] }),
            ),
            ("invalid", json!({ "checks": [check("a", "maybe")] })),
        ];
        for (expected, bad) in cases {
            let err = set_checks(&conn, "p", "acme.github", &bad).unwrap_err();
            assert!(
                err.message.contains(expected),
                "{expected:?} not in {:?}",
                err.message
            );
        }
        assert_eq!(
            snap(&conn, "p")["providers"][0]["checks"],
            json!([check("cli", "ok")])
        );
    }

    #[test]
    fn error_fix_is_validated_listed_without_its_method_and_cleared() {
        let conn = store();
        let fixed = |fix: serde_json::Value| {
            error(
                &conn,
                "p",
                "acme.github",
                &json!({ "message": "bad key", "fix": fix }),
            )
        };
        for bad in [
            json!({ "label": "Fix", "method": "refresh" }),
            json!({ "label": "Fix", "method": "suggestions.fix." }),
            json!({ "label": "Fix", "method": "suggestions.fix.A b" }),
            json!({ "label": " ", "method": "suggestions.fix.key" }),
            json!({ "label": "x".repeat(MAX_FIX_BYTES + 1), "method": "suggestions.fix.key" }),
        ] {
            assert!(fixed(bad.clone()).is_err(), "{bad}");
        }
        assert!(fix_method(&conn, "p", "acme.github").unwrap().is_none());

        fixed(json!({ "label": "Fix in Linear settings", "method": "suggestions.fix.sign-in" }))
            .unwrap();
        assert_eq!(
            fix_method(&conn, "p", "acme.github").unwrap().as_deref(),
            Some("suggestions.fix.sign-in")
        );
        let mut agent = provider();
        agent.spec.uses_agent = true;
        let entry = snapshot(&conn, "p", &[agent]).unwrap()["providers"][0].clone();
        assert_eq!(entry["uses_agent"], true);
        assert_eq!(entry["fix"], json!({ "label": "Fix in Linear settings" }));

        error(&conn, "p", "acme.github", &json!({ "message": "again" })).unwrap();
        assert!(fix_method(&conn, "p", "acme.github").unwrap().is_none());
        fixed(json!({ "label": "Fix", "method": "suggestions.fix.sign-in" })).unwrap();
        set(&conn, "p", "acme.github", &spec(), &json!({ "items": [] })).unwrap();
        assert!(fix_method(&conn, "p", "acme.github").unwrap().is_none());
        let entry = snapshot(&conn, "p", &[provider()]).unwrap()["providers"][0].clone();
        assert!(entry.get("fix").is_none() && entry.get("uses_agent").is_none());
    }

    #[test]
    fn a_stale_item_keeps_its_first_time_and_expires_after_a_day() {
        let conn = store();
        let publish_at = |items: &[Suggestion], now: i64| {
            set_at(
                &conn,
                "p",
                "acme.github",
                &spec(),
                &json!({ "items": items }),
                now,
            )
            .unwrap()
        };
        let stale = |id: &str| Suggestion {
            stale: true,
            ..item(id, "assigned")
        };
        let day = STALE_TTL_SECS;
        publish_at(&[stale("a"), item("b", "assigned")], 1_000);
        // Republished stale later: the clock keeps running from the first time.
        let out = publish_at(&[stale("a"), item("b", "assigned")], 1_000 + day);
        assert_eq!(out["dropped"], 0, "exactly a day is still listed");
        assert_eq!(snap(&conn, "p")["items"][0]["stale"], true);
        assert!(snap(&conn, "p")["items"][1].get("stale").is_none());
        let out = publish_at(&[stale("a"), item("b", "assigned")], 1_001 + day);
        assert_eq!(out["dropped"], 1);
        assert_eq!(ids(&snap(&conn, "p")), ["b"]);
        // Still published stale after expiring: still dropped, not given a fresh day.
        for later in [2_000 + day, 1_000 + 3 * day] {
            let out = publish_at(&[stale("a"), item("b", "assigned")], later);
            assert_eq!(out["dropped"], 1, "at {later}");
        }
        // Fresh again, then stale again: a new clock starts.
        publish_at(&[item("a", "assigned")], 2_000 + day);
        let out = publish_at(&[stale("a")], 3_000 + day);
        assert_eq!(out["dropped"], 0);
    }

    #[test]
    fn error_message_is_bounded() {
        let conn = store();
        let long = json!({ "message": "x".repeat(MAX_ERROR_BYTES + 1) });
        assert!(error(&conn, "p", "acme.github", &json!({ "message": "boom" })).is_ok());
        assert!(error(&conn, "p", "acme.github", &long).is_err());
    }
}

//! Preferences, provider switches and the refresh interval.

use super::*;

/// Refresh intervals the Settings select offers, in minutes.
pub(crate) const REFRESH_MINUTES: [u32; 5] = [5, 15, 30, 60, 120];

/// Partial update of the profile's suggestion preferences.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefsPatch {
    pub homepage: Option<bool>,
    pub sidebar: Option<bool>,
    pub refresh_minutes: Option<u32>,
    pub start_without_review: Option<bool>,
    pub show_source_ref: Option<bool>,
    /// Every provider id exactly once.
    pub provider_order: Option<Vec<String>>,
    pub provider_enabled: Option<BTreeMap<String, bool>>,
}

/// `(enabled, sort_order)` per stored provider; absent means enabled, last.
pub(super) fn provider_prefs(
    conn: &Connection,
    profile: &str,
) -> rusqlite::Result<HashMap<String, (bool, i64)>> {
    conn.prepare(
        "SELECT provider_id, enabled, sort_order FROM suggestion_provider_prefs WHERE profile = ?1",
    )?
    .query_map([profile], |row| {
        Ok((row.get(0)?, (row.get(1)?, row.get(2)?)))
    })?
    .collect()
}

/// Ids of the providers the user switched off.
pub(crate) fn disabled_providers(
    conn: &Connection,
    profile: &str,
) -> rusqlite::Result<HashSet<String>> {
    conn.prepare(
        "SELECT provider_id FROM suggestion_provider_prefs WHERE profile = ?1 AND enabled = 0",
    )?
    .query_map([profile], |row| row.get(0))?
    .collect()
}

/// Provider indexes in the saved order; unsaved providers follow, ties broken by id.
pub(super) fn ordered_indexes(
    providers: &[Provider],
    stored: &HashMap<String, (bool, i64)>,
) -> Vec<usize> {
    let rank = |i: usize| {
        stored
            .get(&providers[i].id)
            .map_or(i64::from(i32::MAX), |p| p.1)
    };
    let mut order: Vec<usize> = (0..providers.len()).collect();
    order.sort_by(|&a, &b| (rank(a), &providers[a].id).cmp(&(rank(b), &providers[b].id)));
    order
}

pub(crate) const DEFAULT_REFRESH_MINUTES: u32 = 15;

/// `(homepage, sidebar, refresh_minutes, start_without_review, show_source_ref)`
/// of a profile that never saved any.
pub(crate) const DEFAULT_PREFS: Prefs = (true, false, DEFAULT_REFRESH_MINUTES, false, true);

pub(crate) type Prefs = (bool, bool, u32, bool, bool);

pub(crate) fn prefs_json(prefs: Prefs, provider_order: &[&str]) -> Value {
    json!({
        "homepage": prefs.0,
        "sidebar": prefs.1,
        "refresh_minutes": prefs.2,
        "start_without_review": prefs.3,
        "show_source_ref": prefs.4,
        "provider_order": provider_order,
    })
}

pub(super) fn load_prefs(conn: &Connection, profile: &str) -> rusqlite::Result<Prefs> {
    Ok(conn
        .query_row(
            "SELECT homepage, sidebar, refresh_minutes, start_without_review, show_source_ref FROM suggestion_preferences WHERE profile = ?1",
            [profile],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?
        .unwrap_or(DEFAULT_PREFS))
}

pub(crate) fn refresh_minutes(conn: &Connection, profile: &str) -> rusqlite::Result<u32> {
    load_prefs(conn, profile).map(|prefs| prefs.2)
}

/// Applies `patch` atomically; the inner `Err` is a validation message and
/// nothing is stored. The inner `Ok` says whether the stored refresh interval changed.
pub(crate) fn apply_prefs(
    conn: &Connection,
    profile: &str,
    known: &[String],
    patch: &PrefsPatch,
) -> anyhow::Result<Result<bool, String>> {
    if let Some(minutes) = patch.refresh_minutes {
        if !REFRESH_MINUTES.contains(&minutes) {
            return Ok(Err(format!(
                "refresh_minutes must be one of {REFRESH_MINUTES:?}"
            )));
        }
    }
    if let Some(order) = &patch.provider_order {
        let unique: HashSet<&String> = order.iter().collect();
        if unique.len() != order.len()
            || unique.len() != known.len()
            || !known.iter().all(|k| unique.contains(k))
        {
            return Ok(Err(
                "provider_order must list every provider exactly once".into()
            ));
        }
    }
    if let Some(enabled) = &patch.provider_enabled {
        if let Some(id) = enabled.keys().find(|id| !known.contains(id)) {
            return Ok(Err(format!("unknown provider {id:?}")));
        }
    }
    let refresh_changed = match patch.refresh_minutes {
        Some(minutes) => minutes != load_prefs(conn, profile)?.2,
        None => false,
    };
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO suggestion_preferences (profile) VALUES (?1)",
        [profile],
    )?;
    if let Some(v) = patch.homepage {
        tx.execute(
            "UPDATE suggestion_preferences SET homepage = ?2 WHERE profile = ?1",
            params![profile, v],
        )?;
    }
    if let Some(v) = patch.sidebar {
        tx.execute(
            "UPDATE suggestion_preferences SET sidebar = ?2 WHERE profile = ?1",
            params![profile, v],
        )?;
    }
    if let Some(v) = patch.refresh_minutes {
        tx.execute(
            "UPDATE suggestion_preferences SET refresh_minutes = ?2 WHERE profile = ?1",
            params![profile, v],
        )?;
    }
    if let Some(v) = patch.start_without_review {
        tx.execute(
            "UPDATE suggestion_preferences SET start_without_review = ?2 WHERE profile = ?1",
            params![profile, v],
        )?;
    }
    if let Some(v) = patch.show_source_ref {
        tx.execute(
            "UPDATE suggestion_preferences SET show_source_ref = ?2 WHERE profile = ?1",
            params![profile, v],
        )?;
    }
    for (position, id) in patch.provider_order.iter().flatten().enumerate() {
        tx.execute(
            "INSERT INTO suggestion_provider_prefs (profile, provider_id, sort_order) VALUES (?1, ?2, ?3)
             ON CONFLICT (profile, provider_id) DO UPDATE SET sort_order = ?3",
            params![profile, id, position as i64],
        )?;
    }
    for (id, enabled) in patch.provider_enabled.iter().flatten() {
        tx.execute(
            "INSERT INTO suggestion_provider_prefs (profile, provider_id, enabled) VALUES (?1, ?2, ?3)
             ON CONFLICT (profile, provider_id) DO UPDATE SET enabled = ?3",
            params![profile, id, enabled],
        )?;
    }
    tx.commit()?;
    Ok(Ok(refresh_changed))
}

/// Sets `refreshing` on every snapshot provider: asked to refresh (unix seconds in
/// `requested`) and not reported since, so an earlier publish does not end it.
pub(crate) fn attach_refreshing(snapshot: &mut Value, requested: &HashMap<String, i64>) {
    let Some(providers) = snapshot["providers"].as_array_mut() else {
        return;
    };
    for provider in providers {
        let updated = provider["updated_at"].as_i64();
        let is = provider["id"]
            .as_str()
            .and_then(|id| requested.get(id))
            .is_some_and(|&at| updated.is_none_or(|u| at > u));
        provider["refreshing"] = json!(is);
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn prefs_default_patch_partially_and_reject_bad_input() {
        let conn = store();
        let known = ["acme.github".to_string(), "acme.linear".to_string()];
        let other = || Provider {
            id: "acme.linear".into(),
            name: "Linear".into(),
            spec: spec(),
        };
        let both = [provider(), other()];
        let snap = |profile: &str| snapshot(&conn, profile, &both).unwrap();
        let apply = |patch: serde_json::Value| {
            apply_prefs(&conn, "p", &known, &serde_json::from_value(patch).unwrap()).unwrap()
        };

        assert_eq!(
            snap("p")["prefs"],
            json!({ "homepage": true, "sidebar": false, "refresh_minutes": 15,
                    "start_without_review": false, "show_source_ref": true,
                    "provider_order": ["acme.github", "acme.linear"] })
        );
        assert_eq!(
            snapshot(&conn, "p", &[]).unwrap()["prefs"],
            prefs_json(DEFAULT_PREFS, &[])
        );
        // Unsaved providers sort by id whatever the registry order.
        let reversed = [other(), provider()];
        assert_eq!(
            snapshot(&conn, "p", &reversed).unwrap()["prefs"]["provider_order"],
            json!(["acme.github", "acme.linear"])
        );

        assert_eq!(refresh_minutes(&conn, "p").unwrap(), 15);
        // Only a real change of the interval reports `true`.
        for (patch, changed) in [
            (json!({ "sidebar": true }), false),
            (json!({ "refresh_minutes": 15 }), false),
            (json!({ "refresh_minutes": 60 }), true),
            (json!({ "refresh_minutes": 60 }), false),
            (json!({ "homepage": false }), false),
        ] {
            assert_eq!(apply(patch.clone()), Ok(changed), "{patch}");
        }
        apply(json!({ "homepage": true })).unwrap();
        apply(json!({ "sidebar": true })).unwrap();
        for (field, value) in [("start_without_review", true), ("show_source_ref", false)] {
            apply(json!({ field: value })).unwrap();
            assert_eq!(snap("p")["prefs"][field], value, "{field}");
            assert_eq!(snap("q")["prefs"][field], !value, "{field}");
        }
        apply(json!({ "refresh_minutes": 60, "provider_order": ["acme.linear", "acme.github"] }))
            .unwrap();
        let prefs = snap("p")["prefs"].clone();
        assert_eq!(prefs["sidebar"], true);
        assert_eq!(prefs["homepage"], true);
        assert_eq!(prefs["refresh_minutes"], 60);
        assert_eq!(refresh_minutes(&conn, "p").unwrap(), 60);
        assert_eq!(refresh_minutes(&conn, "other").unwrap(), 15);
        assert_eq!(
            prefs["provider_order"],
            json!(["acme.linear", "acme.github"])
        );
        assert_eq!(snap("p")["providers"][0]["id"], "acme.linear");
        assert_eq!(snap("other")["prefs"]["sidebar"], false);

        for bad in [
            json!({ "refresh_minutes": 7 }),
            json!({ "provider_order": ["acme.github"] }),
            json!({ "provider_order": ["acme.github", "acme.github"] }),
            json!({ "provider_order": ["acme.github", "nope"] }),
            json!({ "provider_enabled": { "nope": false } }),
        ] {
            assert!(apply(bad.clone()).is_err(), "{bad}");
        }
        assert_eq!(snap("p")["prefs"]["refresh_minutes"], 60);
        assert!(serde_json::from_value::<PrefsPatch>(json!({ "extra": 1 })).is_err());
    }

    #[test]
    fn disabled_provider_stays_listed_but_hides_items_and_refresh() {
        let conn = store();
        publish(&conn, "p", &[item("a", "assigned")]);
        assert_eq!(ids(&snap(&conn, "p")), ["a"]);
        assert!(disabled_providers(&conn, "p").unwrap().is_empty());

        let patch = serde_json::from_value(json!({ "provider_enabled": { "acme.github": false } }))
            .unwrap();
        apply_prefs(&conn, "p", &["acme.github".to_string()], &patch)
            .unwrap()
            .unwrap();
        let snapshot = snap(&conn, "p");
        assert!(ids(&snapshot).is_empty());
        assert_eq!(snapshot["providers"][0]["enabled"], false);
        assert!(snapshot["providers"][0]["updated_at"].is_i64());
        assert_eq!(
            disabled_providers(&conn, "p").unwrap(),
            HashSet::from(["acme.github".to_string()])
        );
        assert!(disabled_providers(&conn, "other").unwrap().is_empty());
    }

    #[test]
    fn refreshing_marks_every_provider() {
        let mut snapshot = json!({ "providers": [
            { "id": "a" },
            { "id": "b", "updated_at": 90 },
            { "id": "c", "updated_at": 110 },
            { "id": "d", "updated_at": 100 },
            { "id": "e" },
        ]});
        let requested = HashMap::from(["b", "c", "d", "e"].map(|id| (id.to_string(), 100)));
        attach_refreshing(&mut snapshot, &requested);
        let flags: Vec<bool> = (0..5)
            .map(|i| snapshot["providers"][i]["refreshing"].as_bool().unwrap())
            .collect();
        // a never asked; b not heard since; c and d reported at or after the request; e never reported.
        assert_eq!(flags, [false, true, false, false, true]);
    }
}

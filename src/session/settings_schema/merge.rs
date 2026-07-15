//! Generic sparse-override merge.
//!
//! Profile and repo overrides are stored as a sparse JSON object keyed by
//! section then field. Merging applies them onto the serialized global config:
//! nested objects merge recursively; scalars and arrays replace wholesale
//! (matching the legacy hand-written merge, where e.g. `sandbox.extra_volumes`
//! replaces rather than extends). Because the merge is structural, adding a
//! config field never requires touching an override struct or a merge arm.

use serde_json::Value;

/// Merge `overrides` into `base` in place. Both are expected to be JSON
/// objects at the top level (sections). For each overridden leaf:
/// - object + object  -> recurse (so a single overridden field does not wipe
///   its siblings),
/// - anything else     -> the override value replaces the base value.
pub fn merge_json(base: &mut Value, overrides: &Value) {
    let (Value::Object(base_map), Value::Object(over_map)) = (&mut *base, overrides) else {
        // A non-object override replaces the base outright. Callers always
        // hand us objects; this keeps the function total.
        *base = overrides.clone();
        return;
    };

    for (key, over_val) in over_map {
        match base_map.get_mut(key) {
            Some(base_val) if base_val.is_object() && over_val.is_object() => {
                merge_json(base_val, over_val);
            }
            _ => {
                base_map.insert(key.clone(), over_val.clone());
            }
        }
    }
}

/// Apply onto `target` every leaf whose value differs between `baseline` and
/// `current`, leaving every other leaf of `target` untouched.
///
/// This is the "write back only what the user actually edited" counterpart to
/// [`merge_json`]. `baseline` is a snapshot taken when an editor opened,
/// `current` is that snapshot after the user's edits, and `target` is a fresh
/// load from disk that may already carry a concurrent writer's unrelated
/// changes. Only the leaves the user moved are written, so those concurrent
/// changes survive a save instead of being reverted by a stale whole-config
/// overwrite.
///
/// Unlike [`merge_json`], this honours removals: a key in `baseline` that is
/// absent from `current` is removed from `target` rather than ignored. That is
/// what makes an emptied collection save correctly. Fields serialized with
/// `skip_serializing_if` (`Config::environment`, `SessionConfig::tools`)
/// vanish from `current` when emptied rather than turning into `[]` or `{}`,
/// and only removing the key lets them deserialize back to their
/// `#[serde(default)]`. Writing `null` in their place would fail to
/// deserialize outright (`environment` goes through a string-or-seq visitor
/// that rejects null).
pub fn apply_changed_leaves(target: &mut Value, baseline: &Value, current: &Value) {
    let (Value::Object(base_map), Value::Object(cur_map)) = (baseline, current) else {
        if baseline != current {
            *target = current.clone();
        }
        return;
    };
    let Value::Object(target_map) = target else {
        // Nothing object-shaped to merge into leaf-wise; take the edit whole.
        if baseline != current {
            *target = current.clone();
        }
        return;
    };

    let keys: std::collections::BTreeSet<&String> = base_map.keys().chain(cur_map.keys()).collect();
    for key in keys {
        match (base_map.get(key), cur_map.get(key)) {
            // Untouched by the user: whatever `target` holds wins.
            (Some(base_val), Some(cur_val)) if base_val == cur_val => {}
            (Some(base_val), Some(cur_val)) if base_val.is_object() && cur_val.is_object() => {
                match target_map.get_mut(key) {
                    Some(target_val) if target_val.is_object() => {
                        apply_changed_leaves(target_val, base_val, cur_val);
                    }
                    // `target` has no object here to merge into, so build one
                    // from the changed leaves alone; the rest defaults, which
                    // is what an absent subtree meant anyway.
                    _ => {
                        let mut built = Value::Object(serde_json::Map::new());
                        apply_changed_leaves(&mut built, base_val, cur_val);
                        target_map.insert(key.clone(), built);
                    }
                }
            }
            (_, Some(cur_val)) => {
                target_map.insert(key.clone(), cur_val.clone());
            }
            // Present at open, gone after editing: an emptied
            // `skip_serializing_if` collection or a cleared `Option`.
            (Some(_), None) => {
                target_map.remove(key);
            }
            (None, None) => {}
        }
    }
}

/// Remove the value at `section.field` from a sparse override object, pruning
/// the section table if it becomes empty. Used when a PATCH sends `null` for a
/// field to clear a profile/repo override (revert to inheriting the global).
/// Returns true if anything was removed.
pub fn clear_path(overrides: &mut Value, section: &str, field: &str) -> bool {
    let Value::Object(root) = overrides else {
        return false;
    };
    let Some(Value::Object(section_map)) = root.get_mut(section) else {
        return false;
    };
    let removed = section_map.remove(field).is_some();
    if section_map.is_empty() {
        root.remove(section);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn override_leaf_keeps_siblings() {
        let mut base = json!({"acp": {"enabled": false, "default_agent": "aoe-agent"}});
        merge_json(&mut base, &json!({"acp": {"enabled": true}}));
        assert_eq!(
            base,
            json!({"acp": {"enabled": true, "default_agent": "aoe-agent"}})
        );
    }

    #[test]
    fn arrays_replace_not_extend() {
        let mut base = json!({"sandbox": {"extra_volumes": ["/a:/a"]}});
        merge_json(&mut base, &json!({"sandbox": {"extra_volumes": ["/b:/b"]}}));
        assert_eq!(base, json!({"sandbox": {"extra_volumes": ["/b:/b"]}}));
    }

    #[test]
    fn absent_key_inherits() {
        let mut base = json!({"acp": {"enabled": false, "max_concurrent_workers": 5}});
        merge_json(&mut base, &json!({"acp": {"enabled": true}}));
        assert_eq!(base["acp"]["max_concurrent_workers"], json!(5));
    }

    #[test]
    fn clear_removes_field_and_prunes_empty_section() {
        let mut overrides = json!({"acp": {"enabled": true}});
        assert!(clear_path(&mut overrides, "acp", "enabled"));
        assert_eq!(overrides, json!({}));
    }

    #[test]
    fn clear_keeps_other_fields() {
        let mut overrides = json!({"acp": {"enabled": true, "replay_bytes": 1024}});
        assert!(clear_path(&mut overrides, "acp", "enabled"));
        assert_eq!(overrides, json!({"acp": {"replay_bytes": 1024}}));
    }

    #[test]
    fn clear_missing_is_noop() {
        let mut overrides = json!({"acp": {"enabled": true}});
        assert!(!clear_path(&mut overrides, "sandbox", "cpu_limit"));
        assert_eq!(overrides, json!({"acp": {"enabled": true}}));
    }

    /// The whole point: a field another process changed while the editor was
    /// open must survive a save that did not touch it.
    #[test]
    fn concurrent_edit_to_untouched_field_survives() {
        let baseline = json!({"theme": {"name": "dark"}, "session": {"confirm_delete": false}});
        let current = json!({"theme": {"name": "light"}, "session": {"confirm_delete": false}});
        // Another process flipped `confirm_delete` after the editor opened.
        let mut target = json!({"theme": {"name": "dark"}, "session": {"confirm_delete": true}});

        apply_changed_leaves(&mut target, &baseline, &current);

        assert_eq!(
            target["theme"]["name"],
            json!("light"),
            "the user's edit lands"
        );
        assert_eq!(
            target["session"]["confirm_delete"],
            json!(true),
            "a concurrent writer's untouched field must not be reverted"
        );
    }

    /// An emptied `skip_serializing_if` collection disappears from `current`
    /// rather than serializing as `[]`, so the key must be removed (letting
    /// serde's `default` rebuild it) rather than left at its old value.
    #[test]
    fn emptied_skip_serializing_collection_is_removed() {
        let baseline = json!({"environment": ["A=1"], "default_profile": "work"});
        let current = json!({"default_profile": "work"});
        let mut target = json!({"environment": ["A=1"], "default_profile": "work"});

        apply_changed_leaves(&mut target, &baseline, &current);

        assert!(
            !target.as_object().unwrap().contains_key("environment"),
            "an emptied collection must be removed, not left stale: {target}"
        );
    }

    #[test]
    fn newly_set_optional_field_is_added() {
        let baseline = json!({"session": {}});
        let current = json!({"session": {"cpu_limit": "2"}});
        let mut target = json!({"session": {}});

        apply_changed_leaves(&mut target, &baseline, &current);
        assert_eq!(target["session"]["cpu_limit"], json!("2"));
    }

    #[test]
    fn changed_array_replaces_wholesale() {
        let baseline = json!({"sandbox": {"extra_volumes": ["/a:/a"]}});
        let current = json!({"sandbox": {"extra_volumes": ["/b:/b"]}});
        let mut target = json!({"sandbox": {"extra_volumes": ["/a:/a"]}});

        apply_changed_leaves(&mut target, &baseline, &current);
        assert_eq!(target["sandbox"]["extra_volumes"], json!(["/b:/b"]));
    }

    /// A save with no edits at all must not write anything, even when the
    /// on-disk copy has drifted from the snapshot.
    #[test]
    fn no_edits_leaves_target_untouched() {
        let baseline = json!({"theme": {"name": "dark"}});
        let current = baseline.clone();
        let mut target = json!({"theme": {"name": "light"}, "added_by_peer": true});
        let before = target.clone();

        apply_changed_leaves(&mut target, &baseline, &current);
        assert_eq!(target, before);
    }

    /// A key the running binary does not know about (written by a newer peer)
    /// lives only in `target`, never in the snapshots, so it must survive.
    #[test]
    fn key_only_in_target_survives() {
        let baseline = json!({"theme": {"name": "dark"}});
        let current = json!({"theme": {"name": "light"}});
        let mut target = json!({"theme": {"name": "dark", "future_key": 7}});

        apply_changed_leaves(&mut target, &baseline, &current);
        assert_eq!(target["theme"]["name"], json!("light"));
        assert_eq!(
            target["theme"]["future_key"],
            json!(7),
            "an unknown sibling key must not be dropped"
        );
    }
}

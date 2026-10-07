//! Structural merge of sparse JSON overrides: objects recurse, scalars and arrays replace.

use serde_json::Value;

pub fn merge_json(base: &mut Value, overrides: &Value) {
    let (Value::Object(base_map), Value::Object(over_map)) = (&mut *base, overrides) else {
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

/// Applies onto a fresh `target` only the leaves that changed from `baseline`
/// (editor open) to `current` (after edits), so concurrent writes to other
/// leaves survive. A key removed in `current` is removed from `target`: emptied
/// `skip_serializing_if` fields vanish rather than serialize, and `null` would
/// not deserialize.
pub fn apply_changed_leaves(target: &mut Value, baseline: &Value, current: &Value) {
    let (Value::Object(base_map), Value::Object(cur_map)) = (baseline, current) else {
        if baseline != current {
            *target = current.clone();
        }
        return;
    };
    let Value::Object(target_map) = target else {
        if baseline != current {
            *target = current.clone();
        }
        return;
    };

    let keys: std::collections::BTreeSet<&String> = base_map.keys().chain(cur_map.keys()).collect();
    for key in keys {
        match (base_map.get(key), cur_map.get(key)) {
            (Some(base_val), Some(cur_val)) if base_val == cur_val => {}
            (Some(base_val), Some(cur_val)) if base_val.is_object() && cur_val.is_object() => {
                match target_map.get_mut(key) {
                    Some(target_val) if target_val.is_object() => {
                        apply_changed_leaves(target_val, base_val, cur_val);
                    }
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
            (Some(_), None) => {
                target_map.remove(key);
            }
            (None, None) => {}
        }
    }
}

/// Clears an override, pruning an emptied section; true if anything was removed.
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
    fn merge_recurses_objects_and_replaces_arrays() {
        let mut base = json!({"acp": {"enabled": false, "default_agent": "aoe-agent"}, "sandbox": {"extra_volumes": ["/a:/a"]}});
        merge_json(
            &mut base,
            &json!({"acp": {"enabled": true}, "sandbox": {"extra_volumes": ["/b:/b"]}}),
        );
        assert_eq!(
            base,
            json!({"acp": {"enabled": true, "default_agent": "aoe-agent"}, "sandbox": {"extra_volumes": ["/b:/b"]}})
        );
    }

    #[test]
    fn clear_path_removes_field_and_prunes_empty_section() {
        let mut overrides = json!({"acp": {"enabled": true, "replay_events": 1024}});
        assert!(!clear_path(&mut overrides, "sandbox", "cpu_limit"));
        assert!(clear_path(&mut overrides, "acp", "enabled"));
        assert_eq!(overrides, json!({"acp": {"replay_events": 1024}}));
        assert!(clear_path(&mut overrides, "acp", "replay_events"));
        assert_eq!(overrides, json!({}));
    }

    #[test]
    fn apply_changed_leaves_writes_only_edits() {
        let cases = [
            (
                "concurrent edit to an untouched field survives",
                json!({"theme": {"name": "dark"}, "session": {"confirm_delete": false}}),
                json!({"theme": {"name": "light"}, "session": {"confirm_delete": false}}),
                json!({"theme": {"name": "dark"}, "session": {"confirm_delete": true}}),
                json!({"theme": {"name": "light"}, "session": {"confirm_delete": true}}),
            ),
            (
                "emptied skip_serializing collection is removed",
                json!({"environment": ["A=1"], "default_profile": "work"}),
                json!({"default_profile": "work"}),
                json!({"environment": ["A=1"], "default_profile": "work"}),
                json!({"default_profile": "work"}),
            ),
            (
                "newly set field is added",
                json!({"session": {}}),
                json!({"session": {"cpu_limit": "2"}}),
                json!({"session": {}}),
                json!({"session": {"cpu_limit": "2"}}),
            ),
            (
                "changed array replaces wholesale",
                json!({"sandbox": {"extra_volumes": ["/a:/a"]}}),
                json!({"sandbox": {"extra_volumes": ["/b:/b"]}}),
                json!({"sandbox": {"extra_volumes": ["/a:/a"]}}),
                json!({"sandbox": {"extra_volumes": ["/b:/b"]}}),
            ),
            (
                "no edits leave a drifted target untouched",
                json!({"theme": {"name": "dark"}}),
                json!({"theme": {"name": "dark"}}),
                json!({"theme": {"name": "light"}, "added_by_peer": true}),
                json!({"theme": {"name": "light"}, "added_by_peer": true}),
            ),
            (
                "unknown key from a newer peer survives",
                json!({"theme": {"name": "dark"}}),
                json!({"theme": {"name": "light"}}),
                json!({"theme": {"name": "dark", "future_key": 7}}),
                json!({"theme": {"name": "light", "future_key": 7}}),
            ),
        ];
        for (label, baseline, current, mut target, expected) in cases {
            apply_changed_leaves(&mut target, &baseline, &current);
            assert_eq!(target, expected, "{label}");
        }
    }
}

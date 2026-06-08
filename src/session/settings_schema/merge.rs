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
}

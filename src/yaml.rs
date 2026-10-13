//! Shared YAML parsing and serialization. Merge keys remain ordinary data until
//! the execution validator explicitly applies them.

pub(crate) fn from_str<'de, T: serde::Deserialize<'de>>(
    input: &'de str,
) -> Result<T, yaml_serde::Error> {
    yaml_serde::from_str(input)
}

pub(crate) fn from_slice<'de, T: serde::Deserialize<'de>>(
    input: &'de [u8],
) -> Result<T, yaml_serde::Error> {
    yaml_serde::from_slice(input)
}

/// The emitter disables line wrapping, keeping shell hook commands unfolded.
pub(crate) fn to_string<T: serde::Serialize>(value: &T) -> Result<String, yaml_serde::Error> {
    yaml_serde::to_string(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn parses_and_writes_yaml_with_explicit_merge_policy() {
        let input = "z: yes\na: on\nbase: &b {backend: docker}\nterminal:\n  <<: *b\n";
        let parsed: Value = from_str(input).unwrap();
        assert_eq!(
            parsed,
            json!({"z": "yes", "a": "on", "base": {"backend": "docker"},
                   "terminal": {"<<": {"backend": "docker"}}})
        );
        let keys: Vec<_> = parsed.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["z", "a", "base", "terminal"]);

        let mut merged: yaml_serde::Value = from_slice(input.as_bytes()).unwrap();
        merged.apply_merge().unwrap();
        assert_eq!(merged["terminal"]["backend"].as_str(), Some("docker"));
        assert!(merged["terminal"].get("<<").is_none());

        let command = format!(
            "sh -c 'case \"$X\" in *) {} ;; esac' # aoe",
            "a ".repeat(60)
        );
        let written = to_string(&json!({"command": command})).unwrap();
        assert_eq!(written.lines().count(), 1, "{written}");
        let reread: Value = from_str(&written).unwrap();
        assert_eq!(reread["command"], command);
    }

    #[test]
    fn typed_document_duplicate_policy_and_parse_diagnostics_are_preserved() {
        let input = "key: first\nkey: second\n";
        assert_eq!(from_str::<Value>(input).unwrap()["key"], "second");
        assert!(from_str::<yaml_serde::Value>(input).is_err());

        let error = from_str::<Value>("secret: PRIVATE_INPUT_SENTINEL\nbroken: [\n").unwrap_err();
        assert!(error.location().is_some());
        assert!(!error.to_string().contains("PRIVATE_INPUT_SENTINEL"));
        assert!(!format!("{error:?}").contains("PRIVATE_INPUT_SENTINEL"));
    }
}

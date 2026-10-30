//! YAML options matching the `serde_yaml` 0.9 behavior callers were written
//! against.

use serde_saphyr::{MergeKeyPolicy, Options, SerializerOptions};

/// YAML 1.2 booleans, ordinary merge keys, and diagnostics without input snippets.
pub(crate) fn options() -> Options {
    let mut options = Options::default();
    options.strict_booleans = true;
    options.merge_keys = MergeKeyPolicy::AsOrdinary;
    options.with_snippet = false;
    options
}

/// Serialize without folding long strings, so commands stay on one line.
pub(crate) fn to_string<T: serde::Serialize>(
    value: &T,
) -> Result<String, serde_saphyr::ser::Error> {
    let mut options = SerializerOptions::default();
    options.folded_wrap_chars = usize::MAX;
    serde_saphyr::to_string_with_options(value, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn parses_and_writes_like_serde_yaml() {
        let input = "z: yes\na: on\nbase: &b {backend: docker}\nterminal:\n  <<: *b\n";
        let parsed: Value = serde_saphyr::from_str_with_options(input, options()).unwrap();
        assert_eq!(
            parsed,
            json!({"z": "yes", "a": "on", "base": {"backend": "docker"},
                   "terminal": {"<<": {"backend": "docker"}}})
        );
        let keys: Vec<_> = parsed.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["z", "a", "base", "terminal"]);

        let mut merge_options = options();
        merge_options.merge_keys = MergeKeyPolicy::Merge;
        let merged: Value = serde_saphyr::from_str_with_options(input, merge_options).unwrap();
        assert_eq!(merged["terminal"], json!({"backend": "docker"}));

        let command = format!(
            "sh -c 'case \"$X\" in *) {} ;; esac' # aoe",
            "a ".repeat(60)
        );
        let written = to_string(&json!({"command": command})).unwrap();
        assert_eq!(written.lines().count(), 1, "{written}");
        let reread: Value = serde_saphyr::from_str_with_options(&written, options()).unwrap();
        assert_eq!(reread["command"], command);
    }

    #[test]
    fn typed_document_duplicate_policy_and_parse_diagnostics_are_preserved() {
        let input = "key: first\nkey: second\n";
        let mut last_wins = options();
        last_wins.duplicate_keys = serde_saphyr::DuplicateKeyPolicy::LastWins;
        assert_eq!(
            serde_saphyr::from_str_with_options::<Value>(input, last_wins).unwrap()["key"],
            "second"
        );
        let error = serde_saphyr::from_str_with_options::<Value>(
            "secret: PRIVATE_INPUT_SENTINEL\nbroken: [\n",
            options(),
        )
        .unwrap_err();
        assert!(!error.to_string().contains("PRIVATE_INPUT_SENTINEL"));
        assert!(!format!("{error:?}").contains("PRIVATE_INPUT_SENTINEL"));
    }
}

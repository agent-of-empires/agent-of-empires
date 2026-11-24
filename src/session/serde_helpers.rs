//! Shared serde helpers for config deserialization.

pub(crate) use aoe_daemon_client::string_or_vec;

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct TestRequired {
        #[serde(deserialize_with = "super::string_or_vec")]
        items: Vec<String>,
    }

    #[test]
    fn string_or_vec_accepts_a_string_or_an_array() {
        for (raw, want) in [
            (r#"items = "hello""#, vec!["hello"]),
            (r#"items = ["a", "b"]"#, vec!["a", "b"]),
        ] {
            let t: TestRequired = toml::from_str(raw).unwrap();
            assert_eq!(t.items, want);
        }
    }
}

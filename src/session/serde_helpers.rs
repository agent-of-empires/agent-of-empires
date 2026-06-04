//! Shared serde helpers for config deserialization.

use serde::de;

/// Deserialize a field as either a single string or a Vec of strings.
/// Allows `key = "value"` as shorthand for `key = ["value"]` in TOML.
pub(crate) fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: de::Deserializer<'de>,
{
    struct Visitor;

    impl<'de> de::Visitor<'de> for Visitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or array of strings")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<String>, E> {
            Ok(vec![value.to_string()])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut vec = Vec::new();
            while let Some(val) = seq.next_element()? {
                vec.push(val);
            }
            Ok(vec)
        }
    }

    deserializer.deserialize_any(Visitor)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct TestRequired {
        #[serde(deserialize_with = "super::string_or_vec")]
        items: Vec<String>,
    }

    #[test]
    fn string_or_vec_with_string() {
        let t: TestRequired = toml::from_str(r#"items = "hello""#).unwrap();
        assert_eq!(t.items, vec!["hello"]);
    }

    #[test]
    fn string_or_vec_with_array() {
        let t: TestRequired = toml::from_str(r#"items = ["a", "b"]"#).unwrap();
        assert_eq!(t.items, vec!["a", "b"]);
    }
}

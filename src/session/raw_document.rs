use anyhow::{Context, Result};
use serde::de::{MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

struct FieldName<'a>(Cow<'a, str>);

impl<'de> Deserialize<'de> for FieldName<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct NameVisitor;
        impl<'de> Visitor<'de> for NameVisitor {
            type Value = FieldName<'de>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object field name")
            }
            fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E> {
                Ok(FieldName(Cow::Borrowed(value)))
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(FieldName(Cow::Owned(value.to_owned())))
            }
            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
                Ok(FieldName(Cow::Owned(value)))
            }
        }
        deserializer.deserialize_identifier(NameVisitor)
    }
}

pub(crate) struct RawObject<'a> {
    fields: Vec<(FieldName<'a>, &'a RawValue)>,
}

impl<'de> Deserialize<'de> for RawObject<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = RawObject<'de>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut fields = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(name) = map.next_key()? {
                    fields.push((name, map.next_value()?));
                }
                Ok(RawObject { fields })
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

impl<'a> RawObject<'a> {
    pub(crate) fn parse(raw: &'a RawValue) -> Result<Self> {
        Ok(serde_json::from_str(raw.get())?)
    }

    pub(crate) fn unique(&self, name: &str) -> Result<Option<&'a RawValue>> {
        let mut matches = self.fields.iter().filter(|(field, _)| field.0 == name);
        let value = matches.next().map(|(_, value)| *value);
        anyhow::ensure!(matches.next().is_none(), "duplicate canonical field {name}");
        Ok(value)
    }

    pub(crate) fn values<'b>(&'b self, name: &'b str) -> impl Iterator<Item = &'a RawValue> + 'b {
        self.fields
            .iter()
            .filter(move |(field, _)| field.0 == name)
            .map(|(_, value)| *value)
    }

    pub(crate) fn strings<'b>(&'b self, name: &'b str) -> impl Iterator<Item = String> + 'b {
        self.values(name)
            .filter_map(|value| serde_json::from_str(value.get()).ok())
    }
}

pub(crate) enum Emission<'a> {
    Original(&'a RawValue),
    Changed(Box<RawValue>),
}

impl Serialize for Emission<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Original(raw) => raw.serialize(serializer),
            Self::Changed(raw) => raw.serialize(serializer),
        }
    }
}

struct ObjectEmission<'a>(Vec<(Cow<'a, str>, Emission<'a>)>);
impl Serialize for ObjectEmission<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

struct ArrayEmission<'a>(Vec<Emission<'a>>);
impl Serialize for ArrayEmission<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for value in &self.0 {
            seq.serialize_element(value)?;
        }
        seq.end()
    }
}

// Compare the typed before/after projection, not raw spelling or unknown fields.
pub(crate) fn patch<'a>(raw: &'a RawValue, before: &Value, after: &Value) -> Result<Emission<'a>> {
    patch_member(raw, before, after, None)
}

fn patch_member<'a>(
    raw: &'a RawValue,
    before: &Value,
    after: &Value,
    array_field: Option<&str>,
) -> Result<Emission<'a>> {
    if before == after {
        return Ok(Emission::Original(raw));
    }
    let changed = match (before, after) {
        (Value::Object(before), Value::Object(after)) => {
            let object = RawObject::parse(raw)?;
            let mut fields = Vec::with_capacity(object.fields.len());
            for (name, value) in &object.fields {
                let prior = before.get(name.0.as_ref());
                let next = after.get(name.0.as_ref());
                if prior == next {
                    fields.push((Cow::Borrowed(name.0.as_ref()), Emission::Original(value)));
                    continue;
                }
                object.unique(&name.0)?;
                if let Some(next) = next {
                    let value = match prior {
                        Some(prior) => patch_member(value, prior, next, Some(name.0.as_ref()))?,
                        None => Emission::Changed(serde_json::value::to_raw_value(next)?),
                    };
                    fields.push((Cow::Borrowed(name.0.as_ref()), value));
                }
            }
            for (name, value) in after {
                if before.get(name) != Some(value) && object.unique(name)?.is_none() {
                    fields.push((
                        Cow::Borrowed(name),
                        Emission::Changed(serde_json::value::to_raw_value(value)?),
                    ));
                }
            }
            serde_json::value::to_raw_value(&ObjectEmission(fields))?
        }
        (Value::Array(before), Value::Array(after)) => {
            let originals: Vec<&RawValue> = serde_json::from_str(raw.get())?;
            anyhow::ensure!(
                originals.len() == before.len(),
                "canonical array differs from its typed projection"
            );
            // Execution stores are atomic paths, not extension-bearing records.
            if array_field == Some("stores") && before.iter().chain(after).all(Value::is_string) {
                return Ok(Emission::Changed(serde_json::value::to_raw_value(after)?));
            }
            let mut consumed = vec![false; before.len()];
            let mut matched = 0;
            let mut inserted = false;
            let mut values = Vec::with_capacity(after.len());
            for value in after {
                let mut candidate = None;
                for (index, prior) in before.iter().enumerate() {
                    if prior == value || same_identity(prior, value, array_field) {
                        anyhow::ensure!(candidate.is_none(), "ambiguous canonical array member");
                        candidate = Some(index);
                    }
                }
                values.push(match candidate {
                    Some(index) => {
                        anyhow::ensure!(!consumed[index], "canonical array member reused");
                        consumed[index] = true;
                        matched += 1;
                        patch_member(originals[index], &before[index], value, array_field)?
                    }
                    None => {
                        inserted = true;
                        Emission::Changed(serde_json::value::to_raw_value(value)?)
                    }
                });
            }
            anyhow::ensure!(
                matched == before.len() || !inserted,
                "canonical array replacement has no unique survivor correspondence"
            );
            serde_json::value::to_raw_value(&ArrayEmission(values))?
        }
        _ => serde_json::value::to_raw_value(after)?,
    };
    Ok(Emission::Changed(changed))
}

fn same_identity(before: &Value, after: &Value, array_field: Option<&str>) -> bool {
    let keys: &[&str] = match array_field {
        // A repository's checkout path and branch are mutable transaction output.
        Some("repos") => &["name", "source_path", "main_repo_path"],
        // Lane acknowledgements change without replacing retirement evidence.
        Some("sandbox_content_resets") => &[
            "slot",
            "transaction",
            "tool",
            "agent",
            "roots",
            "recovery",
            "retired_terminal",
            "retired_terminal_binding",
            "retired_structured",
            "retired_import",
        ],
        Some("launches") => &[
            "nonce",
            "boot",
            "generation",
            "incarnation",
            "profile_identity",
        ],
        Some("preparations") => &["nonce", "boot", "generation"],
        Some("creations") => &[
            "format",
            "nonce",
            "session_id",
            "created_at",
            "generation",
            "boot",
            "commitment",
            "profile_identity",
        ],
        _ => {
            // An unclassified native record has no inferred nonce-only identity.
            if before.get("nonce").is_some() || after.get("nonce").is_some() {
                return false;
            }
            let mut identified = false;
            for key in ["id", "path", "name", "worktree_path"] {
                let prior = before.get(key);
                let next = after.get(key);
                if prior != next {
                    return false;
                }
                identified |= prior
                    .is_some_and(|value| value.as_str().is_some_and(|value| !value.is_empty()));
            }
            return identified;
        }
    };
    keys.iter().all(|key| {
        if array_field == Some("sandbox_content_resets") && *key == "retired_terminal_binding" {
            // This optional evidence is omitted by the typed projection when absent.
            return before.get(*key) == after.get(*key);
        }
        if array_field == Some("repos") {
            return before.get(*key).is_some_and(|prior| {
                prior.as_str().is_some_and(|value| !value.is_empty())
                    && after.get(*key) == Some(prior)
            });
        }
        if *key == "profile_identity" {
            return same_profile_stamp(before.get(*key), after.get(*key));
        }
        if array_field == Some("launches") && *key == "incarnation" {
            let prior = before.get(*key).unwrap_or(&Value::Null);
            let next = after.get(*key).unwrap_or(&Value::Null);
            // A provisional slot may gain its first birth, never a replacement.
            return prior == next
                || before.get(*key).is_some()
                    && prior.is_null()
                    && next.is_object()
                    && before.get("profile_identity").is_some_and(Value::is_object);
        }
        before.get(*key).is_some() && before.get(*key) == after.get(*key)
    })
}

fn same_profile_stamp(before: Option<&Value>, after: Option<&Value>) -> bool {
    let before = before.unwrap_or(&Value::Null);
    let after = after.unwrap_or(&Value::Null);
    if before == after {
        return true;
    }
    // Legacy quarantine adds no birth and retains the original weak dev/ino.
    if !after.is_object() || after.get("birth_time") != Some(&Value::Null) {
        return false;
    }
    let pair = match before {
        Value::Array(pair) if pair.len() == 2 => pair[0].as_u64().zip(pair[1].as_u64()),
        Value::Object(stamp) if stamp.get("birth_time").is_none_or(Value::is_null) => stamp
            .get("device")
            .and_then(Value::as_u64)
            .zip(stamp.get("inode").and_then(Value::as_u64)),
        _ => None,
    };
    pair.is_some_and(|(device, inode)| {
        after.get("device").and_then(Value::as_u64) == Some(device)
            && after.get("inode").and_then(Value::as_u64) == Some(inode)
    })
}

#[derive(Clone, Copy)]
pub(crate) struct OwnerSlot {
    pub(crate) index: usize,
    pub(crate) count: usize,
    pub(crate) ambiguous: bool,
}

pub(crate) struct RawDocument {
    pub(crate) rows: Vec<Box<RawValue>>,
}

impl RawDocument {
    pub(crate) fn parse(content: &str) -> Result<Self> {
        let rows = if content.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(content).context("canonical document is not an array")?
        };
        Ok(Self { rows })
    }

    pub(crate) fn owners(&self, field: &str) -> HashMap<String, OwnerSlot> {
        let mut owners = HashMap::new();
        for (index, raw) in self.rows.iter().enumerate() {
            let Ok(object) = RawObject::parse(raw) else {
                continue;
            };
            let ambiguous = object.unique(field).is_err();
            for id in object.strings(field) {
                let entry = owners.entry(id).or_insert(OwnerSlot {
                    index,
                    count: 0,
                    ambiguous,
                });
                entry.count += 1;
                entry.ambiguous |= ambiguous;
            }
        }
        owners
    }
}

pub(crate) struct RowDocument {
    pub(crate) raw: RawDocument,
    pub(crate) owners: HashMap<String, OwnerSlot>,
    before: Vec<Option<Value>>,
    field: &'static str,
}

impl RowDocument {
    pub(crate) fn project<T: serde::de::DeserializeOwned + Serialize>(
        raw: RawDocument,
        field: &'static str,
    ) -> Result<(Self, Vec<T>)> {
        let owners = raw.owners(field);
        let mut before = Vec::with_capacity(raw.rows.len());
        let mut decoded = Vec::new();
        for (index, row) in raw.rows.iter().enumerate() {
            let value = (|| -> Result<Option<T>> {
                let object = RawObject::parse(row)?;
                let key: FieldName<'_> = serde_json::from_str(
                    object
                        .unique(field)?
                        .context("missing owner identity")?
                        .get(),
                )?;
                let slot = owners.get(key.0.as_ref()).context("unindexed owner")?;
                if slot.count != 1 || slot.ambiguous {
                    return Ok(None);
                }
                Ok(Some(serde_json::from_str(row.get())?))
            })()
            .ok()
            .flatten();
            if let Some(value) = value {
                before.push(Some(serde_json::to_value(&value)?));
                decoded.push(value);
            } else {
                before.push(None);
            }
            debug_assert_eq!(before.len(), index + 1);
        }
        Ok((
            Self {
                raw,
                owners,
                before,
                field,
            },
            decoded,
        ))
    }

    pub(crate) fn admit(&self, id: &str) -> Result<()> {
        if let Some(slot) = self.owners.get(id) {
            anyhow::ensure!(
                slot.count == 1 && !slot.ambiguous,
                "ambiguous canonical {} {id}",
                self.field
            );
            anyhow::ensure!(
                self.before[slot.index].is_some(),
                "undecodable canonical {} {id}",
                self.field
            );
        }
        Ok(())
    }

    pub(crate) fn admit_all(&self) -> Result<()> {
        anyhow::ensure!(
            self.before.iter().all(Option::is_some),
            "canonical owner inventory contains opaque or ambiguous rows"
        );
        Ok(())
    }

    pub(crate) fn baseline(&self, id: &str) -> Option<&Value> {
        self.owners
            .get(id)
            .and_then(|slot| self.before[slot.index].as_ref())
    }

    pub(crate) fn compose_row<T: Serialize>(&self, id: &str, after: &T) -> Result<Emission<'_>> {
        self.admit(id)?;
        let slot = self
            .owners
            .get(id)
            .context("imported canonical owner disappeared")?;
        patch(
            &self.raw.rows[slot.index],
            self.before[slot.index]
                .as_ref()
                .context("opaque imported owner")?,
            &serde_json::to_value(after)?,
        )
    }

    pub(crate) fn render<T: Serialize>(
        &self,
        after: &[T],
        key: impl Fn(&T) -> &str,
        selected: Option<&std::collections::HashSet<&str>>,
        mut imports: HashMap<&str, Emission<'_>>,
    ) -> Result<Vec<u8>> {
        let mut ids = std::collections::HashSet::with_capacity(after.len());
        let mut changed = Vec::with_capacity(after.len());
        for row in after {
            let id = key(row);
            anyhow::ensure!(
                ids.insert(id),
                "duplicate emitted canonical {} {id}",
                self.field
            );
            self.admit(id)?;
            let value = serde_json::to_value(row)?;
            if let Some(slot) = self.owners.get(id) {
                let before = self.before[slot.index]
                    .as_ref()
                    .context("opaque emitted owner")?;
                anyhow::ensure!(
                    before == &value || selected.is_none_or(|selected| selected.contains(id)),
                    "write changed an unselected canonical {} {id}",
                    self.field
                );
                changed.push(patch(&self.raw.rows[slot.index], before, &value)?);
            } else {
                changed.push(match imports.remove(id) {
                    Some(imported) => imported,
                    None => Emission::Changed(serde_json::value::to_raw_value(&value)?),
                });
            }
        }
        for (id, slot) in &self.owners {
            if self.before[slot.index].is_some() && !ids.contains(id.as_str()) {
                anyhow::ensure!(
                    selected.is_none_or(|selected| selected.contains(id.as_str())),
                    "write removed an unselected canonical {} {id}",
                    self.field
                );
            }
        }
        let frozen = self.before.iter().filter(|row| row.is_none()).count();
        let mut emitted = Vec::with_capacity(frozen + after.len());
        let mut changed = changed.into_iter();
        for (index, raw) in self.raw.rows.iter().enumerate() {
            if self.before[index].is_none() {
                emitted.push(Emission::Original(raw));
            } else if let Some(row) = changed.next() {
                emitted.push(row);
            }
        }
        emitted.extend(changed);
        Ok(serde_json::to_vec_pretty(&ArrayEmission(emitted))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Serialize)]
    struct Row {
        id: String,
        items: Vec<Item>,
    }
    #[derive(Deserialize, Serialize)]
    struct Item {
        name: String,
        value: u32,
    }

    #[test]
    fn canonical_patches_keep_extensions_on_changed_and_reordered_members() -> Result<()> {
        let extension = r#"{"same":1,"same":2,"huge":1234567890123456789012345678901234567890,"float":1e400,"escaped":"\u0061"}"#;
        let raw = RawDocument::parse(&format!(
            r#"[{{"id":"owner","items":[{{"name":"first","value":1,"extension":{extension}}},{{"name":"second","value":2,"extension":{extension}}}]}}]"#
        ))?;
        let (document, mut rows) = RowDocument::project::<Row>(raw, "id")?;
        rows[0].items.swap(0, 1);
        rows[0].items[1].value = 3;
        let output = document.render(&rows, |row| row.id.as_str(), None, HashMap::new())?;
        let raw = RawDocument::parse(std::str::from_utf8(&output)?)?;
        let row = RawObject::parse(&raw.rows[0])?;
        let members: Vec<&RawValue> = serde_json::from_str(row.unique("items")?.unwrap().get())?;
        for member in &members {
            assert_eq!(
                RawObject::parse(member)?
                    .unique("extension")?
                    .unwrap()
                    .get(),
                extension
            );
        }
        let decoded: Vec<Row> = serde_json::from_slice(&output)?;
        assert_eq!(decoded[0].items[0].name, "second");
        assert_eq!(decoded[0].items[1].name, "first");
        assert_eq!(decoded[0].items[1].value, 3);
        Ok(())
    }
    fn emitted_array(raw: &str, before: &Value, after: &Value) -> Result<String> {
        let raw: Box<RawValue> = serde_json::from_str(raw)?;
        Ok(serde_json::to_string(&patch(&raw, before, after)?)?)
    }

    #[test]
    fn changed_arrays_refuse_ambiguous_or_reused_survivors() -> Result<()> {
        let raw = r#"[{"name":"same","value":1,"extension":"deleted"},{"name":"same","value":1,"extension":"survivor"}]"#;
        let same = serde_json::json!({"name":"same","value":1});
        let before = serde_json::json!([same, same]);
        for after in [
            serde_json::json!([same]),
            serde_json::json!([{"name":"same","value":2}]),
            serde_json::json!([same, same, {"name":"new","value":3}]),
        ] {
            assert!(emitted_array(raw, &before, &after).is_err());
        }
        assert_eq!(emitted_array(raw, &before, &before)?, raw);
        assert_eq!(emitted_array(raw, &before, &serde_json::json!([]))?, "[]");
        let one = serde_json::json!([same]);
        assert!(emitted_array(
            r#"[{"name":"same","value":1,"extension":"original"}]"#,
            &one,
            &serde_json::json!([same, same]),
        )
        .is_err());
        // An exact projection must not break a colliding identity tie either.
        assert!(emitted_array(
            r#"[{"name":"same","value":1},{"name":"same","value":2}]"#,
            &serde_json::json!([{"name":"same","value":1},{"name":"same","value":2}]),
            &one,
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn changed_arrays_distinguish_insertions_deletions_and_unidentified_edits() -> Result<()> {
        let raw = r#"[{"value":1,"extension":"first"},{"value":2,"extension":"second"}]"#;
        let before = serde_json::json!([{"value":1},{"value":2}]);
        let output = emitted_array(raw, &before, &serde_json::json!([{"value":2}]))?;
        assert_eq!(output, r#"[{"value":2,"extension":"second"}]"#);
        let output = emitted_array(
            raw,
            &before,
            &serde_json::json!([
                {"value":2},{"value":1},{"value":3}
            ]),
        )?;
        assert_eq!(
            output,
            r#"[{"value":2,"extension":"second"},{"value":1,"extension":"first"},{"value":3}]"#
        );
        for after in [
            serde_json::json!([{"value":1},{"value":3}]),
            serde_json::json!([{"value":3}]),
        ] {
            assert!(emitted_array(raw, &before, &after).is_err());
        }
        // No first identifier may override another identifying component.
        assert!(emitted_array(
            r#"[{"id":"shared","name":"a","value":1},{"id":"shared","name":"b","value":2}]"#,
            &serde_json::json!([{"id":"shared","name":"a","value":1},{"id":"shared","name":"b","value":2}]),
            &serde_json::json!([{"id":"shared","name":"unknown","value":3}]),
        ).is_err());
        Ok(())
    }

    #[test]
    fn field_specific_arrays_keep_native_and_reset_correspondence_closed() -> Result<()> {
        let opaque = r#"{"same":1,"same":2,"float":1e400,"escaped":"\u0061"}"#;
        let raw = format!(r#"{{"stores":["/old"],"boot":[1,2],"extension":{opaque}}}"#);
        let before = serde_json::json!({"stores":["/old"],"boot":[1,2]});
        let after = serde_json::json!({"stores":["/new"],"boot":[1,2]});
        let raw: Box<RawValue> = serde_json::from_str(&raw)?;
        let output = serde_json::to_string(&patch(&raw, &before, &after)?)?;
        let output: Box<RawValue> = serde_json::from_str(&output)?;
        assert_eq!(
            RawObject::parse(&output)?
                .unique("extension")?
                .unwrap()
                .get(),
            opaque
        );
        assert!(patch(
            &raw,
            &before,
            &serde_json::json!({"stores":["/new"],"boot":[3,4]})
        )
        .is_err());
        for field in ["unknown", "nonce", "commitment", "start", "namespace"] {
            let before = serde_json::json!({(field):["old"]});
            assert!(patch(
                &serde_json::value::to_raw_value(&before)?,
                &before,
                &serde_json::json!({(field):["new"]}),
            )
            .is_err());
        }
        let before = serde_json::json!({"stores":[{"value":1}]});
        assert!(patch(
            &serde_json::value::to_raw_value(&before)?,
            &before,
            &serde_json::json!({"stores":[{"value":2}]}),
        )
        .is_err());

        let reset_raw = format!(
            r#"{{"slot":"original","transaction":"transaction","tool":"claude","agent":"claude","roots":["/native"],"recovery":["/recovery"],"terminal":{{"pending":true,"generation":null}},"structured":{{"pending":true,"generation":null}},"retired_terminal":"old-terminal","retired_structured":["old-acp"],"retired_import":true,"extension":{opaque}}}"#
        );
        let reset: crate::migrations::v033_isolate_sandbox_content::SandboxContentReset =
            serde_json::from_str(&reset_raw)?;
        let reset = serde_json::to_value(reset)?;
        let before = serde_json::json!({"sandbox_content_resets":[reset]});
        let raw: Box<RawValue> =
            serde_json::from_str(&format!(r#"{{"sandbox_content_resets":[{reset_raw}]}}"#))?;
        let mut claimed = reset.clone();
        claimed["structured"]["generation"] = serde_json::json!(7);
        claimed["structured"]["pending"] = serde_json::json!(false);
        let output = serde_json::to_string(&patch(
            &raw,
            &before,
            &serde_json::json!({"sandbox_content_resets":[claimed]}),
        )?)?;
        let output: Box<RawValue> = serde_json::from_str(&output)?;
        let resets: Vec<&RawValue> = serde_json::from_str(
            RawObject::parse(&output)?
                .unique("sandbox_content_resets")?
                .unwrap()
                .get(),
        )?;
        assert_eq!(
            RawObject::parse(resets[0])?
                .unique("extension")?
                .unwrap()
                .get(),
            opaque
        );
        for component in [
            "slot",
            "transaction",
            "tool",
            "agent",
            "roots",
            "recovery",
            "retired_terminal",
            "retired_terminal_binding",
            "retired_structured",
            "retired_import",
        ] {
            let mut foreign = claimed.clone();
            foreign[component] = serde_json::json!("foreign");
            assert!(
                patch(
                    &raw,
                    &before,
                    &serde_json::json!({"sandbox_content_resets":[foreign]})
                )
                .is_err(),
                "{component}"
            );
        }
        let duplicate_raw: Box<RawValue> = serde_json::from_str(&format!(
            r#"{{"sandbox_content_resets":[{reset_raw},{reset_raw}]}}"#
        ))?;
        assert!(patch(
            &duplicate_raw,
            &serde_json::json!({"sandbox_content_resets":[reset,reset]}),
            &serde_json::json!({"sandbox_content_resets":[claimed]}),
        )
        .is_err());
        assert!(patch(
            &raw,
            &before,
            &serde_json::json!({"sandbox_content_resets":[claimed,claimed]})
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn workspace_repo_geometry_reorder_and_append_keep_original_extensions() -> Result<()> {
        use crate::session::WorkspaceRepo;
        let raw = r#"{"repos":[{"name":"shared","source_path":"/src/a","branch":"old","worktree_path":"/old/a","main_repo_path":"/src/a","managed_by_aoe":true,"extension":{"same":1,"same":2,"number":1e400}},{"name":"shared","source_path":"/src/b","branch":"old","worktree_path":"/old/b","main_repo_path":"/src/b","managed_by_aoe":true,"extension":"b"}]}"#;
        #[derive(Deserialize, Serialize)]
        struct Workspace {
            repos: Vec<WorkspaceRepo>,
        }
        let mut typed: Workspace = serde_json::from_str(raw)?;
        let before = serde_json::to_value(&typed)?;
        typed.repos.swap(0, 1);
        typed.repos[1].worktree_path = "/moved/a".into();
        typed.repos[1].branch = "renamed".into();
        typed.repos[0].base_branch_override = Some("main".into());
        let mut added = typed.repos[0].clone();
        added.name = "added".into();
        added.source_path = "/src/c".into();
        added.main_repo_path = "/src/c".into();
        added.worktree_path = "/new/c".into();
        typed.repos.push(added);
        let after = serde_json::to_value(&typed)?;
        let raw: Box<RawValue> = serde_json::from_str(raw)?;
        let output = serde_json::to_string(&patch(&raw, &before, &after)?)?;
        let output: Box<RawValue> = serde_json::from_str(&output)?;
        let object = RawObject::parse(&output)?;
        let repos: Vec<&RawValue> = serde_json::from_str(object.unique("repos")?.unwrap().get())?;
        assert_eq!(
            RawObject::parse(repos[0])?
                .unique("extension")?
                .unwrap()
                .get(),
            r#""b""#
        );
        assert_eq!(
            RawObject::parse(repos[1])?
                .unique("extension")?
                .unwrap()
                .get(),
            r#"{"same":1,"same":2,"number":1e400}"#
        );
        assert!(RawObject::parse(repos[2])?.unique("extension")?.is_none());
        let decoded: Workspace = serde_json::from_str(output.get())?;
        assert_eq!(decoded.repos, typed.repos);
        Ok(())
    }

    #[test]
    fn native_array_correspondence_uses_complete_original_tuples() -> Result<()> {
        let launch = serde_json::json!({
            "nonce":[1,2],"boot":[3,4],"generation":5,
            "incarnation":{"pid":6,"start":[7,8]},"profile_identity":{"dev":9,"ino":10,"btime":11},
            "stop_endpoint":null,"registry":null
        });
        let creation = serde_json::json!({
            "format":1,"nonce":"native","session_id":"session","created_at":"original",
            "generation":5,"boot":[3,4],"commitment":[12,13],
            "profile_identity":{"dev":9,"ino":10,"btime":11},
            "births":[],"effect_acknowledged":false
        });
        for (field, original, components) in [
            (
                "launches",
                launch,
                vec![
                    "nonce",
                    "boot",
                    "generation",
                    "incarnation",
                    "profile_identity",
                ],
            ),
            (
                "preparations",
                serde_json::json!({"nonce":[1,2],"boot":[3,4],"generation":5}),
                vec!["nonce", "boot", "generation"],
            ),
            (
                "creations",
                creation,
                vec![
                    "format",
                    "nonce",
                    "session_id",
                    "created_at",
                    "generation",
                    "boot",
                    "commitment",
                    "profile_identity",
                ],
            ),
        ] {
            let mut second = original.clone();
            second["generation"] = serde_json::json!(50);
            let before = serde_json::json!({(field): [original, second]});
            let mut raw = before.clone();
            raw[field][0]["extension"] = serde_json::json!("first");
            raw[field][1]["extension"] = serde_json::json!("second");
            let raw: Box<RawValue> = serde_json::value::to_raw_value(&raw)?;
            let mut after = serde_json::json!({(field): [second, original]});
            let status = if field == "creations" {
                "effect_acknowledged"
            } else {
                "registry"
            };
            if field != "preparations" {
                after[field][0][status] = serde_json::json!(true);
            }
            let output = serde_json::to_value(patch(&raw, &before, &after)?)?;
            assert_eq!(output[field][0]["extension"], "second");
            assert_eq!(output[field][1]["extension"], "first");
            for component in components {
                let mut replaced = original.clone();
                replaced[component] = serde_json::json!("replacement");
                assert!(
                    patch(&raw, &before, &serde_json::json!({(field):[replaced]})).is_err(),
                    "{field}.{component}"
                );
            }
            let mut duplicate_raw = serde_json::json!({(field):[original,original]});
            duplicate_raw[field][0]["extension"] = serde_json::json!("deleted");
            duplicate_raw[field][1]["extension"] = serde_json::json!("survivor");
            let duplicate_raw = serde_json::value::to_raw_value(&duplicate_raw)?;
            assert!(patch(
                &duplicate_raw,
                &serde_json::json!({(field):[original,original]}),
                &serde_json::json!({(field):[original]})
            )
            .is_err());
        }
        Ok(())
    }

    #[test]
    fn provisional_native_slot_accepts_only_its_unique_first_birth() -> Result<()> {
        let pending = serde_json::json!({
            "nonce":[1,2],"boot":[3,4],"generation":5,"incarnation":null,
            "profile_identity":{"dev":6,"ino":7,"btime":8},
            "stop_endpoint":null,"registry":null
        });
        let mut actual = pending.clone();
        actual["incarnation"] =
            serde_json::json!({"pid":9,"start":[10,11],"group":9,"namespace":[12,13]});
        let before = serde_json::json!({"launches":[pending]});
        let mut raw = before.clone();
        raw["launches"][0]["extension"] = serde_json::json!("original");
        let raw = serde_json::value::to_raw_value(&raw)?;
        let output = serde_json::to_value(patch(
            &raw,
            &before,
            &serde_json::json!({"launches":[actual]}),
        )?)?;
        assert_eq!(output["launches"][0]["extension"], "original");
        assert_eq!(output["launches"][0]["incarnation"], actual["incarnation"]);
        for component in ["nonce", "boot", "generation", "profile_identity"] {
            let mut foreign = actual.clone();
            foreign[component] = serde_json::json!("foreign");
            assert!(patch(&raw, &before, &serde_json::json!({"launches":[foreign]})).is_err());
        }
        for raw_before in [
            serde_json::json!({"launches":[pending,pending]}),
            serde_json::json!({"launches":[pending,actual]}),
        ] {
            let raw_before_bytes = serde_json::value::to_raw_value(&raw_before)?;
            assert!(patch(
                &raw_before_bytes,
                &raw_before,
                &serde_json::json!({"launches":[actual]})
            )
            .is_err());
        }
        let born = serde_json::json!({"launches":[actual]});
        let born_bytes = serde_json::value::to_raw_value(&born)?;
        let mut replacement = actual.clone();
        replacement["incarnation"]["start"] = serde_json::json!([100, 101]);
        for invalid in [pending, replacement] {
            assert!(patch(
                &born_bytes,
                &born,
                &serde_json::json!({"launches":[invalid]})
            )
            .is_err());
        }
        // Reordering an already-born record alongside its distinct pending slot is safe.
        let mut other_pending = actual.clone();
        other_pending["nonce"] = serde_json::json!([20, 21]);
        other_pending["incarnation"] = Value::Null;
        let mixed_before = serde_json::json!({"launches":[actual,other_pending]});
        let mut mixed_raw = mixed_before.clone();
        mixed_raw["launches"][0]["extension"] = serde_json::json!("born");
        mixed_raw["launches"][1]["extension"] = serde_json::json!("pending");
        let mut other_actual = other_pending;
        other_actual["incarnation"] =
            serde_json::json!({"pid":22,"start":[23,24],"group":22,"namespace":[25,26]});
        let output = serde_json::to_value(patch(
            &serde_json::value::to_raw_value(&mixed_raw)?,
            &mixed_before,
            &serde_json::json!({"launches":[other_actual,actual]}),
        )?)?;
        assert_eq!(output["launches"][0]["extension"], "pending");
        assert_eq!(output["launches"][1]["extension"], "born");
        Ok(())
    }

    #[test]
    fn workspace_import_composition_handles_moves_and_refuses_ambiguous_survivors() -> Result<()> {
        #[derive(Deserialize, Serialize)]
        struct Owner {
            id: String,
            project_path: String,
            repos: Vec<crate::session::WorkspaceRepo>,
        }
        let row = r#"{"id":"owner","project_path":"/old","repos":[{"name":"repo","source_path":"/source","main_repo_path":"/source","worktree_path":"/old/repo","branch":"old","managed_by_aoe":false,"extension":"first"}]}"#;
        let (source, mut rows) =
            RowDocument::project::<Owner>(RawDocument::parse(&format!("[{row}]"))?, "id")?;
        rows[0].project_path = "/moved".into();
        rows[0].repos[0].worktree_path = "/moved/repo".into();
        rows[0].repos[0].branch = "renamed".into();
        let imported = source.compose_row("owner", &rows[0])?;
        let (target, _) = RowDocument::project::<Owner>(RawDocument::parse("[]")?, "id")?;
        let output = target.render(
            &rows,
            |row| row.id.as_str(),
            None,
            HashMap::from([("owner", imported)]),
        )?;
        let raw = RawDocument::parse(std::str::from_utf8(&output)?)?;
        let object = RawObject::parse(&raw.rows[0])?;
        let repos: Vec<&RawValue> = serde_json::from_str(object.unique("repos")?.unwrap().get())?;
        assert_eq!(
            RawObject::parse(repos[0])?
                .unique("extension")?
                .unwrap()
                .get(),
            r#""first""#
        );
        let decoded: Vec<Owner> = serde_json::from_slice(&output)?;
        assert_eq!(decoded[0].project_path, "/moved");
        assert_eq!(decoded[0].repos[0].worktree_path, "/moved/repo");
        // Construct the duplicate raw members without deserializing their extensions.
        let owner_object: Box<RawValue> = serde_json::from_str(row)?;
        let owner_object = RawObject::parse(&owner_object)?;
        let members: Vec<&RawValue> =
            serde_json::from_str(owner_object.unique("repos")?.unwrap().get())?;
        let duplicated = format!(
            r#"[{{"id":"owner","project_path":"/old","repos":[{},{}]}}]"#,
            members[0].get(),
            members[0].get().replace("first", "second")
        );
        let (source, mut rows) =
            RowDocument::project::<Owner>(RawDocument::parse(&duplicated)?, "id")?;
        rows[0].repos.remove(0);
        assert!(source.compose_row("owner", &rows[0]).is_err());
        assert_eq!(
            source.raw.rows[0].get(),
            &duplicated[1..duplicated.len() - 1]
        );
        Ok(())
    }
}

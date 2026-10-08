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
                        Some(prior) => patch(value, prior, next)?,
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
            let mut values = Vec::with_capacity(after.len());
            for (index, value) in after.iter().enumerate() {
                if before.get(index) == Some(value) {
                    values.push(Emission::Original(originals[index]));
                } else if let Some(prior) = before.iter().position(|prior| prior == value) {
                    values.push(Emission::Original(originals[prior]));
                } else if let Some(prior) = before
                    .get(index)
                    .filter(|prior| same_identity(prior, value))
                {
                    values.push(patch(originals[index], prior, value)?);
                } else if let Some((index, prior)) = before
                    .iter()
                    .enumerate()
                    .find(|(_, prior)| same_identity(prior, value))
                {
                    values.push(patch(originals[index], prior, value)?);
                } else {
                    values.push(Emission::Changed(serde_json::value::to_raw_value(value)?));
                }
            }
            serde_json::value::to_raw_value(&ArrayEmission(values))?
        }
        _ => serde_json::value::to_raw_value(after)?,
    };
    Ok(Emission::Changed(changed))
}

fn same_identity(before: &Value, after: &Value) -> bool {
    for key in ["id", "nonce", "path", "name", "worktree_path"] {
        if let Some(value) = before.get(key) {
            return after.get(key) == Some(value);
        }
    }
    false
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
}

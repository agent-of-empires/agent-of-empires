use anyhow::{Context, Result};
use serde::de::{MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

use super::runner_journal::{NativeBirthKey, NativeLaunchPhase};
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
    pub(crate) fn sandbox_enabled(&self) -> Result<bool> {
        let Some(sandbox) = self.unique("sandbox_info")? else {
            return Ok(false);
        };
        let Some(sandbox) = serde_json::from_str::<Option<&RawValue>>(sandbox.get())? else {
            return Ok(false);
        };
        let sandbox = Self::parse(sandbox)?;
        match sandbox.unique("enabled")? {
            Some(enabled) => Ok(serde_json::from_str(enabled.get())?),
            None => Ok(false),
        }
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
    patch_member(raw, before, after, None, None)
}

pub(crate) fn patch_with_no_target_retirement<'a>(
    raw: &'a RawValue,
    before: &Value,
    after: &Value,
    receipt: &super::runner_journal::OriginalNoTargetReceipt,
) -> Result<Emission<'a>> {
    anyhow::ensure!(
        before.get("id").and_then(Value::as_str) == Some(receipt.original().session_id())
            && before.get("id") == after.get("id")
            && before.get("created_at") == after.get("created_at"),
        "no-target raw writer changed its original owner"
    );
    patch_member(raw, before, after, None, Some(receipt))
}

fn native_field_projection(raw: &RawValue, key: &str) -> Result<Value> {
    use super::runner_journal::RegistryWitness;
    let value = match key {
        "nonce" | "boot" => serde_json::to_value(serde_json::from_str::<[u8; 16]>(raw.get())?)?,
        "generation" => serde_json::to_value(serde_json::from_str::<u64>(raw.get())?)?,
        "incarnation" => serde_json::to_value(serde_json::from_str::<
            Option<crate::process::ProcessIncarnation>,
        >(raw.get())?)?,
        "profile_identity" => serde_json::to_value(serde_json::from_str::<
            Option<super::DirectoryIdentity>,
        >(raw.get())?)?,
        "phase" => serde_json::to_value(serde_json::from_str::<NativeLaunchPhase>(raw.get())?)?,
        "stop_endpoint" => serde_json::to_value(serde_json::from_str::<
            Option<crate::process::worker_registry::SocketEndpointIdentity>,
        >(raw.get())?)?,
        "registry" => {
            serde_json::to_value(serde_json::from_str::<Option<RegistryWitness>>(raw.get())?)?
        }
        _ => anyhow::bail!("unclassified native canonical field {key}"),
    };
    Ok(value)
}

fn native_phase(value: &Value) -> Result<NativeLaunchPhase> {
    serde_json::from_value(
        value
            .get("phase")
            .context("native launch has no explicit phase")?
            .clone(),
    )
    .context("native launch has an invalid phase")
}

fn native_slot_matches(before: &Value, after: &Value) -> bool {
    ["nonce", "boot", "generation", "profile_identity"]
        .iter()
        .all(|key| before.get(*key).is_some() && before.get(*key) == after.get(*key))
}

fn same_native_launch_identity(before: &Value, after: &Value) -> bool {
    if !native_slot_matches(before, after) {
        return false;
    }
    let Ok(next_phase) = native_phase(after) else {
        return false;
    };
    let prior_incarnation = before.get("incarnation");
    let next_incarnation = after.get("incarnation");
    if prior_incarnation.is_none() || next_incarnation.is_none() {
        return false;
    }
    let Some(prior_phase) = before.get("phase") else {
        return next_phase
            == (NativeLaunchPhase::Unresolved {
                may_authorize: true,
            })
            && prior_incarnation == next_incarnation
            && serde_json::from_value::<NativeBirthKey>(before.clone()).is_ok();
    };
    let Ok(prior_phase) = serde_json::from_value::<NativeLaunchPhase>(prior_phase.clone()) else {
        return false;
    };
    if !prior_phase.allows_transition_to(next_phase) {
        return false;
    }
    prior_incarnation == next_incarnation
        || prior_phase == NativeLaunchPhase::Natal
            && matches!(
                next_phase,
                NativeLaunchPhase::Armed
                    | NativeLaunchPhase::Unresolved {
                        may_authorize: false
                    }
            )
            && prior_incarnation.is_some_and(Value::is_null)
            && next_incarnation.is_some_and(Value::is_object)
            && before.get("profile_identity").is_some_and(Value::is_object)
}

fn validate_native_launch_patch(
    raw: &RawValue,
    before: &[Value],
    after: &[Value],
    retirement: Option<&super::runner_journal::OriginalNoTargetReceipt>,
) -> Result<()> {
    let originals: Vec<&RawValue> = serde_json::from_str(raw.get())?;
    anyhow::ensure!(
        originals.len() == before.len(),
        "native array differs from its typed projection"
    );
    for (index, (original, prior)) in originals.iter().zip(before).enumerate() {
        let object = RawObject::parse(original)?;
        for key in [
            "nonce",
            "boot",
            "generation",
            "incarnation",
            "profile_identity",
            "phase",
            "stop_endpoint",
            "registry",
        ] {
            let expected = prior.get(key);
            let raw_field = object.unique(key)?;
            if expected.is_none() {
                continue;
            }
            let actual = raw_field
                .map(|raw| native_field_projection(raw, key))
                .transpose()?;
            let optional_null = matches!(key, "profile_identity" | "stop_endpoint" | "registry")
                && actual.is_none()
                && expected == Some(&Value::Null);
            anyhow::ensure!(
                optional_null || actual.as_ref() == expected,
                "raw native launch field {key} differs from its typed projection"
            );
        }
        anyhow::ensure!(
            !before[..index].iter().any(|other| {
                ["nonce", "boot", "generation"]
                    .iter()
                    .all(|key| other.get(*key) == prior.get(*key))
            }),
            "ambiguous original native launch slot"
        );
        let mut survivors = after.iter().filter(|next| native_slot_matches(prior, next));
        let Some(next) = survivors.next() else {
            let receipt = retirement.context(
                "protected native launch cannot disappear without original retirement evidence",
            )?;
            anyhow::ensure!(
                *prior == serde_json::to_value(receipt.launch())?
                    && native_phase(prior)?
                        == (NativeLaunchPhase::Unresolved {
                            may_authorize: false
                        })
                    && after
                        .iter()
                        .all(|next| before.iter().any(|old| old == next)),
                "no-target receipt does not authorize this exact original removal"
            );
            continue;
        };
        anyhow::ensure!(
            survivors.next().is_none(),
            "native launch slot was duplicated"
        );
        anyhow::ensure!(
            same_native_launch_identity(prior, next),
            "native birth or explicit phase transition was replaced"
        );
        for key in ["stop_endpoint", "registry"] {
            let evidence = prior.get(key).unwrap_or(&Value::Null);
            if !evidence.is_null() {
                let next_evidence = next
                    .get(key)
                    .context("original native resource evidence disappeared")?;
                if key == "stop_endpoint" {
                    anyhow::ensure!(
                        next_evidence == evidence,
                        "original native stop endpoint changed"
                    );
                } else {
                    anyhow::ensure!(
                        next_evidence.is_object()
                            && evidence.get("control_file_identity")
                                == next_evidence.get("control_file_identity")
                            && evidence.get("socket_path") == next_evidence.get("socket_path"),
                        "original native control evidence changed"
                    );
                }
            }
        }
    }
    for (index, next) in after.iter().enumerate() {
        let phase = native_phase(next)?;
        anyhow::ensure!(
            !after[..index].iter().any(|other| {
                ["nonce", "boot", "generation"]
                    .iter()
                    .all(|key| other.get(*key) == next.get(*key))
            }),
            "duplicate resulting native launch slot"
        );
        if !before.iter().any(|prior| native_slot_matches(prior, next)) {
            anyhow::ensure!(
                phase == NativeLaunchPhase::Natal
                    && next.get("incarnation") == Some(&Value::Null)
                    && next.get("stop_endpoint") == Some(&Value::Null)
                    && next.get("registry") == Some(&Value::Null),
                "new native launch must be an explicit unpublished Natal slot"
            );
        }
    }
    Ok(())
}

fn patch_member<'a>(
    raw: &'a RawValue,
    before: &Value,
    after: &Value,
    array_field: Option<&str>,
    retirement: Option<&super::runner_journal::OriginalNoTargetReceipt>,
) -> Result<Emission<'a>> {
    if array_field == Some("launches") && before.is_array() {
        validate_native_launch_patch(
            raw,
            before.as_array().context("native before is not an array")?,
            after.as_array().context("native after is not an array")?,
            retirement,
        )?;
    }
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
                        Some(prior) => {
                            patch_member(value, prior, next, Some(name.0.as_ref()), retirement)?
                        }
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
                        patch_member(
                            originals[index],
                            &before[index],
                            value,
                            array_field,
                            retirement,
                        )?
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
    if array_field == Some("launches") {
        return same_native_launch_identity(before, after);
    }
    let keys: &[&str] = match array_field {
        Some("repos") => &["name", "source_path", "main_repo_path"],
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
    pub(crate) fn render_value_changes(
        &self,
        changes: &[(usize, &Value, Value)],
    ) -> Result<Vec<u8>> {
        let mut previous = None;
        for (index, _, _) in changes {
            anyhow::ensure!(
                *index < self.rows.len(),
                "changed raw row is outside document"
            );
            anyhow::ensure!(
                previous.is_none_or(|prior| prior < *index),
                "changed raw rows must be unique and ordered"
            );
            previous = Some(*index);
        }
        let mut changes = changes.iter().peekable();
        let mut emitted = Vec::with_capacity(self.rows.len());
        for (index, raw) in self.rows.iter().enumerate() {
            if changes.peek().is_some_and(|change| change.0 == index) {
                let (_, before, after) = changes.next().expect("peeked raw change");
                emitted.push(patch(raw, before, after)?);
            } else {
                emitted.push(Emission::Original(raw));
            }
        }
        Ok(serde_json::to_vec_pretty(&ArrayEmission(emitted))?)
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
        imports: HashMap<&str, Emission<'_>>,
    ) -> Result<Vec<u8>> {
        self.render_with_no_target_retirement(after, key, selected, imports, None)
    }

    pub(crate) fn render_with_no_target_retirement<T: Serialize>(
        &self,
        after: &[T],
        key: impl Fn(&T) -> &str,
        selected: Option<&std::collections::HashSet<&str>>,
        mut imports: HashMap<&str, Emission<'_>>,
        receipt: Option<&super::runner_journal::OriginalNoTargetReceipt>,
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
                changed.push(
                    match receipt.filter(|receipt| receipt.original().session_id() == id) {
                        Some(receipt) => patch_with_no_target_retirement(
                            &self.raw.rows[slot.index],
                            before,
                            &value,
                            receipt,
                        )?,
                        None => patch(&self.raw.rows[slot.index], before, &value)?,
                    },
                );
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
        let launch = native_launch_codec_fixture(NativeLaunchPhase::Armed)?;
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
            if field == "creations" {
                after[field][0]["effect_acknowledged"] = serde_json::json!(true);
            } else if field == "launches" {
                after[field][0]["phase"] = serde_json::to_value(NativeLaunchPhase::Unresolved {
                    may_authorize: true,
                })?;
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
        let pending = native_launch_codec_fixture(NativeLaunchPhase::Natal)?;
        let actual = native_launch_codec_fixture(NativeLaunchPhase::Armed)?;
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
        let mut refused_birth = actual.clone();
        refused_birth["phase"] = serde_json::to_value(NativeLaunchPhase::Unresolved {
            may_authorize: false,
        })?;
        let refusal = serde_json::to_value(patch(
            &raw,
            &before,
            &serde_json::json!({"launches":[refused_birth]}),
        )?)?;
        assert_eq!(refusal["launches"][0]["incarnation"], actual["incarnation"]);
        assert_eq!(refusal["launches"][0]["phase"], refused_birth["phase"]);
        assert_eq!(refusal["launches"][0]["extension"], "original");
        refused_birth["phase"] = serde_json::to_value(NativeLaunchPhase::Unresolved {
            may_authorize: true,
        })?;
        assert!(patch(
            &raw,
            &before,
            &serde_json::json!({"launches":[refused_birth]})
        )
        .is_err());
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

        let mut other_pending = actual.clone();
        other_pending["nonce"] = serde_json::to_value([20_u8; 16])?;
        other_pending["incarnation"] = Value::Null;
        other_pending["phase"] = serde_json::to_value(NativeLaunchPhase::Natal)?;
        let mixed_before = serde_json::json!({"launches":[actual,other_pending]});
        let mut mixed_raw = mixed_before.clone();
        mixed_raw["launches"][0]["extension"] = serde_json::json!("born");
        mixed_raw["launches"][1]["extension"] = serde_json::json!("pending");
        let mut other_actual = other_pending;
        other_actual["incarnation"] =
            serde_json::json!({"pid":22,"start":[23,24],"group":22,"namespace":[25,26]});
        other_actual["phase"] = serde_json::to_value(NativeLaunchPhase::Armed)?;
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
    fn native_launch_codec_fixture(phase: NativeLaunchPhase) -> Result<Value> {
        let nonce = [1_u8; 16];
        let boot = [2_u8; 16];
        let stamp = crate::session::DirectoryIdentity {
            device: 9,
            inode: 10,
            birth_time: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(11)),
        };
        let incarnation = crate::process::ProcessIncarnation {
            pid: 6,
            group: 6,
            start: [7, 8],
            namespace: [12, 13],
        };
        let published = phase == NativeLaunchPhase::Published;
        Ok(serde_json::json!({
            "nonce": nonce,
            "boot": boot,
            "generation": 5,
            "incarnation": if phase == NativeLaunchPhase::Natal { Value::Null } else { serde_json::to_value(incarnation)? },
            "profile_identity": stamp,
            "phase": phase,
            "stop_endpoint": if published { serde_json::to_value(stamp)? } else { Value::Null },
            "registry": if published { serde_json::json!({
                "record_file_identity": stamp,
                "control_file_identity": stamp,
                "socket_path": "/codec/original.sock",
            }) } else { Value::Null },
        }))
    }
    #[test]
    fn native_phase_transitions_and_legacy_upgrade_preserve_original_raw_evidence() -> Result<()> {
        let phases = [
            NativeLaunchPhase::Natal,
            NativeLaunchPhase::Armed,
            NativeLaunchPhase::Published,
            NativeLaunchPhase::Unresolved {
                may_authorize: false,
            },
            NativeLaunchPhase::Unresolved {
                may_authorize: true,
            },
        ];
        for prior_phase in phases {
            for next_phase in phases {
                let original = native_launch_codec_fixture(prior_phase)?;
                let before = serde_json::json!({"launches": [original]});
                let mut raw = before.clone();
                raw["launches"][0]["extension"] = serde_json::json!({"original": true});
                raw["launches"][0]["profile_identity"]["extension"] = serde_json::json!("profile");
                if prior_phase == NativeLaunchPhase::Published {
                    raw["launches"][0]["registry"]["extension"] = serde_json::json!("registry");
                }
                let mut next = original.clone();
                next["phase"] = serde_json::to_value(next_phase)?;
                if prior_phase == NativeLaunchPhase::Natal && next_phase == NativeLaunchPhase::Armed
                {
                    next["incarnation"] = native_launch_codec_fixture(NativeLaunchPhase::Armed)?
                        ["incarnation"]
                        .clone();
                }
                if prior_phase == NativeLaunchPhase::Armed
                    && next_phase == NativeLaunchPhase::Published
                {
                    let published = native_launch_codec_fixture(NativeLaunchPhase::Published)?;
                    next["stop_endpoint"] = published["stop_endpoint"].clone();
                    next["registry"] = published["registry"].clone();
                }
                let bytes = serde_json::value::to_raw_value(&raw)?;
                let after = serde_json::json!({"launches": [next]});
                let result = patch(&bytes, &before, &after);
                if prior_phase.allows_transition_to(next_phase) {
                    let output = serde_json::to_value(result?)?;
                    assert_eq!(
                        output["launches"][0]["extension"],
                        raw["launches"][0]["extension"]
                    );
                    assert_eq!(
                        output["launches"][0]["profile_identity"]["extension"],
                        "profile"
                    );
                    if prior_phase == NativeLaunchPhase::Published {
                        assert_eq!(output["launches"][0]["registry"]["extension"], "registry");
                    }
                } else {
                    assert!(result.is_err(), "{prior_phase:?} -> {next_phase:?}");
                }
            }
        }
        let mut legacy = native_launch_codec_fixture(NativeLaunchPhase::Armed)?;
        legacy.as_object_mut().unwrap().remove("phase");
        let before = serde_json::json!({"launches": [legacy]});
        let mut raw = before.clone();
        raw["launches"][0]["extension"] = serde_json::json!("legacy");
        for phase in phases {
            let mut upgraded = legacy.clone();
            upgraded["phase"] = serde_json::to_value(phase)?;
            let bytes = serde_json::value::to_raw_value(&raw)?;
            let result = patch(
                &bytes,
                &before,
                &serde_json::json!({"launches": [upgraded]}),
            );
            if phase
                == (NativeLaunchPhase::Unresolved {
                    may_authorize: true,
                })
            {
                let output = serde_json::to_value(result?)?;
                assert_eq!(output["launches"][0]["extension"], "legacy");
            } else {
                assert!(result.is_err(), "legacy -> {phase:?}");
            }
        }
        for phase in [Value::Null, serde_json::json!({"state": "missing"})] {
            let mut malformed = legacy.clone();
            malformed["phase"] = phase;
            let malformed = serde_json::json!({"launches": [malformed]});
            let bytes = serde_json::value::to_raw_value(&malformed)?;
            assert!(patch(&bytes, &malformed, &serde_json::json!({"launches": []})).is_err());
        }
        let current = serde_json::json!({"launches": [native_launch_codec_fixture(NativeLaunchPhase::Natal)?]});
        let bytes = serde_json::value::to_raw_value(&current)?;
        assert!(patch(&bytes, &current, &serde_json::json!({"launches": []})).is_err());
        Ok(())
    }
}

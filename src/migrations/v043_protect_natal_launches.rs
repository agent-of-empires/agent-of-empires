use crate::session::raw_document::{patch, Emission, RawObject};
use crate::session::{AnchoredDir, DirectoryIdentity};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct LegacyBirth {
    nonce: [u8; 16],
    boot: [u8; 16],
    generation: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    incarnation: Option<crate::process::ProcessIncarnation>,
    profile_identity: DirectoryIdentity,
}

fn protect_row(raw: &RawValue) -> Result<Option<Box<RawValue>>> {
    let fields = RawObject::parse(raw)?;
    if fields
        .unique("created_at")
        .ok()
        .flatten()
        .is_none_or(|raw| serde_json::from_str::<chrono::DateTime<chrono::Utc>>(raw.get()).is_err())
    {
        return Ok(None);
    }
    let Some(journal) = fields.unique("runner_journal").ok().flatten() else {
        return Ok(None);
    };
    let Ok(journal) = RawObject::parse(journal) else {
        return Ok(None);
    };
    for key in ["coverage", "launches", "preparations", "creations"] {
        if journal.unique(key).is_err() {
            return Ok(None);
        }
    }
    let Some(launches) = journal.unique("launches")? else {
        return Ok(None);
    };
    let Ok(launches) = serde_json::from_str::<Vec<&RawValue>>(launches.get()) else {
        return Ok(None);
    };
    let mut births = Vec::with_capacity(launches.len());
    let mut before = Vec::with_capacity(launches.len());
    let mut after = Vec::with_capacity(launches.len());
    let mut changed = false;
    for raw in launches {
        let Ok(fields) = RawObject::parse(raw) else {
            return Ok(None);
        };
        for key in [
            "nonce",
            "boot",
            "generation",
            "incarnation",
            "profile_identity",
            "stop_endpoint",
            "registry",
            "phase",
        ] {
            if fields.unique(key).is_err() {
                return Ok(None);
            }
        }
        let Ok(birth) = serde_json::from_str::<LegacyBirth>(raw.get()) else {
            return Ok(None);
        };
        if birth.nonce == [0; 16]
            || birth.boot == [0; 16]
            || birth.generation == 0
            || !birth.profile_identity.is_durable()
            || birth
                .incarnation
                .is_some_and(|birth| birth.pid == 0 || birth.group != birth.pid)
            || births.contains(&birth)
        {
            return Ok(None);
        }
        let prior = serde_json::to_value(&birth)?;
        let mut next = prior.clone();
        if fields.unique("phase")?.is_none() {
            next.as_object_mut().unwrap().insert(
                "phase".into(),
                serde_json::json!({"state": "unresolved", "may_authorize": true}),
            );
            changed = true;
        }
        births.push(birth);
        before.push(prior);
        after.push(next);
    }
    if !changed {
        return Ok(None);
    }
    let before = serde_json::json!({"runner_journal": {"launches": before}});
    let after = serde_json::json!({"runner_journal": {"launches": after}});
    match patch(raw, &before, &after)? {
        Emission::Original(_) => Ok(None),
        Emission::Changed(updated) => Ok(Some(updated)),
    }
}

pub(super) fn run(app: &AnchoredDir, version: u32) -> Result<()> {
    super::v042_canonical_execution_journal::rewrite_profiles(app, version, protect_row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_legacy_births_without_rewriting_opaque_tokens() -> Result<()> {
        let birth = LegacyBirth {
            nonce: [1; 16],
            boot: [2; 16],
            generation: 7,
            incarnation: None,
            profile_identity: DirectoryIdentity {
                device: 11,
                inode: 12,
                birth_time: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(3)),
            },
        };
        let encoded = serde_json::to_string(&birth)?;
        let launch = format!(
            r#"{},"opaque":1e400,"opaque":10.5000,"escaped":"\u0061"}}"#,
            encoded.strip_suffix('}').unwrap()
        );
        let row = |launch: &str, dob: &str| {
            RawValue::from_string(format!(
                r#"{{"id":"legacy","created_at":"{dob}","runner_journal":{{"coverage":"complete","launches":[{launch}],"preparations":[],"creations":[],"opaque":1e400}},"extension":"\u0061","extension":1.20e+04}}"#
            ))
        };
        let dob = "2026-10-09T00:00:00Z";
        let original = row(&launch, dob)?;
        let updated = protect_row(&original)?.expect("valid original legacy tuple");
        let fields = RawObject::parse(&updated)?;
        assert_eq!(
            fields
                .values("extension")
                .map(RawValue::get)
                .collect::<Vec<_>>(),
            [r#""\u0061""#, "1.20e+04"]
        );
        let journal = RawObject::parse(fields.unique("runner_journal")?.unwrap())?;
        assert_eq!(journal.unique("opaque")?.unwrap().get(), "1e400");
        let launches: Vec<&RawValue> =
            serde_json::from_str(journal.unique("launches")?.unwrap().get())?;
        let after = RawObject::parse(launches[0])?;
        let original_launch = RawValue::from_string(launch.clone())?;
        let before = RawObject::parse(&original_launch)?;
        for key in [
            "nonce",
            "boot",
            "generation",
            "incarnation",
            "profile_identity",
            "escaped",
        ] {
            assert_eq!(
                after.unique(key)?.unwrap().get(),
                before.unique(key)?.unwrap().get()
            );
        }
        assert_eq!(
            after
                .values("opaque")
                .map(RawValue::get)
                .collect::<Vec<_>>(),
            ["1e400", "10.5000"]
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(after.unique("phase")?.unwrap().get())?,
            serde_json::json!({"state": "unresolved", "may_authorize": true})
        );
        assert!(protect_row(&updated)?.is_none());
        for (launch, dob) in [
            (r#"{"nonce":[1]}"#.to_owned(), dob),
            (
                format!(
                    "{},\"phase\":null,\"phase\":null}}",
                    encoded.strip_suffix('}').unwrap()
                ),
                dob,
            ),
            (launch, "invalid-original-DOB"),
        ] {
            assert!(protect_row(&row(&launch, dob)?)?.is_none());
        }
        Ok(())
    }
}

use anyhow::{Context, Result};

use super::raw_document::{patch, Emission, RawDocument, RawObject};
use crate::process::{CustodianLiveness, OriginalCustodianBirth};

pub(crate) fn pending_custodian(
    raw: &serde_json::value::RawValue,
    profile: super::storage::DirectoryIdentity,
) -> Result<Option<OriginalCustodianBirth>> {
    let row = RawObject::parse(raw)?;
    let Some(lease) = row
        .unique("lifecycle_reservation")?
        .filter(|raw| raw.get() != "null")
    else {
        return Ok(None);
    };
    let lease = RawObject::parse(lease)?;
    let operation: super::LifecycleOperation = serde_json::from_str(
        lease
            .unique("op")?
            .context("missing claim operation")?
            .get(),
    )?;
    if !matches!(
        operation,
        super::LifecycleOperation::Create | super::LifecycleOperation::Attach
    ) {
        return Ok(None);
    }
    let claims = lease
        .unique("path_claims")?
        .context("missing claim inventory")?;
    let fields = RawObject::parse(claims)?;
    fields.unique("state")?.context("missing claim state")?;
    fields.unique("paths")?.context("missing candidate paths")?;
    if !matches!(
        serde_json::from_str::<super::WorktreePathClaims>(claims.get())?,
        super::WorktreePathClaims::Pending(_)
    ) {
        return Ok(None);
    }
    let Some(custodian) = lease.unique("custodian")?.filter(|raw| raw.get() != "null") else {
        return Ok(None);
    };
    let custodian: OriginalCustodianBirth = serde_json::from_str(custodian.get())?;
    let id: String = serde_json::from_str(row.unique("id")?.context("missing owner")?.get())?;
    let created_at: chrono::DateTime<chrono::Utc> = serde_json::from_str(
        row.unique("created_at")?
            .context("missing row birth")?
            .get(),
    )?;
    let generation: u64 = serde_json::from_str(
        row.unique("lifecycle_generation")?
            .context("missing row counter")?
            .get(),
    )?;
    let lease_generation: u64 = serde_json::from_str(
        lease
            .unique("generation")?
            .context("missing claim counter")?
            .get(),
    )?;
    let _: chrono::DateTime<chrono::Utc> =
        serde_json::from_str(lease.unique("at")?.context("missing claim time")?.get())?;
    if !profile.is_durable()
        || custodian.profile != profile
        || custodian.session_id != id
        || custodian.created_at != created_at
        || custodian.generation != generation
        || lease_generation != generation
    {
        return Ok(None);
    }
    Ok(Some(custodian))
}

pub(crate) fn mark_lost(
    document: &mut RawDocument,
    id: &str,
    profile: super::storage::DirectoryIdentity,
    observe: &impl Fn(&OriginalCustodianBirth) -> CustodianLiveness,
) -> Result<bool> {
    let owners = document.owners("id");
    let Some(owner) = owners
        .get(id)
        .filter(|owner| owner.count == 1 && !owner.ambiguous)
    else {
        return Ok(false);
    };
    let raw = &document.rows[owner.index];
    let Ok(Some(custodian)) = pending_custodian(raw, profile) else {
        return Ok(false);
    };
    if observe(&custodian) != CustodianLiveness::Lost {
        return Ok(false);
    }
    let before =
        serde_json::json!({"lifecycle_reservation": {"path_claims": {"state": "pending"}}});
    let after = serde_json::json!({"lifecycle_reservation": {"path_claims": {"state": "unknown"}}});
    if let Emission::Changed(updated) = patch(raw, &before, &after)? {
        document.rows[owner.index] = updated;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (String, super::super::storage::DirectoryIdentity) {
        let profile = super::super::storage::DirectoryIdentity {
            device: 1,
            inode: 2,
            birth_time: Some(std::time::UNIX_EPOCH),
        };
        let birth = OriginalCustodianBirth {
            boot: "boot".into(),
            process: crate::process::ProcessIncarnation {
                pid: 4312,
                group: 4312,
                start: [123, 0],
                namespace: [7, 8],
            },
            profile,
            session_id: "owner".into(),
            created_at: "2000-01-01T00:00:00Z".parse().unwrap(),
            generation: 4,
        };
        let birth = serde_json::to_string(&birth).unwrap();
        (
            format!(
                r#"{{"id":"owner","created_at":"2000-01-01T00:00:00Z","lifecycle_generation":4,"project_path":"/tmp/current","opaque":{{"same":1,"same":2,"huge":1234567890123456789012345678901234567890,"float":1e400}},"lifecycle_reservation":{{"op":"create","generation":4,"at":"2000-01-01T00:00:00Z","custodian":{birth},"path_claims":{{"state":"pending","paths":["/tmp/a","/tmp/\u0062","/tmp/a"],"opaque":{{"same":1,"same":2}}}}}}}}"#
            ),
            profile,
        )
    }

    #[test]
    fn original_loss_changes_only_state_and_retains_literal_complete_inventory() {
        let (row, profile) = fixture();
        let opaque = r#"{"id":"other","project_path":false,"unknown":{"same":1,"same":2}}"#;
        let duplicate = r#"{"id":"duplicated","id":"duplicated","lifecycle_reservation":{"path_claims":{"state":"pending","paths":["/tmp/reserved"]}}}"#;
        let mut document = RawDocument::parse(&format!("[{row},{opaque},{duplicate}]")).unwrap();
        for liveness in [CustodianLiveness::Live, CustodianLiveness::Uncertain] {
            assert!(!mark_lost(&mut document, "owner", profile, &|_| liveness).unwrap());
            assert_eq!(document.rows[0].get(), row);
        }
        assert!(mark_lost(&mut document, "owner", profile, &|_| {
            CustodianLiveness::Lost
        })
        .unwrap());
        assert_eq!(
            document.rows[0].get(),
            row.replace(r#""state":"pending""#, r#""state":"unknown""#)
        );
        assert_eq!(document.rows[1].get(), opaque);
        assert_eq!(document.rows[2].get(), duplicate);
        let retained = serde_json::to_vec(&document.rows).unwrap();
        assert!(!mark_lost(&mut document, "owner", profile, &|_| {
            CustodianLiveness::Lost
        })
        .unwrap());
        assert_eq!(serde_json::to_vec(&document.rows).unwrap(), retained);
        let projected = super::super::deletion::WorktreeOwnerDocument::project(
            RawDocument::parse(&format!("[{}]", document.rows[0].get())).unwrap(),
        )
        .unwrap();
        assert!(matches!(&projected.rows[0].pending,
            super::super::WorktreePathClaims::Unknown(Some(paths))
                if paths == &vec![std::path::PathBuf::from("/tmp/a"), std::path::PathBuf::from("/tmp/b"), std::path::PathBuf::from("/tmp/a")]));
    }

    #[test]
    fn missing_or_mismatched_original_evidence_and_duplicate_owners_stay_protected() {
        let (row, profile) = fixture();
        let mismatched = [
            row.replace(r#""lifecycle_generation":4"#, r#""lifecycle_generation":5"#),
            row.replace(r#""generation":4,"at""#, r#""generation":5,"at""#),
            row.replacen(
                r#""created_at":"2000-01-01T00:00:00Z""#,
                r#""created_at":"2001-01-01T00:00:00Z""#,
                1,
            ),
            row.replace(r#""custodian":"#, r#""custodian":null,"old_custodian":"#),
            row.replace(r#""state":"pending""#, r#""state":"unknown""#),
            row.replace(
                r#""state":"pending""#,
                r#""state":"pending","state":"pending""#,
            ),
        ];
        for altered in mismatched {
            let mut document = RawDocument::parse(&format!("[{altered}]")).unwrap();
            assert!(!mark_lost(&mut document, "owner", profile, &|_| panic!(
                "unbound metadata must not trigger a death observation"
            ))
            .unwrap());
            assert_eq!(document.rows[0].get(), altered);
        }
        let mut replaced_profile = profile;
        replaced_profile.birth_time =
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1));
        let mut document = RawDocument::parse(&format!("[{row}]")).unwrap();
        assert!(!mark_lost(&mut document, "owner", replaced_profile, &|_| {
            CustodianLiveness::Lost
        })
        .unwrap());
        let mut duplicate = RawDocument::parse(&format!("[{row},{row}]")).unwrap();
        assert!(!mark_lost(&mut duplicate, "owner", profile, &|_| panic!(
            "duplicate owner is not an original"
        ))
        .unwrap());
        assert!(duplicate.rows.iter().all(|retained| retained.get() == row));
    }
}

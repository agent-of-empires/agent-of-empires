//! Metadata-only retirement. Retained owners are exclusions, never executable authority.
use super::raw_document::{RawDocument, RawObject};
use super::{AnchoredDir, DirectoryIdentity, Storage};
use anyhow::{Context, Result};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

const FILE: &str = "retained-intents.json";
const VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedOwner {
    source: DirectoryIdentity,
    source_profile: String,
    owner: Box<RawValue>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    records: Vec<RetainedOwner>,
}

struct Selection {
    storage: Storage,
    app: AnchoredDir,
    id: String,
    record: RetainedOwner,
}

#[derive(Clone)]
pub(crate) struct ClaimAbort(Arc<Selection>);

impl ClaimAbort {
    pub(crate) fn id(&self) -> &str {
        &self.0.id
    }
    pub(crate) fn origin(&self) -> &Storage {
        &self.0.storage
    }
    pub(crate) fn check_visible(&self, before: &super::Instance) -> Result<()> {
        let raw = RawObject::parse(&self.0.record.owner)?;
        let expected = serde_json::to_value(before)?;
        for name in [
            "id",
            "created_at",
            "lifecycle_generation",
            "lifecycle_reservation",
            "runner_journal",
            "active_execution",
            "project_path",
            "worktree_info",
            "workspace_info",
        ] {
            let actual = raw
                .unique(name)?
                .map(|value| {
                    if name == "runner_journal" {
                        serde_json::from_str::<super::runner_journal::RunnerExecutionJournal>(
                            value.get(),
                        )
                        .and_then(serde_json::to_value)
                    } else {
                        serde_json::from_str::<serde_json::Value>(value.get())
                    }
                })
                .transpose()?
                .unwrap_or(serde_json::Value::Null);
            let wanted = expected.get(name).unwrap_or(&serde_json::Value::Null);
            anyhow::ensure!(
                &actual == wanted,
                "selected intent changed before metadata confirmation"
            );
        }
        Ok(())
    }
}

pub(crate) struct ClaimAbortAck(ClaimAbort);

impl ClaimAbortAck {
    pub(crate) fn id(&self) -> &str {
        self.0.id()
    }
    pub(crate) fn origin(&self) -> &Storage {
        self.0.origin()
    }
}

fn owner_id(raw: &RawValue) -> Result<String> {
    serde_json::from_str(
        RawObject::parse(raw)?
            .unique("id")?
            .context("retained owner has no unique id")?
            .get(),
    )
    .context("retained owner id is not a string")
}

fn eligible(raw: &RawValue) -> Result<()> {
    let object = RawObject::parse(raw)?;
    let lease = RawObject::parse(
        object
            .unique("lifecycle_reservation")?
            .context("session has no filesystem intent")?,
    )?;
    let operation: super::LifecycleOperation = serde_json::from_str(
        lease
            .unique("op")?
            .context("intent has no operation")?
            .get(),
    )?;
    anyhow::ensure!(
        matches!(
            operation,
            super::LifecycleOperation::Create | super::LifecycleOperation::Attach
        ),
        "only unfinished Create or Attach intent metadata can be aborted"
    );
    let claims = RawObject::parse(
        lease
            .unique("path_claims")?
            .context("intent has no path claims")?,
    )?;
    let state: String = serde_json::from_str(
        claims
            .unique("state")?
            .context("intent has no claim state")?
            .get(),
    )?;
    anyhow::ensure!(
        matches!(state.as_str(), "pending" | "unknown"),
        "only Pending or Unknown filesystem intent metadata can be aborted"
    );
    Ok(())
}

fn read_at(app: &AnchoredDir) -> Result<Ledger> {
    let Some(bytes) = app.read_regular(Path::new(FILE), usize::MAX)? else {
        let schema = app.read_regular(Path::new(".schema_version"), usize::MAX)?;
        anyhow::ensure!(
            crate::migrations::v041_retained_intents::is_legacy_schema(schema.as_deref())?,
            "mandatory retained intent ledger is missing; refusing ownership admission"
        );
        return Ok(Ledger {
            version: VERSION,
            records: Vec::new(),
        });
    };
    let ledger: Ledger =
        serde_json::from_slice(&bytes).context("unreadable retained intent ledger")?;
    anyhow::ensure!(
        ledger.version == VERSION,
        "unsupported retained intent ledger version"
    );
    for record in &ledger.records {
        owner_id(&record.owner)?;
        eligible(&record.owner)?;
    }
    Ok(ledger)
}

pub(crate) fn initialize_legacy_in(app_dir: &Path) -> Result<()> {
    let app = AnchoredDir::create(app_dir)?;
    if app.regular_lookup(Path::new(FILE))?.is_some() {
        read_at(&app)?;
        return Ok(());
    }
    let schema = app.read_regular(Path::new(".schema_version"), usize::MAX)?;
    anyhow::ensure!(
        crate::migrations::v041_retained_intents::is_legacy_schema(schema.as_deref())?,
        "mandatory retained intent ledger is missing"
    );
    let bytes = serde_json::to_vec(&Ledger {
        version: VERSION,
        records: Vec::new(),
    })?;
    app.publish_file(
        Path::new(FILE),
        &mut bytes.as_slice(),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
        false,
        None,
    )?;
    read_at(&app)?;
    Ok(())
}

pub(crate) fn validate_current_app() -> Result<()> {
    read_at(&AnchoredDir::open(&super::get_app_dir()?)?).map(|_| ())
}

pub(crate) fn retained_raw_owners_in(app_dir: &Path) -> Result<Vec<Box<RawValue>>> {
    if !app_dir.try_exists()? {
        return Ok(Vec::new());
    }
    Ok(read_at(&AnchoredDir::open(app_dir)?)?
        .records
        .into_iter()
        .map(|record| record.owner)
        .collect())
}

fn retained_raw_owners() -> Result<Vec<Box<RawValue>>> {
    let app = super::get_app_dir()?;
    let mut rows = retained_raw_owners_in(&app)?;
    if let Some(sibling) = super::sibling_namespace_app_dir() {
        if sibling.try_exists()? {
            let same = if app.try_exists()? {
                let own = std::fs::metadata(&app)?;
                let peer = std::fs::metadata(&sibling)?;
                own.dev() == peer.dev() && own.ino() == peer.ino()
            } else {
                false
            };
            if !same {
                rows.extend(retained_raw_owners_in(&sibling)?);
            }
        }
    }
    Ok(rows)
}

pub(crate) fn load_owners() -> Result<super::deletion::WorktreeOwnerDocument> {
    super::deletion::WorktreeOwnerDocument::project(RawDocument {
        rows: retained_raw_owners()?,
    })
}

pub(crate) fn ensure_profile_unretained(storage: &Storage) -> Result<()> {
    let original = storage.original_profile_identity()?;
    for app in [
        Some(super::get_app_dir()?),
        super::sibling_namespace_app_dir(),
    ]
    .into_iter()
    .flatten()
    {
        if app.try_exists()? {
            let ledger = read_at(&AnchoredDir::open(&app)?)?;
            anyhow::ensure!(
                !ledger
                    .records
                    .iter()
                    .any(|record| record.source == original),
                "physical profile is retained by an aborted filesystem intent"
            );
        }
    }
    Ok(())
}

pub(crate) fn retained_ids_in(app_dir: &Path) -> Result<HashSet<String>> {
    retained_raw_owners_in(app_dir)?
        .iter()
        .map(|owner| owner_id(owner))
        .collect()
}

pub(crate) fn ensure_id_available(id: &str) -> Result<()> {
    for owner in retained_raw_owners()? {
        anyhow::ensure!(
            owner_id(&owner)? != id,
            "session id {id} is permanently reserved by an aborted filesystem intent"
        );
    }
    Ok(())
}

fn with_owner_locked<T>(
    storage: &Storage,
    id: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _workspace = super::acquire_session_workspace_claim_lock()?;
    let _identity = super::acquire_session_identity_lock()?;
    let _namespace = super::storage::acquire_profile_namespace_lock()?;
    storage.verify_profile_identity()?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock(id)?;
    super::storage::with_storages_locked(std::slice::from_ref(storage), || {
        crate::process::worker_registry::with_registry_fence(id, action)
    })?
}

pub(crate) fn capture(storage: &Storage, id: &str) -> Result<ClaimAbort> {
    super::validate_instance_id(id)?;
    with_owner_locked(storage, id, || {
        let app = AnchoredDir::open(&super::get_app_dir()?)?;
        anyhow::ensure!(
            app.regular_exists(Path::new(FILE)),
            "run data migrations before aborting an intent"
        );
        read_at(&app)?;
        let mut document = storage.load_claim_abort_document_locked()?;
        let owners = document.owners("id");
        let slot = owners
            .get(id)
            .context("no filesystem intent with that exact id")?;
        anyhow::ensure!(
            slot.count == 1 && !slot.ambiguous,
            "selected filesystem intent owner is ambiguous"
        );
        let owner = document.rows.remove(slot.index);
        eligible(&owner)?;
        Ok(ClaimAbort(Arc::new(Selection {
            storage: storage.clone(),
            app,
            id: id.to_owned(),
            record: RetainedOwner {
                source: storage.original_profile_identity()?,
                source_profile: storage.profile().to_owned(),
                owner,
            },
        })))
    })
}

#[derive(Serialize)]
struct AppendedLedger<'a> {
    version: u32,
    #[serde(serialize_with = "serialize_records")]
    records: (&'a [RetainedOwner], &'a RetainedOwner),
}

fn serialize_records<S: Serializer>(
    records: &(&[RetainedOwner], &RetainedOwner),
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    let mut sequence = serializer.serialize_seq(Some(records.0.len() + 1))?;
    for record in records.0.iter().chain(std::iter::once(records.1)) {
        sequence.serialize_element(record)?;
    }
    sequence.end()
}

pub(crate) fn abort(selection: &ClaimAbort) -> Result<ClaimAbortAck> {
    let selected = &selection.0;
    with_owner_locked(&selected.storage, &selected.id, || {
        anyhow::ensure!(
            AnchoredDir::open(&super::get_app_dir()?)?.birth_identity()?
                == selected.app.birth_identity()?,
            "original app namespace changed before metadata abort"
        );
        let ledger = read_at(&selected.app)?;
        let already_retained = ledger.records.iter().any(|record| {
            record.source == selected.record.source
                && record.owner.get() == selected.record.owner.get()
        });
        let mut document = selected.storage.load_claim_abort_document_locked()?;
        let owners = document.owners("id");
        match owners.get(&selected.id) {
            Some(slot) => {
                anyhow::ensure!(
                    slot.count == 1 && !slot.ambiguous,
                    "selected intent owner became ambiguous"
                );
                anyhow::ensure!(
                    document.rows[slot.index].get() == selected.record.owner.get(),
                    "selected intent changed before metadata abort"
                );
                document.rows.remove(slot.index);
            }
            None => anyhow::ensure!(
                already_retained,
                "selected intent disappeared before metadata abort"
            ),
        }
        let source_bytes = serde_json::to_vec_pretty(&document.rows)?;
        if !already_retained {
            let bytes = serde_json::to_vec(&AppendedLedger {
                version: VERSION,
                records: (&ledger.records, &selected.record),
            })?;
            selected.app.publish_file(
                Path::new(FILE),
                &mut bytes.as_slice(),
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
                true,
                None,
            )?;
        } else {
            selected
                .app
                .open_regular(Path::new(FILE), usize::MAX)?
                .context("retained intent ledger disappeared")?
                .sync_all()?;
            selected.app.sync()?;
        }
        selected
            .storage
            .publish_claim_abort_document_locked(&source_bytes)?;
        Ok(ClaimAbortAck(selection.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (
        super::super::test_support::AppDirGuard,
        Storage,
        String,
        std::path::PathBuf,
    ) {
        let env = super::super::test_support::isolate_app_dir();
        let app = super::super::get_app_dir().unwrap();
        initialize_legacy_in(&app).unwrap();
        std::fs::write(app.join(".schema_version"), "41").unwrap();
        let storage = Storage::new_unwatched("intent-source").unwrap();
        let resource = app.parent().unwrap().join("protected-resource");
        std::fs::create_dir_all(&resource).unwrap();
        std::fs::write(resource.join("history"), b"original bytes").unwrap();
        let path = serde_json::to_string(&resource).unwrap();
        let owner = format!(
            r#"{{"id":"aborted-owner","created_at":"2000-01-01T00:00:00Z","lifecycle_generation":4,"project_path":{path},"opaque":{{"same":1,"same":2,"float":1e400}},"lifecycle_reservation":{{"op":"create","generation":4,"at":"2000-01-01T00:00:00Z","path_claims":{{"state":"unknown","paths":[{path},{path}],"opaque":{{"same":1,"same":2}}}}}},"runner_journal":{{"opaque_native_history":1234567890123456789012345678901234567890}}}}"#
        );
        std::fs::write(storage.sessions_path(), format!("[{owner}]")).unwrap();
        (env, storage, owner, resource)
    }

    #[test]
    #[serial_test::serial]
    fn abort_preserves_exact_owner_and_foreign_rows_and_reserves_paths_and_id() {
        let (_env, storage, owner, resource) = fixture();
        let foreign = r#"{"id":"foreign","project_path":false,"opaque":{"same":1,"same":2}}"#;
        std::fs::write(storage.sessions_path(), format!("[{owner},{foreign}]")).unwrap();
        let selection = capture(&storage, "aborted-owner").unwrap();
        let ack = abort(&selection).unwrap();
        assert_eq!(ack.id(), "aborted-owner");
        let remaining =
            RawDocument::parse(&std::fs::read_to_string(storage.sessions_path()).unwrap()).unwrap();
        assert_eq!(remaining.rows[0].get(), foreign);
        let ledger =
            read_at(&AnchoredDir::open(&super::super::get_app_dir().unwrap()).unwrap()).unwrap();
        assert_eq!(ledger.records[0].owner.get(), owner);
        assert_eq!(
            ledger.records[0].source,
            storage.original_profile_identity().unwrap()
        );
        assert!(ensure_id_available("aborted-owner").is_err());
        let claims = load_owners().unwrap();
        assert!(
            matches!(&claims.rows[0].pending, super::super::WorktreePathClaims::Unknown(Some(paths)) if paths == &vec![resource.clone(), resource.clone()])
        );
        assert_eq!(
            std::fs::read(resource.join("history")).unwrap(),
            b"original bytes"
        );
        abort(&selection).unwrap();
        assert_eq!(
            read_at(&AnchoredDir::open(&super::super::get_app_dir().unwrap()).unwrap())
                .unwrap()
                .records
                .len(),
            1
        );
    }

    #[test]
    #[serial_test::serial]
    fn source_failure_keeps_both_owners_until_retry_acknowledges_removal() {
        let (_env, mut storage, owner, resource) = fixture();
        storage.set_fail_writes_for_test(true);
        let failed = capture(&storage, "aborted-owner").unwrap();
        assert!(abort(&failed).is_err());
        let app = AnchoredDir::open(&super::super::get_app_dir().unwrap()).unwrap();
        assert_eq!(read_at(&app).unwrap().records[0].owner.get(), owner);
        assert_eq!(
            RawDocument::parse(&std::fs::read_to_string(storage.sessions_path()).unwrap())
                .unwrap()
                .rows[0]
                .get(),
            owner
        );
        storage.set_fail_writes_for_test(false);
        let retry = capture(&storage, "aborted-owner").unwrap();
        abort(&retry).unwrap();
        assert!(
            RawDocument::parse(&std::fs::read_to_string(storage.sessions_path()).unwrap())
                .unwrap()
                .rows
                .is_empty()
        );
        assert_eq!(read_at(&app).unwrap().records.len(), 1);
        assert_eq!(
            std::fs::read(resource.join("history")).unwrap(),
            b"original bytes"
        );
    }

    #[test]
    #[serial_test::serial]
    fn changed_owner_or_replaced_profile_refuses_without_retaining_or_removing_peer() {
        for replace_profile in [false, true] {
            let (_env, storage, owner, resource) = fixture();
            let selection = capture(&storage, "aborted-owner").unwrap();
            let changed = owner.replace("\"lifecycle_generation\":4", "\"lifecycle_generation\":5");
            if replace_profile {
                let profile = storage.sessions_path().parent().unwrap().to_path_buf();
                std::fs::rename(&profile, profile.with_file_name("departed-source")).unwrap();
                Storage::new_unwatched("intent-source").unwrap();
            }
            std::fs::write(storage.sessions_path(), format!("[{changed}]")).unwrap();
            assert!(abort(&selection).is_err());
            assert_eq!(
                RawDocument::parse(&std::fs::read_to_string(storage.sessions_path()).unwrap())
                    .unwrap()
                    .rows[0]
                    .get(),
                changed
            );
            assert!(
                read_at(&AnchoredDir::open(&super::super::get_app_dir().unwrap()).unwrap())
                    .unwrap()
                    .records
                    .is_empty()
            );
            assert_eq!(
                std::fs::read(resource.join("history")).unwrap(),
                b"original bytes"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn missing_or_unreadable_mandatory_ledger_refuses_before_source_effects() {
        for replacement in [
            None,
            Some("not-json"),
            Some(r#"{"version":2,"records":[]}"#),
        ] {
            let (_env, storage, owner, resource) = fixture();
            let selection = capture(&storage, "aborted-owner").unwrap();
            let app = super::super::get_app_dir().unwrap();
            match replacement {
                None => std::fs::remove_file(app.join(FILE)).unwrap(),
                Some(bytes) => std::fs::write(app.join(FILE), bytes).unwrap(),
            }
            assert!(abort(&selection).is_err());
            assert!(initialize_legacy_in(&app).is_err());
            assert_eq!(
                RawDocument::parse(&std::fs::read_to_string(storage.sessions_path()).unwrap())
                    .unwrap()
                    .rows[0]
                    .get(),
                owner
            );
            assert_eq!(
                std::fs::read(resource.join("history")).unwrap(),
                b"original bytes"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn sibling_retention_reserves_id_and_paths_in_a_fresh_current_namespace() {
        let (_env, storage, _owner, resource) = fixture();
        abort(&capture(&storage, "aborted-owner").unwrap()).unwrap();
        let app = super::super::get_app_dir().unwrap();
        let sibling = super::super::sibling_namespace_app_dir().unwrap();
        if sibling.try_exists().unwrap() {
            std::fs::remove_dir_all(&sibling).unwrap();
        }
        std::fs::rename(&app, &sibling).unwrap();
        initialize_legacy_in(&app).unwrap();
        let fresh = Storage::new_unwatched("fresh").unwrap();
        assert!(ensure_id_available("aborted-owner").is_err());
        let inventory = super::super::deletion::paths_in_use_except(&[]);
        assert!(matches!(
            &inventory,
            super::super::deletion::PathsInUse::Known(_)
        ));
        assert!(inventory.covers(&resource));
        assert!(inventory.covers(&resource.join("child")));
        let mut replacement =
            super::super::Instance::new("replacement", resource.to_str().unwrap());
        replacement.id = "aborted-owner".into();
        assert!(fresh
            .update(|rows, _| {
                rows.push(replacement);
                Ok(())
            })
            .is_err());
        assert!(fresh.load().unwrap().is_empty());
        assert_eq!(
            std::fs::read(resource.join("history")).unwrap(),
            b"original bytes"
        );
    }

    #[test]
    #[serial_test::serial]
    fn retained_owner_keeps_existing_restart_marker_and_refuses_its_rewrite_or_claim() {
        let (_env, storage, _owner, _resource) = fixture();
        crate::process::worker_registry::mark_restart_pending("aborted-owner", 4);
        let marker = crate::process::worker_registry::restart_marker_path("aborted-owner").unwrap();
        let before = std::fs::read(&marker).unwrap();
        assert_eq!(
            crate::process::worker_registry::peek_restart_marker("aborted-owner"),
            Some(4)
        );
        abort(&capture(&storage, "aborted-owner").unwrap()).unwrap();
        crate::process::worker_registry::mark_restart_pending("aborted-owner", 99);
        assert!(crate::process::worker_registry::claim_restart_marker("aborted-owner").is_none());
        crate::process::worker_registry::clear_restart_marker("aborted-owner");
        assert_eq!(std::fs::read(marker).unwrap(), before);
    }

    #[test]
    #[serial_test::serial]
    fn profile_mutations_keep_the_original_pfd_but_allow_disjoint_unrelated_profiles() {
        let (_env, storage, _owner, resource) = fixture();
        abort(&capture(&storage, "aborted-owner").unwrap()).unwrap();
        Storage::new_unwatched("unrelated").unwrap();
        super::super::rename_profile("unrelated", "renamed-unrelated").unwrap();
        super::super::delete_profile("renamed-unrelated").unwrap();
        assert!(super::super::rename_profile("intent-source", "renamed-source").is_err());
        assert!(super::super::delete_profile("intent-source").is_err());
        storage.verify_profile_identity().unwrap();
        assert_eq!(
            std::fs::read(resource.join("history")).unwrap(),
            b"original bytes"
        );
        assert!(load_owners()
            .unwrap()
            .rows
            .iter()
            .any(|row| row.ids.iter().any(|id| id == "aborted-owner")));
    }
}

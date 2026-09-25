//! Narrow projections from native stores that also carry conversation state.

use std::collections::HashSet;
use std::fs::{self, Permissions};

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use serde_json::{Map, Value};

use crate::session::anchored_fs::{AnchoredDir, FilePublication};

use super::super::policy::{CollectionKind, Exception};
use super::super::{
    canonical_source, guard, open_canonical_dir, open_canonical_file, publish_guarded_file,
    snapshot_config_database, NativeStateBoundary, ReadAccess, StateOrigin,
};
use super::state;

fn optional_child(root: &AnchoredDir, relative: &Path) -> Result<Option<AnchoredDir>> {
    match root.child(relative) {
        Ok(child) => Ok(Some(child)),
        Err(error)
            if (error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) || error.raw_os_error() == Some(libc::ELOOP)
            }) || error
                .downcast_ref::<nix::errno::Errno>()
                .is_some_and(|error| {
                    matches!(
                        error,
                        nix::errno::Errno::ENOENT
                            | nix::errno::Errno::ENOTDIR
                            | nix::errno::Errno::ELOOP
                    )
                })) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Seed the authored skill collection. Native metadata inside it is state and
/// stays behind; the sandbox keeps the skill trees themselves, and the returned
/// names are what the controls projection may describe.
pub(in super::super) fn seed_skills(
    boundary: &NativeStateBoundary,
    scope: usize,
    source: &guard::SourceRoot,
    destination: &AnchoredDir,
    discovery_links: bool,
) -> Result<HashSet<String>> {
    let access = ReadAccess {
        root: Some(source),
        exception: Exception::Collection {
            scope,
            root: source.path(),
            kind: CollectionKind::Skills,
        },
    };
    super::super::seed_directory(
        &source.path().join("skills"),
        destination,
        Path::new("skills"),
        boundary,
        discovery_links,
        access,
    )?;
    let Some(local) = optional_child(destination, Path::new("skills"))? else {
        return Ok(HashSet::new());
    };
    let mut retained = HashSet::new();
    for name in local.read_dir(Path::new(""), usize::MAX)? {
        let Some(name) = name.to_str().filter(|name| !name.starts_with('.')) else {
            continue;
        };
        if local.regular_lookup(Path::new(name))? == Some(false) {
            retained.insert(name.to_owned());
        }
    }
    Ok(retained)
}

/// Plugin trees carry their own runtime state, so seed them as a collection and
/// let the declared state rules decide which entries stay behind.
pub(in super::super) fn seed_plugins(
    boundary: &NativeStateBoundary,
    scope: usize,
    source: &guard::SourceRoot,
    destination: &AnchoredDir,
    discovery_links: bool,
) -> Result<()> {
    let access = ReadAccess {
        root: Some(source),
        exception: Exception::Collection {
            scope,
            root: source.path(),
            kind: CollectionKind::Plugins,
        },
    };
    super::super::seed_directory(
        &source.path().join("plugins"),
        destination,
        Path::new("plugins"),
        boundary,
        discovery_links,
        access,
    )
}

pub(in super::super) fn seed_nodes(
    boundary: &NativeStateBoundary,
    scope: usize,
    source: &guard::SourceRoot,
    destination: &AnchoredDir,
) -> Result<()> {
    // Call only for an initial home/profile. A later local deletion is a choice.
    if let Some(local) = optional_child(destination, Path::new("workspace/meetings"))? {
        if local.regular_lookup(Path::new("nodes.json"))?.is_some() {
            return Ok(());
        }
    }
    let lookup = source.path().join("workspace");
    let canonical = match fs::canonicalize(&lookup) {
        Ok(path) => path,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error).context("resolving native workspace configuration"),
    };
    if super::super::original_escapes(boundary, &canonical) {
        return Ok(());
    }
    let workspace = open_canonical_dir(&canonical)?;
    let Some(meetings) = optional_child(&workspace, Path::new("meetings"))? else {
        return Ok(());
    };
    let Some(mut file) = meetings.open_regular(Path::new("nodes.json"), usize::MAX)? else {
        return Ok(());
    };
    let leaves = [canonical.join("meetings/nodes.json")];
    let access = ReadAccess {
        root: Some(source),
        exception: Exception::Mixed {
            origin: StateOrigin::Hermes {
                scope,
                marker: state::WORKSPACE_MARKER,
            },
            leaves: &leaves,
        },
    };
    if boundary.rejects(&leaves[0], false, access) {
        return Ok(());
    }
    let mut guard = guard::ReadGuard::new(boundary, access)?;
    guard.record_route(&lookup, &canonical)?;
    guard.pin_directory(&workspace)?;
    guard.pin_directory(&meetings)?;
    if !guard.record_file(&leaves[0], &file)? {
        return Ok(());
    }
    let target = destination.create_child(Path::new("workspace/meetings"))?;
    publish_guarded_file(
        &mut file,
        &guard,
        &target,
        Path::new("nodes.json"),
        Permissions::from_mode(0o600),
        false,
    )?;
    Ok(())
}

const PROJECT_TABLES: &[(&str, &str)] = &[
    ("projects", "CREATE TABLE projects (id TEXT PRIMARY KEY, slug TEXT NOT NULL UNIQUE, name TEXT NOT NULL, description TEXT, icon TEXT, color TEXT, board_slug TEXT, primary_path TEXT, created_at INTEGER NOT NULL, archived INTEGER NOT NULL DEFAULT 0)"),
    ("project_folders", "CREATE TABLE project_folders (project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE, path TEXT NOT NULL, label TEXT, is_primary INTEGER NOT NULL DEFAULT 0, added_at INTEGER NOT NULL, PRIMARY KEY (project_id, path))"),
    ("project_meta", "CREATE TABLE project_meta (key TEXT PRIMARY KEY, value TEXT)"),
    ("discovered_repos", "CREATE TABLE discovered_repos (root TEXT PRIMARY KEY, label TEXT, last_seen INTEGER NOT NULL)"),
];

pub(in super::super) fn seed_projects(
    boundary: &NativeStateBoundary,
    scope: usize,
    source: &guard::SourceRoot,
    destination: &AnchoredDir,
) -> Result<()> {
    if destination
        .regular_lookup(Path::new("projects.db"))?
        .is_some()
    {
        return Ok(());
    }
    let lookup = source.path().join("projects.db");
    let canonical = match fs::canonicalize(&lookup) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("resolving native project configuration"),
    };
    let mut wal = canonical.as_os_str().to_os_string();
    wal.push("-wal");
    let mut journal = canonical.as_os_str().to_os_string();
    journal.push("-journal");
    let leaves = [canonical, PathBuf::from(wal), PathBuf::from(journal)];
    let access = ReadAccess {
        root: Some(source),
        exception: Exception::Mixed {
            origin: StateOrigin::Hermes {
                scope,
                marker: state::PROJECTS_MARKER,
            },
            leaves: &leaves,
        },
    };
    let Some(snapshot) = snapshot_config_database(&lookup, boundary, access)? else {
        return Ok(());
    };
    let original = snapshot.directory.path().join("snapshot.db");
    let output_path = snapshot.directory.path().join("projects.db");
    let output = rusqlite::Connection::open(&output_path)?;
    output.execute(
        "ATTACH DATABASE ?1 AS original",
        [original.to_string_lossy().as_ref()],
    )?;
    let transaction = output.unchecked_transaction()?;
    for &(table, empty_schema) in PROJECT_TABLES {
        let schema: Option<String> = transaction
            .query_row(
                "SELECT sql FROM original.sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .optional()?;
        transaction.execute_batch(schema.as_deref().unwrap_or(empty_schema))?;
        if schema.is_some() && table != "discovered_repos" {
            let condition = if table == "project_meta" {
                " WHERE key = 'active_id'"
            } else {
                ""
            };
            transaction.execute(
                &format!("INSERT INTO main.{table} SELECT * FROM original.{table}{condition}"),
                [],
            )?;
        }
    }
    // Keep the configuration tables' schema/index evolution, not unrelated
    // tables, cache rows, or triggers belonging to other native stores.
    let indices = {
        let mut query = transaction.prepare("SELECT sql FROM original.sqlite_schema WHERE type = 'index' AND sql IS NOT NULL AND tbl_name IN ('projects', 'project_folders', 'project_meta', 'discovered_repos')")?;
        let rows = query.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for sql in indices {
        transaction.execute_batch(&sql)?;
    }
    transaction.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_project_folders_path ON project_folders(path)",
    )?;
    for pragma in ["user_version", "application_id"] {
        let value: i64 =
            transaction.query_row(&format!("PRAGMA original.{pragma}"), [], |row| row.get(0))?;
        transaction.pragma_update(None, pragma, value)?;
    }
    transaction.commit()?;
    output.execute_batch("DETACH DATABASE original")?;
    drop(output);
    let mut file = snapshot
        .directory
        .open_regular(Path::new("projects.db"), usize::MAX)?
        .context("project projection disappeared before publication")?;
    publish_guarded_file(
        &mut file,
        &snapshot.guard,
        destination,
        Path::new("projects.db"),
        Permissions::from_mode(0o600),
        false,
    )?;
    Ok(())
}

pub(super) fn usage_controls(value: &Value, retained: &HashSet<String>, created_at: &str) -> Value {
    let mut projected = Map::new();
    for (name, record) in value.as_object().into_iter().flatten() {
        if !retained.contains(name) {
            continue;
        }
        let Some(record) = record.as_object() else {
            continue;
        };
        let mut controls = Map::new();
        for key in ["pinned", "agent_created"] {
            if let Some(value) = record.get(key).and_then(Value::as_bool) {
                controls.insert(key.to_owned(), Value::Bool(value));
            }
        }
        if let Some(state @ ("active" | "stale" | "archived")) =
            record.get("state").and_then(Value::as_str)
        {
            controls.insert("state".to_owned(), Value::String(state.to_owned()));
        }
        if let Some(author) = record.get("created_by") {
            let author = match author.as_str() {
                Some(author @ ("agent" | "installed")) => Value::String(author.to_owned()),
                _ => Value::Null,
            };
            controls.insert("created_by".to_owned(), author);
        }
        if !controls.is_empty() {
            controls.insert(
                "created_at".to_owned(),
                Value::String(created_at.to_owned()),
            );
            projected.insert(name.clone(), Value::Object(controls));
        }
    }
    Value::Object(projected)
}

#[cfg(test)]
mod tests {
    use super::super::super::super::AGENT_CONFIG_MOUNTS;
    use super::*;
    use std::os::unix::fs::symlink;

    fn boundary(source: &Path, destination: &Path) -> NativeStateBoundary {
        fs::create_dir_all(destination).unwrap();
        let mount = AGENT_CONFIG_MOUNTS
            .iter()
            .find(|mount| mount.tool_name == "hermes")
            .unwrap();
        NativeStateBoundary::for_fixture(source, destination, mount).unwrap()
    }

    #[test]
    fn authored_skills_cross_while_native_skill_metadata_stays_behind() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("skills/authored")).unwrap();
        fs::write(source.join("skills/authored/SKILL.md"), b"AUTHORED_SKILL").unwrap();
        fs::write(
            source.join("skills/.usage.json"),
            br#"{"authored":{"last_session_id":"PRIVATE_ACTIVITY"}}"#,
        )
        .unwrap();
        fs::create_dir_all(source.join("skills/.hub")).unwrap();
        fs::write(source.join("skills/.hub/audit.log"), b"PRIVATE_AUDIT").unwrap();
        fs::write(
            source.join("skills/.hub/lock.json"),
            br#"{"installed":{"install_path":"/host/private"}}"#,
        )
        .unwrap();
        fs::write(
            source.join("skills/.bundled_manifest"),
            b"bundled:0123456789abcdef\n",
        )
        .unwrap();
        fs::write(
            source.join("skills/.bundled_manifest_aplgomqo.tmp"),
            b"bundled:0123456789abcdef\n",
        )
        .unwrap();
        let destination = temporary.path().join("active");
        let boundary = boundary(&source, &destination);
        let output = AnchoredDir::open(&destination).unwrap();
        let retained = seed_skills(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &output,
            true,
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.join("skills/authored/SKILL.md")).unwrap(),
            b"AUTHORED_SKILL"
        );
        assert!(!destination.join("skills/.usage.json").exists());
        assert!(!destination.join("skills/.hub/audit.log").exists());
        assert!(!destination.join("skills/.hub/lock.json").exists());
        assert!(!destination.join("skills/.bundled_manifest").exists());
        assert!(!destination
            .join("skills/.bundled_manifest_aplgomqo.tmp")
            .exists());
        assert!(
            !destination.join("skills/.hub").exists(),
            "a state container name must not be exported"
        );
        assert_eq!(retained, HashSet::from(["authored".to_owned()]));
    }

    #[test]
    fn committed_nodes_are_private_config_but_other_workspace_names_are_not() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("workspace/meetings")).unwrap();
        let nodes = source.join("workspace/meetings/nodes.json");
        let content = br#"{"nodes":[{"id":"remote","token":"portable"}]}"#;
        fs::write(&nodes, content).unwrap();
        fs::set_permissions(&nodes, Permissions::from_mode(0o644)).unwrap();
        fs::write(
            source.join("workspace/meetings/auth.json"),
            b"BROWSER_PRIVATE",
        )
        .unwrap();
        let destination = temporary.path().join("active");
        let boundary = boundary(&source, &destination);
        let root = AnchoredDir::open(&destination).unwrap();
        seed_nodes(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &root,
        )
        .unwrap();
        let published = destination.join("workspace/meetings/nodes.json");
        assert_eq!(fs::read(&published).unwrap(), content);
        assert_eq!(
            fs::metadata(&published).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!destination.join("workspace/meetings/auth.json").exists());
        fs::write(&published, b"LOCAL_NODE_CHOICE").unwrap();
        seed_nodes(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &root,
        )
        .unwrap();
        assert_eq!(fs::read(&published).unwrap(), b"LOCAL_NODE_CHOICE");
        drop(boundary);

        fs::hard_link(&nodes, source.join("workspace/meetings/node_token.json")).unwrap();
        let rejected = temporary.path().join("rejected");
        let boundary = self::boundary(&source, &rejected);
        seed_nodes(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &AnchoredDir::open(&rejected).unwrap(),
        )
        .unwrap();
        assert!(!rejected.join("workspace").exists());
        drop(boundary);
        fs::remove_file(source.join("workspace/meetings/node_token.json")).unwrap();
        fs::hard_link(&nodes, temporary.path().join("authored-node-config")).unwrap();
        let authored = temporary.path().join("authored");
        let boundary = self::boundary(&source, &authored);
        seed_nodes(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &AnchoredDir::open(&authored).unwrap(),
        )
        .unwrap();
        assert_eq!(
            fs::read(authored.join("workspace/meetings/nodes.json")).unwrap(),
            content
        );
    }

    /// A native symlink inside the store onto a single-link nodes file denies its projection;
    /// a symlink authored outside the store does not.
    #[test]
    #[serial_test::serial]
    fn single_link_nodes_are_projected_only_without_a_native_alias() {
        use std::os::unix::fs::MetadataExt;

        // (link relative to the temp root, link target, projected)
        for (link, target, projected) in [
            (
                "source/sessions/ref",
                "../workspace/meetings/nodes.json",
                false,
            ),
            ("source/workspace/meetings/other", "nodes.json", false),
            (
                "authored-node-config",
                "source/workspace/meetings/nodes.json",
                true,
            ),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
            let source = temporary.path().join("source");
            fs::create_dir_all(source.join("workspace/meetings")).unwrap();
            fs::create_dir(source.join("sessions")).unwrap();
            let nodes = source.join("workspace/meetings/nodes.json");
            let content = br#"{"nodes":[{"id":"remote","token":"native-context"}]}"#;
            fs::write(&nodes, content).unwrap();
            let link = temporary.path().join(link);
            let target = if projected {
                temporary.path().join(target)
            } else {
                PathBuf::from(target)
            };
            symlink(&target, &link).unwrap();
            assert_eq!(fs::metadata(&nodes).unwrap().nlink(), 1);
            let destination = temporary.path().join("active");
            let boundary = boundary(&source, &destination);
            seed_nodes(
                &boundary,
                boundary.hermes.source.unwrap(),
                &boundary.source_root,
                &AnchoredDir::open(&destination).unwrap(),
            )
            .unwrap();
            assert_eq!(
                destination.join("workspace").exists(),
                projected,
                "{link:?}"
            );
            let seeded = fs::read(destination.join("workspace/meetings/nodes.json")).ok();
            assert_eq!(
                seeded.as_deref(),
                projected.then_some(&content[..]),
                "{link:?}"
            );
            assert_eq!(fs::read(&nodes).unwrap(), content);
            assert_eq!(fs::read(&link).unwrap(), content);
            assert_eq!(fs::read_link(&link).unwrap(), target);
        }
    }
    #[test]
    fn canonical_profile_aliases_cannot_borrow_each_others_mixed_exception() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let coder = source.join("profiles/coder");
        fs::create_dir_all(coder.join("workspace/meetings")).unwrap();
        fs::write(
            coder.join("workspace/meetings/nodes.json"),
            b"OTHER_PROFILE_CONTEXT",
        )
        .unwrap();
        fs::write(coder.join("auth.json"), b"PORTABLE_AUTH").unwrap();
        symlink("coder", source.join("profiles/alias")).unwrap();
        let destination = temporary.path().join("active");
        let boundary = boundary(&source, &destination);
        let scope = boundary
            .hermes
            .homes
            .iter()
            .position(|scope| {
                scope.lookup == super::super::super::canonical_expected_path(&coder).unwrap()
            })
            .unwrap();
        let profile = guard::SourceRoot::new(&coder).unwrap();
        let output = AnchoredDir::open(&destination).unwrap();
        seed_nodes(&boundary, scope, &profile, &output).unwrap();
        assert!(!destination.join("workspace").exists());
        super::super::super::publish_source_file(
            &coder.join("auth.json"),
            &output,
            Path::new("auth.json"),
            &boundary,
            false,
            ReadAccess::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.join("auth.json")).unwrap(),
            b"PORTABLE_AUTH"
        );
    }

    fn project_journal_fixture(source: &Path, mode: &str) -> rusqlite::Connection {
        fs::create_dir_all(source).unwrap();
        let database = rusqlite::Connection::open(source.join("projects.db")).unwrap();
        database.execute_batch(&format!("PRAGMA journal_mode={mode}; PRAGMA page_size=1024; PRAGMA cache_size=2; PRAGMA cache_spill=ON;")).unwrap();
        for &(_, schema) in PROJECT_TABLES {
            database.execute_batch(schema).unwrap();
        }
        database.execute_batch("INSERT INTO projects(id,slug,name,created_at) VALUES('project','workspace','Workspace',1); INSERT INTO discovered_repos VALUES('/PRIVATE_REPO','CACHE_PRIVATE',1); CREATE TABLE history(id INTEGER PRIMARY KEY, secret TEXT, padding BLOB); WITH RECURSIVE rows(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM rows WHERE id<128) INSERT INTO history SELECT id, 'UNRELATED_PRIVATE', zeroblob(900) FROM rows;").unwrap();
        database
    }

    fn assert_project_projection(destination: &Path) {
        let projected = rusqlite::Connection::open(destination.join("projects.db")).unwrap();
        assert_eq!(
            projected
                .query_row("SELECT name FROM projects WHERE id='project'", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "Workspace"
        );
        assert_eq!(
            projected
                .query_row("SELECT COUNT(*) FROM discovered_repos", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            projected
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name='history'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn project_configuration_accepts_inactive_rollback_journals_without_changing_source() {
        for mode in ["PERSIST", "TRUNCATE"] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source");
            let _database = project_journal_fixture(&source, mode);
            let database_before = fs::read(source.join("projects.db")).unwrap();
            let journal_before = fs::read(source.join("projects.db-journal")).unwrap();
            if mode == "PERSIST" {
                assert!(journal_before.len() >= 28);
                assert_eq!(&journal_before[..28], &[0; 28]);
            } else {
                assert!(journal_before.is_empty());
            }
            let destination = temporary.path().join("active");
            let boundary = boundary(&source, &destination);
            seed_projects(
                &boundary,
                boundary.hermes.source.unwrap(),
                &boundary.source_root,
                &AnchoredDir::open(&destination).unwrap(),
            )
            .unwrap();
            assert_eq!(
                fs::read(source.join("projects.db")).unwrap(),
                database_before,
                "{mode}"
            );
            assert_eq!(
                fs::read(source.join("projects.db-journal")).unwrap(),
                journal_before,
                "{mode}"
            );
            assert_project_projection(&destination);
        }
    }

    #[test]
    fn project_configuration_refuses_spilled_pages_and_retries_after_rollback() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let database = project_journal_fixture(&source, "DELETE");
        let committed = fs::read(source.join("projects.db")).unwrap();
        database
            .execute_batch("BEGIN IMMEDIATE; UPDATE history SET secret='UNCOMMITTED_PRIVATE';")
            .unwrap();
        let database_before = fs::read(source.join("projects.db")).unwrap();
        let journal_before = fs::read(source.join("projects.db-journal")).unwrap();
        assert_ne!(
            database_before, committed,
            "the transaction must spill to the main file"
        );
        assert!(journal_before.len() >= 28 && journal_before[..28].iter().any(|byte| *byte != 0));
        let destination = temporary.path().join("active");
        let source_boundary = boundary(&source, &destination);
        let output = AnchoredDir::open(&destination).unwrap();
        let result = seed_projects(
            &source_boundary,
            source_boundary.hermes.source.unwrap(),
            &source_boundary.source_root,
            &output,
        );
        assert!(
            result.is_err(),
            "an active journal must prevent publication: {result:?}"
        );
        assert!(!destination.join("projects.db").exists());
        assert_eq!(
            fs::read(source.join("projects.db")).unwrap(),
            database_before
        );
        assert_eq!(
            fs::read(source.join("projects.db-journal")).unwrap(),
            journal_before
        );
        database.execute_batch("ROLLBACK;").unwrap();
        let source_boundary = boundary(&source, &destination);
        seed_projects(
            &source_boundary,
            source_boundary.hermes.source.unwrap(),
            &source_boundary.source_root,
            &output,
        )
        .unwrap();
        assert_project_projection(&destination);
    }

    #[test]
    fn project_journal_cannot_borrow_another_native_states_exception() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let _database = project_journal_fixture(&source, "PERSIST");
        fs::create_dir(source.join("sessions")).unwrap();
        let journal = source.join("projects.db-journal");
        let alias = source.join("sessions/native-journal");
        fs::hard_link(&journal, &alias).unwrap();
        let database_before = fs::read(source.join("projects.db")).unwrap();
        let journal_before = fs::read(&journal).unwrap();
        let destination = temporary.path().join("active");
        let boundary = boundary(&source, &destination);
        let result = seed_projects(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &AnchoredDir::open(&destination).unwrap(),
        );
        assert!(
            result.is_err(),
            "a journal aliased to native state must fail: {result:?}"
        );
        assert!(!destination.join("projects.db").exists());
        assert_eq!(
            fs::read(source.join("projects.db")).unwrap(),
            database_before
        );
        assert_eq!(fs::read(&journal).unwrap(), journal_before);
        assert_eq!(fs::read(&alias).unwrap(), journal_before);
    }

    #[test]
    fn project_wal_configuration_survives_without_cache_or_unrelated_database_state() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir(&source).unwrap();
        let database = rusqlite::Connection::open(source.join("projects.db")).unwrap();
        database
            .execute_batch("PRAGMA journal_mode = WAL;")
            .unwrap();
        for &(_, schema) in PROJECT_TABLES {
            database.execute_batch(schema).unwrap();
        }
        database.execute_batch("ALTER TABLE projects ADD COLUMN custom_color TEXT; INSERT INTO projects(id,slug,name,created_at,custom_color) VALUES('project','workspace','Workspace',1,'violet'); INSERT INTO project_folders(project_id,path,added_at) VALUES('project','/code/project',1); INSERT INTO project_meta VALUES('active_id','project'),('repo_discovery_policy','CACHE_PRIVATE'); INSERT INTO discovered_repos VALUES('/PRIVATE_REPO','CACHE_PRIVATE',1); CREATE TABLE history(secret TEXT); INSERT INTO history VALUES('UNRELATED_PRIVATE');").unwrap();
        let originals: Vec<_> = ["projects.db", "projects.db-wal", "projects.db-shm"]
            .into_iter()
            .map(|name| (name, fs::read(source.join(name)).unwrap()))
            .collect();
        let destination = temporary.path().join("active");
        let boundary = boundary(&source, &destination);
        let output = AnchoredDir::open(&destination).unwrap();
        seed_projects(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &output,
        )
        .unwrap();
        for (name, bytes) in originals {
            assert_eq!(
                fs::read(source.join(name)).unwrap(),
                bytes,
                "original {name} must remain untouched"
            );
        }
        let projected = rusqlite::Connection::open(destination.join("projects.db")).unwrap();
        assert_eq!(
            projected
                .query_row(
                    "SELECT name || ':' || custom_color FROM projects",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "Workspace:violet"
        );
        assert_eq!(
            projected
                .query_row("SELECT path FROM project_folders", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "/code/project"
        );
        assert_eq!(
            projected
                .query_row(
                    "SELECT value FROM project_meta WHERE key='active_id'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "project"
        );
        assert_eq!(
            projected
                .query_row("SELECT COUNT(*) FROM discovered_repos", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            projected
                .query_row(
                    "SELECT COUNT(*) FROM project_meta WHERE key='repo_discovery_policy'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            projected
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name='history'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        let bytes = fs::read(destination.join("projects.db")).unwrap();
        for forbidden in [
            b"CACHE_PRIVATE".as_slice(),
            b"UNRELATED_PRIVATE",
            b"PRIVATE_REPO",
        ] {
            assert!(!bytes
                .windows(forbidden.len())
                .any(|window| window == forbidden));
        }
        projected
            .execute("UPDATE projects SET name='Local workspace'", [])
            .unwrap();
        drop(projected);
        let local = fs::read(destination.join("projects.db")).unwrap();
        database
            .execute("UPDATE projects SET name='Changed host workspace'", [])
            .unwrap();
        seed_projects(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &output,
        )
        .unwrap();
        assert_eq!(fs::read(destination.join("projects.db")).unwrap(), local);
    }

    #[test]
    fn skill_controls_drop_activity_and_orphan_names_and_do_not_waive_atomic_remnants() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("skills")).unwrap();
        let usage = serde_json::json!({ "kept": { "pinned": true, "state": "stale", "created_by": "agent", "agent_created": false, "created_at": "2000-01-01T00:00:00+00:00", "use_count": 42, "last_session_id": "PRIVATE_ACTIVITY" }, "PRIVATE_ORPHAN": { "pinned": true } });
        fs::write(
            source.join("skills/.usage.json"),
            serde_json::to_vec(&usage).unwrap(),
        )
        .unwrap();
        fs::write(
            source.join("skills/.curator_state"),
            br#"{"paused":true,"last_report":"PRIVATE_ACTIVITY"}"#,
        )
        .unwrap();
        let retained = HashSet::from(["kept".to_owned()]);
        let destination = temporary.path().join("active");
        let boundary = boundary(&source, &destination);
        seed_skill_controls(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &AnchoredDir::open(&destination).unwrap(),
            &retained,
        )
        .unwrap();
        let mut projected: Value =
            serde_json::from_slice(&fs::read(destination.join("skills/.usage.json")).unwrap())
                .unwrap();
        let created = projected["kept"]
            .as_object_mut()
            .unwrap()
            .remove("created_at")
            .unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(created.as_str().unwrap())
                .unwrap()
                .timestamp()
                > 946684800
        );
        assert_eq!(
            projected,
            serde_json::json!({ "kept": { "pinned": true, "state": "stale", "created_by": "agent", "agent_created": false } })
        );
        assert_eq!(
            serde_json::from_slice::<Value>(
                &fs::read(destination.join("skills/.curator_state")).unwrap()
            )
            .unwrap(),
            serde_json::json!({ "paused": true })
        );
        drop(boundary);
        fs::hard_link(
            source.join("skills/.usage.json"),
            source.join("skills/.usage_abcdefgh.tmp"),
        )
        .unwrap();
        let rejected = temporary.path().join("rejected");
        let boundary = self::boundary(&source, &rejected);
        seed_skill_controls(
            &boundary,
            boundary.hermes.source.unwrap(),
            &boundary.source_root,
            &AnchoredDir::open(&rejected).unwrap(),
            &retained,
        )
        .unwrap();
        assert!(!rejected.join("skills/.usage.json").exists());
        assert_eq!(
            serde_json::from_slice::<Value>(
                &fs::read(rejected.join("skills/.curator_state")).unwrap()
            )
            .unwrap(),
            serde_json::json!({ "paused": true })
        );
    }
}

pub(in super::super) fn seed_skill_controls(
    boundary: &NativeStateBoundary,
    scope: usize,
    source: &guard::SourceRoot,
    destination: &AnchoredDir,
    retained: &HashSet<String>,
) -> Result<()> {
    let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false);
    for (relative, marker) in [
        ("skills/.usage.json", state::SKILLS_USAGE_MARKER),
        ("skills/.curator_state", state::SKILLS_CURATOR_MARKER),
    ] {
        let relative = Path::new(relative);
        let local = optional_child(destination, Path::new("skills"))?;
        let leaf = Path::new(relative.file_name().context("skill metadata has no name")?);
        if let Some(local) = &local {
            if local.regular_lookup(leaf)?.is_some() {
                continue;
            }
        }
        let lookup = source.path().join(relative);
        let canonical = match fs::canonicalize(&lookup) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("resolving native skill controls"),
        };
        let leaves = [canonical];
        let access = ReadAccess {
            root: Some(source),
            exception: Exception::Mixed {
                origin: StateOrigin::Hermes { scope, marker },
                leaves: &leaves,
            },
        };
        let Some(canonical) = canonical_source(&lookup, boundary, false, access)? else {
            continue;
        };
        let Some(mut file) = open_canonical_file(&canonical, access)? else {
            continue;
        };
        let mut guard = guard::ReadGuard::new(boundary, access)?;
        guard.record_route(&lookup, &canonical)?;
        if !guard.record_file(&canonical, &file)? {
            continue;
        }

        let value: Value = match serde_json::from_reader(&mut file) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let value = if marker == state::SKILLS_USAGE_MARKER {
            usage_controls(&value, retained, &created_at)
        } else {
            let Some(paused) = value.get("paused").and_then(Value::as_bool) else {
                continue;
            };
            serde_json::json!({ "paused": paused })
        };
        if value.as_object().is_none_or(Map::is_empty) {
            continue;
        }
        let local = match local {
            Some(local) => local,
            None => destination.create_child(Path::new("skills"))?,
        };
        let bytes = serde_json::to_vec_pretty(&value)?;
        let validate = || guard.validate();
        local.publish_file(
            leaf,
            &mut bytes.as_slice(),
            Permissions::from_mode(0o600),
            false,
            Some(FilePublication {
                staging: &boundary.private_stage.anchor,
                validate: &validate,
            }),
        )?;
    }
    Ok(())
}

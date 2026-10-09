//! Historical Create native authority cannot be reconstructed from row status or journal absence.
use anyhow::{Context, Result};
use std::fs;

pub fn run() -> Result<()> {
    tracing::info!(target: "migrations", "v039: preserving historical Create native uncertainty");
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    let _namespace = crate::session::acquire_profile_namespace_lock()?;
    let app = crate::session::get_app_dir()?;
    let mut paths = crate::session::list_profiles_for_worktree_inventory()?
        .into_iter()
        .map(|profile| app.join("profiles").join(profile).join("sessions.json"))
        .collect::<Vec<_>>();
    paths.push(app.join("sessions.json"));
    for path in paths {
        let directory = path.parent().context("sessions file has no parent")?;
        let original = fs::File::open(directory)?;
        let _lock = crate::session::acquire_storage_flock(
            directory,
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        anyhow::ensure!(
            crate::session::same_filesystem_identity(
                &original.metadata()?,
                &fs::metadata(directory)?
            ),
            "profile changed during original Create coverage migration"
        );
        let content = match fs::read_to_string(&path) {
            Ok(content) if content.trim().is_empty() => continue,
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    _ => anyhow::bail!("unreadable original sessions file {}", path.display()),
                }
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let mut document = crate::session::raw_document::RawDocument::parse(&content)?;
        if migrate_document(&mut document)? {
            crate::session::backup_before_migration(&path)?;
            crate::session::atomic_write(&path, &serde_json::to_vec_pretty(&document.rows)?)?;
            crate::session::sync_parent_directory(&path)?;
        }
    }
    Ok(())
}
fn migrate_document(document: &mut crate::session::raw_document::RawDocument) -> Result<bool> {
    use crate::session::raw_document::{patch, Emission, RawObject};
    let mut changed = false;
    for row in &mut document.rows {
        let Ok(fields) = RawObject::parse(row) else {
            continue;
        };
        let Ok(journal) = fields.unique("runner_journal") else {
            continue;
        };
        let (before, after) = match journal {
            None => (
                serde_json::json!({}),
                serde_json::json!({"runner_journal":
                serde_json::to_value(crate::session::runner_journal::RunnerExecutionJournal::default())?}),
            ),
            Some(journal) => {
                let Ok(journal) = RawObject::parse(journal) else {
                    continue;
                };
                let Ok(coverage) = journal.unique("create_coverage") else {
                    continue;
                };
                let Ok(creations) = journal.unique("creations") else {
                    continue;
                };
                if coverage.is_some() {
                    continue;
                }
                let mut additions = serde_json::json!({"create_coverage":"unknown"});
                if creations.is_none() {
                    additions["creations"] = serde_json::json!([]);
                }
                (
                    serde_json::json!({"runner_journal":{}}),
                    serde_json::json!({"runner_journal":additions}),
                )
            }
        };
        if let Emission::Changed(updated) = patch(row, &before, &after)? {
            *row = updated;
            changed = true;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::raw_document::{RawDocument, RawObject};
    #[test]
    fn historical_journals_never_mint_missing_original_create_births() {
        let opaque = r#"{"duplicate":1,"duplicate":2,"huge":1e400,"literal":"\u0061"}"#;
        let old = format!(
            r#"{{"id":"same","runner_journal":{{"coverage":"complete","launches":[],"preparations":[],"extension":{opaque}}},"extension":{opaque}}}"#
        );
        let ambiguous = r#"{"id":"same","runner_journal":{"coverage":"complete","coverage":"unknown"},"runner_journal":{}}"#;
        let mut document = RawDocument::parse(&format!("[{old},{ambiguous}]")).unwrap();
        assert!(migrate_document(&mut document).unwrap());
        assert_eq!(document.rows[1].get(), ambiguous);
        let fields = RawObject::parse(&document.rows[0]).unwrap();
        assert_eq!(fields.unique("extension").unwrap().unwrap().get(), opaque);
        let journal = RawObject::parse(fields.unique("runner_journal").unwrap().unwrap()).unwrap();
        assert_eq!(journal.unique("extension").unwrap().unwrap().get(), opaque);
        assert_eq!(
            journal.unique("create_coverage").unwrap().unwrap().get(),
            r#""unknown""#
        );
        let parsed: crate::session::runner_journal::RunnerExecutionJournal =
            serde_json::from_str(fields.unique("runner_journal").unwrap().unwrap().get()).unwrap();
        assert!(!parsed.proves_quiescent());
        assert!(
            parsed.proves_runner_quiescent(),
            "real historical runner proof remains usable for non-destructive admission"
        );
        let before = serde_json::to_vec(&document.rows).unwrap();
        assert!(!migrate_document(&mut document).unwrap());
        assert_eq!(serde_json::to_vec(&document.rows).unwrap(), before);
    }
}

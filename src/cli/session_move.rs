//! `agent-of-empires session move`: transfer sessions to another profile.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::collections::BTreeMap;

use crate::session::{
    acquire_session_identity_lock, acquire_session_title_lock, duplicate_session_error,
    is_duplicate_session, list_profiles, GroupMovePlan, Instance, Storage,
};

#[derive(Args)]
pub struct MoveArgs {
    /// Session IDs or titles to move
    #[arg(required = true)]
    identifiers: Vec<String>,

    /// Destination profile; it must already exist
    #[arg(long)]
    to_profile: String,
}

pub async fn run(profile: &str, args: MoveArgs) -> Result<()> {
    let target_name = args.to_profile.as_str();
    if !list_profiles()?.iter().any(|p| p == target_name) {
        bail!("Profile '{target_name}' does not exist");
    }
    let source = Storage::open_unwatched(profile)?;
    if source.profile() == target_name {
        bail!("Sessions are already in profile '{target_name}'");
    }
    let target = Storage::open_unwatched(target_name)?;

    let instances = source.load()?;
    let mut ids = Vec::with_capacity(args.identifiers.len());
    for identifier in &args.identifiers {
        let id = super::resolve_session(identifier, &instances)?.id.clone();
        if ids.contains(&id) {
            bail!("Session {id} was named more than once");
        }
        ids.push(id);
    }
    ids.sort();

    // Same order as the TUI group move: identity, then title and lifecycle per session.
    let _identity_lock = acquire_session_identity_lock()?;
    let mut guards = Vec::with_capacity(ids.len() * 2);
    for id in &ids {
        guards
            .push(acquire_session_title_lock(id).context("failed to acquire session title lock")?);
        guards.push(
            source
                .acquire_instance_lifecycle_lock(id)
                .context("failed to acquire session lifecycle lock")?,
        );
    }

    let authoritative = source.load()?;
    crate::tmux::refresh_session_cache();
    let mut batch = Vec::with_capacity(ids.len());
    for id in &ids {
        let mut live = authoritative
            .iter()
            .find(|i| i.id == *id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?
            .clone();
        live.source_profile = source.profile().to_string();
        live.update_status_with_metadata(None, None);
        refuse_if_busy(&live)?;
        batch.push(live);
    }
    ensure_no_duplicates(&target.load()?, &batch)?;

    // One transaction per group path: GroupMovePlan carries a single path.
    let mut by_group: BTreeMap<&str, Vec<&Instance>> = BTreeMap::new();
    for inst in &batch {
        by_group.entry(&inst.group_path).or_default().push(inst);
    }
    let mut moved: Vec<&str> = Vec::new();
    for (group, members) in by_group {
        let changes: Vec<(Instance, Instance)> = members
            .iter()
            .map(|i| ((*i).clone(), (*i).clone()))
            .collect();
        let result = source.move_instances_to(
            &target,
            &changes,
            &GroupMovePlan::single(group, group),
            ensure_no_duplicates,
        );
        if let Err(error) = result {
            let remaining: Vec<&str> = batch
                .iter()
                .map(|i| i.id.as_str())
                .filter(|id| !moved.contains(id))
                .collect();
            if moved.is_empty() {
                return Err(error);
            }
            bail!(
                "{error}\nAlready moved to '{target_name}': {}\nNot moved: {}",
                moved.join(", "),
                remaining.join(", ")
            );
        }
        moved.extend(members.iter().map(|i| i.id.as_str()));
    }

    for inst in &batch {
        println!(
            "✓ Moved {} ({}) to profile '{target_name}'",
            inst.title, inst.id
        );
    }
    Ok(())
}

fn refuse_if_busy(inst: &Instance) -> Result<()> {
    if inst.status.blocks_worktree_edit()
        || inst.has_fresh_lifecycle_reservation(chrono::Utc::now())
    {
        bail!(
            "Cannot move session {} while it is active ({:?}); stop it first",
            inst.id,
            inst.status
        );
    }
    Ok(())
}

/// Rejects a candidate whose title and path match a target row or an earlier candidate.
fn ensure_no_duplicates(target_rows: &[Instance], candidates: &[Instance]) -> Result<()> {
    for (index, candidate) in candidates.iter().enumerate() {
        let clash = is_duplicate_session(
            target_rows.iter(),
            &candidate.title,
            &candidate.project_path,
            None,
        ) || is_duplicate_session(
            candidates[..index].iter(),
            &candidate.title,
            &candidate.project_path,
            None,
        );
        if clash {
            return Err(duplicate_session_error(&candidate.title));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Status;
    use serial_test::serial;

    fn seed(profile: &str, rows: Vec<Instance>) -> Storage {
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|instances, _| {
                *instances = rows;
                Ok(())
            })
            .unwrap();
        storage
    }

    fn args(identifiers: &[&str], to: &str) -> MoveArgs {
        MoveArgs {
            identifiers: identifiers.iter().map(|s| s.to_string()).collect(),
            to_profile: to.to_string(),
        }
    }

    fn grouped(title: &str, path: &str, group: &str) -> Instance {
        let mut inst = Instance::new(title, path);
        inst.group_path = group.to_string();
        inst
    }

    #[tokio::test]
    #[serial]
    async fn moves_sessions_across_groups_and_keeps_the_rest() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let (a, b, c) = (
            grouped("task-a", "/tmp/a", "infra/prod"),
            grouped("task-b", "/tmp/b", ""),
            grouped("task-c", "/tmp/c", "infra/prod"),
        );
        let (a_id, b_id) = (a.id.clone(), b.id.clone());
        let source = seed("move-src", vec![a, b, c]);
        let target = seed("move-dst", vec![]);

        run("move-src", args(&[&a_id, "task-b"], "move-dst"))
            .await
            .unwrap();

        let left: Vec<String> = source
            .load()
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert_eq!(left, ["task-c"]);
        let (moved, groups) = target.load_with_groups().unwrap();
        let mut ids: Vec<&str> = moved.iter().map(|i| i.id.as_str()).collect();
        ids.sort();
        let mut want = [a_id.as_str(), b_id.as_str()];
        want.sort();
        assert_eq!(ids, want);
        assert!(moved.iter().all(|i| i.source_profile == "move-dst"));
        assert!(groups.iter().any(|g| g.path == "infra/prod"));
    }

    #[tokio::test]
    #[serial]
    async fn refuses_before_moving_anything() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let a = Instance::new("task-a", "/tmp/a");
        let a_id = a.id.clone();
        let clash_id = Instance::new("same-id", "/tmp/clash");
        let mut dup_id = clash_id.clone();
        dup_id.title = "other-title".into();
        let source = seed(
            "refuse-src",
            vec![a, clash_id.clone(), Instance::new("dup", "/tmp/dup")],
        );
        seed(
            "refuse-dst",
            vec![dup_id, Instance::new("dup", "/tmp/dup/")],
        );

        let cases = [
            (
                vec![a_id.as_str(), a_id.as_str()],
                "refuse-dst",
                "more than once",
            ),
            (vec!["task-a"], "refuse-nope", "does not exist"),
            (vec!["task-a"], "refuse-src", "already in profile"),
            (vec!["task-a", "same-id"], "refuse-dst", "already exists"),
            (vec!["task-a", "dup"], "refuse-dst", "same title and path"),
        ];
        for (ids, to, expect) in cases {
            let error = run("refuse-src", args(&ids, to)).await.unwrap_err();
            assert!(error.to_string().contains(expect), "{ids:?}: {error}");
            assert_eq!(source.load().unwrap().len(), 3, "{ids:?}");
        }
    }

    #[test]
    fn busy_sessions_are_refused() {
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Starting,
            Status::Creating,
            Status::Deleting,
        ] {
            let mut inst = Instance::new("t", "/tmp/t");
            inst.status = status;
            assert!(refuse_if_busy(&inst).is_err(), "{status:?}");
        }
        assert!(refuse_if_busy(&Instance::new("t", "/tmp/t")).is_ok());
    }
}

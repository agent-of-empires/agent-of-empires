//! `agent-of-empires group` subcommands implementation

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::acp::client::require_daemon;
use crate::daemon::{
    ApiErrorCode, DaemonClient, DaemonClientError, DeleteGroupBody, DeleteGroupMode, GroupLocation,
    RuntimeHealth, RUNTIME_PROTOCOL_VERSION,
};
use crate::session::config::resolve_default_profile;
use crate::session::{GroupTree, Storage};

#[derive(Subcommand)]
pub enum GroupCommands {
    /// List all groups
    #[command(alias = "ls")]
    List(GroupListArgs),

    /// Create a new group
    Create(GroupCreateArgs),

    /// Delete a group
    Delete(GroupDeleteArgs),

    /// Move session to group
    Move(GroupMoveArgs),
}

#[derive(Args)]
pub struct GroupListArgs {
    /// Output as JSON
    #[arg(long)]
    pub(crate) json: bool,
}

impl GroupListArgs {
    pub(crate) fn json(&self) -> bool {
        self.json
    }
}

#[derive(Args)]
pub struct GroupCreateArgs {
    /// Group name
    name: String,

    /// Parent group for creating subgroups
    #[arg(long)]
    parent: Option<String>,
}

#[derive(Args)]
pub struct GroupDeleteArgs {
    /// Group name
    name: String,

    /// Force delete by moving sessions to default group
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
pub struct GroupMoveArgs {
    /// Session ID or title
    identifier: String,

    /// Target group
    group: String,
}

#[tracing::instrument(target = "cli.session", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, command: GroupCommands) -> Result<()> {
    match command {
        GroupCommands::List(_) => {
            anyhow::bail!("internal error: runtime read reached local dispatch")
        }
        GroupCommands::Create(args) => create_group(profile, args).await,
        GroupCommands::Delete(args) => delete_group(profile, args).await,
        GroupCommands::Move(args) => move_session(profile, args).await,
    }
}

async fn daemon_client() -> Result<(DaemonClient, String)> {
    let client = require_daemon().await?.daemon_client()?;
    let info = client.runtime_info().await?;
    anyhow::ensure!(
        info.protocol_version == RUNTIME_PROTOCOL_VERSION && info.health == RuntimeHealth::Healthy,
        "Daemon runtime is not ready"
    );
    Ok((client, info.epoch))
}

fn group_location(profile: &str, path: &str) -> GroupLocation {
    let profile = if profile.is_empty() {
        resolve_default_profile()
    } else {
        profile.to_string()
    };
    GroupLocation {
        profile,
        path: path.to_string(),
    }
}

async fn create_group(profile: &str, args: GroupCreateArgs) -> Result<()> {
    let name = args.name.trim();
    let group_path = match args.parent.as_deref().map(str::trim) {
        Some(parent) if !parent.is_empty() => format!("{parent}/{name}"),
        _ => name.to_string(),
    };

    let (client, epoch) = daemon_client().await?;
    client
        .create_group(&group_location(profile, &group_path), &epoch)
        .await?;

    println!("✓ Created group: {}", group_path);
    Ok(())
}

async fn delete_group(profile: &str, args: GroupDeleteArgs) -> Result<()> {
    let name = args.name.trim().to_string();
    let force = args.force;

    let (client, epoch) = daemon_client().await?;
    let body = DeleteGroupBody {
        group: group_location(profile, &name),
        mode: if force {
            DeleteGroupMode::KeepSessions
        } else {
            DeleteGroupMode::EmptyOnly
        },
        cleanup: Default::default(),
    };
    let receipt = client.delete_group(&body, &epoch).await.map_err(|e| {
        let not_empty = matches!(
            &e,
            DaemonClientError::Status {
                code: Some(ApiErrorCode::GroupNotEmpty),
                ..
            }
        );
        if !force && not_empty {
            anyhow::Error::from(e).context(format!(
                "Group '{name}' is not empty. Use --force to move its sessions to default group."
            ))
        } else {
            e.into()
        }
    })?;

    println!("✓ Deleted group: {}", name);
    let moved = receipt.outcome.sessions.len();
    if force && moved > 0 {
        println!("  Moved {} sessions to default group", moved);
    }

    Ok(())
}

async fn move_session(profile: &str, args: GroupMoveArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let identifier = args.identifier.trim().to_string();
    let group = args.group.trim().to_string();

    let old_group = storage.update(|instances, groups| {
        let old = super::patch_instance(instances, &identifier, |inst| {
            let old = inst.group_path.clone();
            inst.group_path = group.clone();
            Ok(old)
        })?;

        if !group.is_empty() {
            let mut group_tree = GroupTree::new_with_groups(instances, groups);
            group_tree.create_group(&group);
            *groups = group_tree.get_all_groups();
        }
        Ok(old)
    })?;

    if old_group.is_empty() {
        println!("✓ Moved session to group: {}", group);
    } else {
        println!("✓ Moved session from '{}' to '{}'", old_group, group);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn empty_profile_resolves_to_the_configured_default() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(home.path());
        crate::session::create_profile("configured").unwrap();
        crate::session::config::update_config(|c| c.default_profile = "configured".into()).unwrap();

        assert_eq!(group_location("", "team").profile, "configured");
        assert_eq!(group_location("work", "team").profile, "work");
    }
}

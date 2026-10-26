//! `agent-of-empires group` subcommands implementation

use anyhow::{bail, Result};
use clap::{Args, Subcommand};

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
        GroupCommands::List(_) => bail!("internal error: runtime read reached local dispatch"),
        GroupCommands::Create(args) => create_group(profile, args).await,
        GroupCommands::Delete(args) => delete_group(profile, args).await,
        GroupCommands::Move(args) => move_session(profile, args).await,
    }
}

async fn create_group(profile: &str, args: GroupCreateArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;

    let name = args.name.trim().to_string();
    let group_path = if let Some(parent) = &args.parent {
        format!("{}/{}", parent.trim(), name)
    } else {
        name.clone()
    };

    storage.update(|instances, groups| {
        let mut group_tree = GroupTree::new_with_groups(instances, groups);
        if group_tree.group_exists(&group_path) {
            bail!("Group already exists: {}", group_path);
        }
        group_tree.create_group(&group_path);
        *groups = group_tree.get_all_groups();
        Ok(())
    })?;

    println!("✓ Created group: {}", group_path);
    Ok(())
}

async fn delete_group(profile: &str, args: GroupDeleteArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let name = args.name.trim().to_string();
    let force = args.force;

    let session_count = storage.update(|instances, groups| {
        let mut group_tree = GroupTree::new_with_groups(instances, groups);
        if !group_tree.group_exists(&name) {
            bail!("Group not found: {}", name);
        }

        let session_count = instances
            .iter()
            .filter(|i| i.group_path == name || i.group_path.starts_with(&format!("{}/", name)))
            .count();

        if session_count > 0 {
            if !force {
                bail!(
                    "Group '{}' contains {} sessions. Use --force to move them to default group.",
                    name,
                    session_count
                );
            }

            for inst in instances.iter_mut() {
                if inst.group_path == name || inst.group_path.starts_with(&format!("{}/", name)) {
                    inst.group_path = String::new();
                }
            }
        }

        group_tree.delete_group(&name);
        *groups = group_tree.get_all_groups();
        Ok(session_count)
    })?;

    println!("✓ Deleted group: {}", name);
    if force && session_count > 0 {
        println!("  Moved {} sessions to default group", session_count);
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

//! `aoe skill` CLI for discovering and managing agent skills.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use reqwest::Method;

use crate::daemon::transport::path_segment;

use crate::session::skills_model::{self, SkillError, SkillProvenance};

#[derive(Subcommand, Debug)]
pub enum SkillCommands {
    /// List discovered skills and their source roots.
    List(SkillListArgs),
    /// Print one skill's SKILL.md.
    View(SkillViewArgs),
    /// Create a new AoE-managed skill.
    Add(SkillAddArgs),
    /// Edit an AoE-managed skill.
    Edit(SkillEditArgs),
    /// Copy an external skill into AoE's managed store.
    Adopt(SkillAdoptArgs),
    /// Delete an AoE-managed skill.
    Remove(SkillRemoveArgs),
    /// Copy AoE-managed skills into the agents' own skills directories.
    Sync(SkillSyncArgs),
}

impl SkillCommands {
    /// Writes go through the Daemon; list and view read local files.
    pub fn writes(&self) -> bool {
        !matches!(self, Self::List(_) | Self::View(_))
    }
}

#[derive(Args, Debug)]
pub struct SkillListArgs {
    /// Output machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct SkillViewArgs {
    /// Skill directory name.
    directory: String,
    /// Source root id, or aoe-managed.
    #[arg(long, default_value = "aoe-managed")]
    source: String,
    /// Output metadata and content as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct SkillAddArgs {
    /// Skill directory name.
    directory: String,
    /// Short description used in the generated SKILL.md.
    #[arg(long)]
    description: Option<String>,
}

#[derive(Args, Debug)]
pub struct SkillEditArgs {
    /// Managed skill directory name.
    directory: String,
    /// Read replacement SKILL.md from this file. Use - for stdin.
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct SkillAdoptArgs {
    /// External source root id, such as claude-user or agents-standard.
    source: String,
    /// Source skill directory name.
    directory: String,
    /// Destination directory name in AoE's managed store.
    #[arg(long = "as")]
    destination: Option<String>,
}

#[derive(Args, Debug)]
pub struct SkillRemoveArgs {
    /// Managed skill directory name.
    directory: String,
}

#[derive(Args, Debug)]
pub struct SkillSyncArgs {
    /// Limit the sync to these source roots. Repeatable. Defaults to all of them.
    #[arg(long = "root", value_name = "ID")]
    roots: Vec<String>,
    /// Take over this skill in the agents' directories, overwriting a skill AoE
    /// does not manage or a propagated copy that was edited there. Repeatable.
    /// Without it a sync never overwrites anything it did not itself write.
    #[arg(long = "replace", value_name = "DIRECTORY")]
    replace: Vec<String>,
    /// Reconcile only this skill. Repeatable. Defaults to every managed skill.
    #[arg(long = "only", value_name = "DIRECTORY")]
    only: Vec<String>,
    /// Output the per-skill outcomes as JSON.
    #[arg(long)]
    json: bool,
}

pub async fn run(command: SkillCommands) -> Result<()> {
    match command {
        SkillCommands::List(args) => list(args),
        SkillCommands::View(args) => view(args),
        SkillCommands::Add(args) => add(args).await,
        SkillCommands::Edit(args) => edit(args).await,
        SkillCommands::Adopt(args) => adopt(args).await,
        SkillCommands::Remove(args) => remove(args).await,
        SkillCommands::Sync(args) => sync(args).await,
    }
}

fn list(args: SkillListArgs) -> Result<()> {
    let skills = skills_model::discover_all()?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "skills": skills,
                "roots": skills_model::skill_roots(),
            }))?
        );
        return Ok(());
    }
    if skills.is_empty() {
        println!("No skills found.");
        return Ok(());
    }
    println!("{:<24} {:<20} NAME", "DIRECTORY", "SOURCE");
    for skill in skills {
        println!(
            "{:<24} {:<20} {}",
            skill.directory,
            skill.provenance.label(),
            skill.name
        );
    }
    Ok(())
}

fn view(args: SkillViewArgs) -> Result<()> {
    let (home, app_dir) = skills_dirs()?;
    let provenance = parse_source(&args.source)?;
    let skill = skills_model::read_skill(&home, &app_dir, &provenance, &args.directory)
        .map_err(skill_error)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&skill)?);
    } else {
        print!("{}", skill.content);
    }
    Ok(())
}

async fn add(args: SkillAddArgs) -> Result<()> {
    let body = serde_json::json!({
        "directory": args.directory,
        "description": args.description,
    });
    super::daemon_client()
        .await?
        .mutate(Method::POST, "/api/skills", Some(body))
        .await?;
    println!("Created managed skill {}.", args.directory);
    Ok(())
}

async fn edit(args: SkillEditArgs) -> Result<()> {
    let client = super::daemon_client().await?;
    let segment = path_segment(&args.directory)?;
    let content = match args.file {
        Some(path) if path.as_os_str() == "-" => read_stdin_capped()?,
        Some(path) => skills_model::read_file_capped(&path, skills_model::MAX_SKILL_MD_BYTES)
            .with_context(|| format!("failed to read {}", path.display()))?,
        None => {
            let skill = client
                .mutate(
                    Method::GET,
                    &format!("/api/skills/aoe-managed/{segment}"),
                    None,
                )
                .await?;
            let current = skill["content"].as_str().unwrap_or_default();
            edit_with_editor(current)?
        }
    };
    let body = serde_json::json!({ "content": content });
    client
        .mutate(Method::PUT, &format!("/api/skills/{segment}"), Some(body))
        .await?;
    println!("Updated managed skill {}.", args.directory);
    Ok(())
}

fn edit_with_editor(current: &str) -> Result<String> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("SKILL.md");
    std::fs::write(&path, current)?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = shell_words::split(&editor).context("failed to parse editor command")?;
    if parts.is_empty() {
        bail!("editor command is empty");
    }
    let program = parts.remove(0);
    let status = Command::new(program)
        .args(parts)
        .arg(&path)
        .status()
        .context("failed to launch editor")?;
    if !status.success() {
        bail!("editor exited with status {status}");
    }
    skills_model::read_file_capped(&path, skills_model::MAX_SKILL_MD_BYTES)
        .context("failed to read edited SKILL.md")
}

fn read_stdin_capped() -> Result<String> {
    let mut buf = Vec::new();
    std::io::stdin()
        .take(skills_model::MAX_SKILL_MD_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > skills_model::MAX_SKILL_MD_BYTES {
        bail!(
            "SKILL.md is too large: the limit is {} bytes",
            skills_model::MAX_SKILL_MD_BYTES
        );
    }
    String::from_utf8(buf).context("stdin is not valid UTF-8")
}

async fn adopt(args: SkillAdoptArgs) -> Result<()> {
    let provenance = parse_external_source(&args.source)?;
    let SkillProvenance::External { root } = provenance else {
        unreachable!("parse_external_source returns External");
    };
    let path = format!(
        "/api/skills/{}/{}/adopt",
        path_segment(&root)?,
        path_segment(&args.directory)?
    );
    let body = serde_json::json!({ "destination": args.destination });
    let reply = super::daemon_client()
        .await?
        .mutate(Method::POST, &path, Some(body))
        .await?;
    let destination = reply["directory"].as_str().unwrap_or(&args.directory);
    println!(
        "Adopted {} as managed skill {}.",
        args.directory, destination
    );
    Ok(())
}

async fn remove(args: SkillRemoveArgs) -> Result<()> {
    let path = format!("/api/skills/{}", path_segment(&args.directory)?);
    super::daemon_client()
        .await?
        .mutate(Method::DELETE, &path, None)
        .await?;
    println!("Removed managed skill {}.", args.directory);
    Ok(())
}

async fn sync(args: SkillSyncArgs) -> Result<()> {
    let body = serde_json::json!({
        "roots": args.roots,
        "replace": args.replace,
        "directories": args.only,
    });
    let reply = super::daemon_client()
        .await?
        .mutate(Method::POST, "/api/skills/sync", Some(body))
        .await?;
    let outcomes: Vec<skills_model::SyncOutcome> =
        serde_json::from_value(reply["outcomes"].clone())?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    } else if outcomes.is_empty() {
        println!("No AoE-managed skills to share.");
    } else {
        println!("{:<24} {:<20} {:<10} DETAIL", "DIRECTORY", "ROOT", "STATUS");
        for outcome in &outcomes {
            println!(
                "{:<24} {:<20} {:<10} {}",
                outcome.directory,
                outcome.root,
                format!("{:?}", outcome.status).to_lowercase(),
                outcome.message.as_deref().unwrap_or("")
            );
        }
    }

    let failed = outcomes
        .iter()
        .filter(|o| o.status == skills_model::SyncStatus::Error)
        .count();
    if failed > 0 {
        bail!("{failed} skill(s) could not be shared");
    }
    Ok(())
}

fn skills_dirs() -> Result<(PathBuf, PathBuf)> {
    let home = dirs::home_dir().context("could not resolve home dir for skills")?;
    Ok((home, crate::session::get_app_dir()?))
}

fn parse_source(source: &str) -> Result<SkillProvenance> {
    if source == "aoe-managed" {
        Ok(SkillProvenance::AoeManaged)
    } else {
        parse_external_source(source)
    }
}

fn parse_external_source(source: &str) -> Result<SkillProvenance> {
    if skills_model::skill_root(source).is_none() {
        let roots = skills_model::skill_roots()
            .iter()
            .map(|root| root.id)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("unknown skill source {source:?}; expected one of: {roots}");
    }
    Ok(SkillProvenance::External {
        root: source.to_string(),
    })
}

fn skill_error(error: SkillError) -> anyhow::Error {
    match error {
        SkillError::InvalidInput(message)
        | SkillError::NotFound(message)
        | SkillError::Collision(message)
        | SkillError::ReadOnly(message) => anyhow::anyhow!("{message}"),
        SkillError::Io(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_parser_accepts_managed_and_known_roots() {
        assert_eq!(
            parse_source("aoe-managed").unwrap(),
            SkillProvenance::AoeManaged
        );
        assert_eq!(
            parse_source("agents-standard").unwrap(),
            SkillProvenance::External {
                root: "agents-standard".to_string()
            }
        );
        assert!(parse_source("unknown").is_err());
    }
}

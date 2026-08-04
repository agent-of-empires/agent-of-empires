//! `aoe skill` CLI for discovering and managing agent skills.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};

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

pub fn run(command: SkillCommands) -> Result<()> {
    match command {
        SkillCommands::List(args) => list(args),
        SkillCommands::View(args) => view(args),
        SkillCommands::Add(args) => add(args),
        SkillCommands::Edit(args) => edit(args),
        SkillCommands::Adopt(args) => adopt(args),
        SkillCommands::Remove(args) => remove(args),
        SkillCommands::Sync(args) => sync(args),
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

fn add(args: SkillAddArgs) -> Result<()> {
    let (_, app_dir) = skills_dirs()?;
    skills_model::create_skill(&app_dir, &args.directory, args.description.as_deref())
        .map_err(skill_error)?;
    println!("Created managed skill {}.", args.directory);
    Ok(())
}

fn edit(args: SkillEditArgs) -> Result<()> {
    let (home, app_dir) = skills_dirs()?;
    // Every input path is bounded before it becomes a String. edit_skill
    // enforces the same limit, but only after the whole input is already in
    // memory, so an oversized file or an endless stdin stream would be read in
    // full just to be rejected.
    let content = match args.file {
        Some(path) if path.as_os_str() == "-" => read_stdin_capped()?,
        Some(path) => skills_model::read_file_capped(&path, skills_model::MAX_SKILL_MD_BYTES)
            .with_context(|| format!("failed to read {}", path.display()))?,
        None => edit_with_editor(&home, &app_dir, &args.directory)?,
    };
    skills_model::edit_skill(&home, &app_dir, &args.directory, &content).map_err(skill_error)?;
    println!("Updated managed skill {}.", args.directory);
    Ok(())
}

fn edit_with_editor(
    home: &std::path::Path,
    app_dir: &std::path::Path,
    directory: &str,
) -> Result<String> {
    let skill = skills_model::read_skill(home, app_dir, &SkillProvenance::AoeManaged, directory)
        .map_err(skill_error)?;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("SKILL.md");
    std::fs::write(&path, skill.content)?;
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

/// Read stdin, refusing more than the `SKILL.md` limit. Reads through one
/// handle and rejects an overflow byte, so a stream that never ends cannot
/// exhaust memory before validation runs.
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

fn adopt(args: SkillAdoptArgs) -> Result<()> {
    let (home, app_dir) = skills_dirs()?;
    let provenance = parse_external_source(&args.source)?;
    let destination = skills_model::adopt_skill(
        &home,
        &app_dir,
        &provenance,
        &args.directory,
        args.destination.as_deref(),
    )
    .map_err(skill_error)?;
    println!(
        "Adopted {} as managed skill {}.",
        args.directory, destination
    );
    Ok(())
}

fn remove(args: SkillRemoveArgs) -> Result<()> {
    let (home, app_dir) = skills_dirs()?;
    skills_model::delete_skill(&home, &app_dir, &args.directory).map_err(skill_error)?;
    println!("Removed managed skill {}.", args.directory);
    Ok(())
}

fn sync(args: SkillSyncArgs) -> Result<()> {
    let (home, app_dir) = skills_dirs()?;
    let options = skills_model::SyncOptions {
        replace: args.replace.into_iter().collect(),
        only: args.only.into_iter().collect(),
    };
    let outcomes = if args.roots.is_empty() {
        skills_model::sync_all_roots(&home, &app_dir, &options)
    } else {
        let mut out = Vec::new();
        for root in &args.roots {
            out.extend(
                skills_model::sync_root(&home, &app_dir, root, &options).map_err(skill_error)?,
            );
        }
        out
    };

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

    // A conflict is a normal, reportable result: the user's own file is intact.
    // Only a genuine failure to write is worth a non-zero exit.
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

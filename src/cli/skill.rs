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

pub fn run(command: SkillCommands) -> Result<()> {
    match command {
        SkillCommands::List(args) => list(args),
        SkillCommands::View(args) => view(args),
        SkillCommands::Add(args) => add(args),
        SkillCommands::Edit(args) => edit(args),
        SkillCommands::Adopt(args) => adopt(args),
        SkillCommands::Remove(args) => remove(args),
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
    let content = match args.file {
        Some(path) if path.as_os_str() == "-" => {
            let mut content = String::new();
            std::io::stdin().read_to_string(&mut content)?;
            content
        }
        Some(path) => std::fs::read_to_string(&path)
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
    std::fs::read_to_string(path).context("failed to read edited SKILL.md")
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

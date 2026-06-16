//! `aoe project` subcommands: manage the project registry used by the
//! multi-repo workspace pickers.

use anyhow::{bail, Result};
use clap::{Args, Subcommand, ValueEnum, ValueHint};
use serde::Serialize;
use std::path::PathBuf;

use crate::session::projects;
use crate::session::{Project, ProjectScope};

#[derive(Subcommand)]
pub enum ProjectCommands {
    /// List registered projects
    #[command(alias = "ls")]
    List(ProjectListArgs),

    /// Add a project to the registry
    Add(ProjectAddArgs),

    /// Remove a project from the registry
    #[command(alias = "rm")]
    Remove(ProjectRemoveArgs),
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum ScopeFilter {
    All,
    Global,
    Profile,
}

#[derive(Args)]
pub struct ProjectListArgs {
    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Filter by scope (default: all)
    #[arg(long, value_enum, default_value_t = ScopeFilter::All)]
    scope: ScopeFilter,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum ScopeArg {
    Global,
    Profile,
}

#[derive(Args)]
pub struct ProjectAddArgs {
    /// Path to the project directory: a git repository, or any directory to run sessions in place
    #[arg(value_hint = ValueHint::DirPath)]
    path: PathBuf,

    /// Display name (defaults to the directory's basename)
    #[arg(long)]
    name: Option<String>,

    /// Registry scope. When omitted: defaults to GLOBAL, unless `-p <profile>`
    /// was passed at the top level, in which case it defaults to PROFILE
    /// (scoping the entry to that profile only).
    #[arg(long, value_enum)]
    scope: Option<ScopeArg>,

    /// Allow registering this path even if it already exists in the other
    /// scope. Without this flag the command errors when the same canonical
    /// path is already registered globally (when adding to profile) or in any
    /// profile (when adding globally). When override is allowed and both
    /// scopes hold the same path, the profile entry shadows the global one.
    #[arg(long)]
    allow_override: bool,

    /// Default base branch for new worktree branches created against this
    /// project, whether it is the launch repo or an extra repo in a multi-repo
    /// workspace. An explicit session base wins; when omitted, falls back to
    /// the global/profile `worktree.default_base_branch`, then the repo's
    /// detected default branch.
    #[arg(long = "base-branch")]
    base_branch: Option<String>,
}

#[derive(Args)]
pub struct ProjectRemoveArgs {
    /// Project name or path to remove
    #[arg(value_hint = ValueHint::AnyPath)]
    name_or_path: String,

    /// Registry scope to remove from. When omitted: defaults to GLOBAL,
    /// unless `-p <profile>` was passed at the top level, in which case it
    /// defaults to PROFILE.
    #[arg(long, value_enum)]
    scope: Option<ScopeArg>,
}

#[derive(Serialize)]
struct ProjectInfo {
    name: String,
    path: String,
    scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_base_branch: Option<String>,
}

#[tracing::instrument(target = "cli.project", skip_all, fields(profile = %profile, profile_explicit))]
pub async fn run(profile: &str, profile_explicit: bool, command: ProjectCommands) -> Result<()> {
    match command {
        ProjectCommands::List(args) => list(profile, args).await,
        ProjectCommands::Add(args) => add(profile, profile_explicit, args).await,
        ProjectCommands::Remove(args) => remove(profile, profile_explicit, args).await,
    }
}

fn resolve_default_scope(profile_explicit: bool) -> ProjectScope {
    if profile_explicit {
        ProjectScope::Profile
    } else {
        ProjectScope::Global
    }
}

async fn list(profile: &str, args: ProjectListArgs) -> Result<()> {
    let entries: Vec<Project> = match args.scope {
        ScopeFilter::All => projects::load_merged(profile)?,
        ScopeFilter::Global => projects::load_global()?,
        ScopeFilter::Profile => projects::load_profile(profile)?,
    };

    if args.json {
        let info: Vec<ProjectInfo> = entries
            .iter()
            .map(|p| ProjectInfo {
                name: p.name.clone(),
                path: p.path.clone(),
                scope: p.scope.as_str().to_string(),
                default_base_branch: p.default_base_branch.clone(),
            })
            .collect();
        super::output::print_json(&info)?;
        return Ok(());
    }

    if entries.is_empty() {
        println!("No projects registered.");
        println!("Add one with: aoe project add <path>");
        return Ok(());
    }

    println!("Projects:\n");
    for p in &entries {
        println!("  • {} [{}]  {}", p.name, p.scope.as_str(), p.path);
        if let Some(base) = &p.default_base_branch {
            println!("      base branch: {base}");
        }
    }
    let n = entries.len();
    println!(
        "\nTotal: {} {}",
        n,
        if n == 1 { "project" } else { "projects" }
    );
    Ok(())
}

async fn add(profile: &str, profile_explicit: bool, args: ProjectAddArgs) -> Result<()> {
    let scope = match args.scope {
        Some(ScopeArg::Global) => ProjectScope::Global,
        Some(ScopeArg::Profile) => ProjectScope::Profile,
        None => resolve_default_scope(profile_explicit),
    };

    let canonical = args
        .path
        .canonicalize()
        .unwrap_or_else(|_| args.path.clone());

    // Non-git directories are allowed: their sessions run in place, with no
    // worktrees or branches. We still reject paths that don't resolve to a
    // directory, which the previous git-repo gate rejected implicitly.
    if !canonical.is_dir() {
        bail!(
            "Path does not exist or is not a directory: {}",
            canonical.display()
        );
    }

    let name = args.name.unwrap_or_else(|| {
        canonical
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string())
    });

    let project = Project::new(name.clone(), canonical.to_string_lossy(), scope)
        .with_base_branch(args.base_branch);
    let is_git = project.is_git();
    let saved = projects::add(profile, scope, project, args.allow_override)?;
    println!(
        "✓ Registered project '{}' [{}] at {}",
        saved.name,
        scope.as_str(),
        saved.path
    );
    if let Some(base) = &saved.default_base_branch {
        println!("  Default base branch: {base}");
    }
    if !is_git {
        println!(
            "  Note: not a git repository; sessions open directly in this folder \
             (no worktree per session, branches, or diff view)."
        );
    }
    Ok(())
}

async fn remove(profile: &str, profile_explicit: bool, args: ProjectRemoveArgs) -> Result<()> {
    let scope = match args.scope {
        Some(ScopeArg::Global) => ProjectScope::Global,
        Some(ScopeArg::Profile) => ProjectScope::Profile,
        None => resolve_default_scope(profile_explicit),
    };
    let removed = projects::remove(profile, scope, &args.name_or_path)?;
    println!(
        "✓ Removed project '{}' [{}] (was at {})",
        removed.name,
        scope.as_str(),
        removed.path
    );
    Ok(())
}

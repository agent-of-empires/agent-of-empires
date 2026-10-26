//! `aoe project` subcommands: manage the project registry used by the

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum, ValueHint};
use serde::Serialize;
use std::path::PathBuf;

use crate::session::projects;
use crate::session::{resolve_existing_profile, Project, ProjectScope};

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

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ScopeFilter {
    All,
    Global,
    Profile,
}

#[derive(Args)]
pub struct ProjectListArgs {
    /// Output as JSON
    #[arg(long)]
    pub(crate) json: bool,

    /// Filter by scope (default: all)
    #[arg(long, value_enum, default_value = "all")]
    pub(crate) scope: ScopeFilter,
}

impl ProjectListArgs {
    pub(crate) fn json(&self) -> bool {
        self.json
    }

    pub(crate) fn scope(&self) -> ScopeFilter {
        self.scope
    }
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

    /// Allow the same canonical path in the global and selected profile scopes.
    /// The profile entry shadows the global entry in merged views.
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
    let resolved_profile = match args.scope {
        ScopeFilter::Global => None,
        ScopeFilter::All | ScopeFilter::Profile => Some(resolve_existing_profile(profile)?),
    };
    let profile = resolved_profile.as_deref().unwrap_or(profile);
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

fn scope_or_default(arg: Option<ScopeArg>, profile_explicit: bool) -> ProjectScope {
    match arg {
        Some(ScopeArg::Global) => ProjectScope::Global,
        Some(ScopeArg::Profile) => ProjectScope::Profile,
        None => resolve_default_scope(profile_explicit),
    }
}

async fn add(profile: &str, profile_explicit: bool, args: ProjectAddArgs) -> Result<()> {
    let scope = scope_or_default(args.scope, profile_explicit);
    let profile = resolve_existing_profile(profile)?;
    // The Daemon resolves the path in its own working directory, so a relative
    // one is anchored here, where the user typed it.
    let absolute = std::path::absolute(&args.path)?;
    let canonical = absolute.canonicalize().unwrap_or(absolute);
    let name = args.name.unwrap_or_else(|| {
        let base = canonical
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());
        projects::unique_name(&profile, scope, &base)
    });
    anyhow::ensure!(
        canonical.is_dir(),
        "Path does not exist or is not a directory: {}",
        canonical.display()
    );
    let path = canonical.to_string_lossy().to_string();
    super::mutate_project(crate::daemon::ProjectMutation::Create(
        crate::daemon::CreateProjectBody {
            profile,
            scope,
            path: path.clone(),
            name: Some(name.clone()),
            allow_override: args.allow_override,
            default_base_branch: args.base_branch.clone(),
            pinned: false,
            overrides: Default::default(),
        },
    ))
    .await?;
    println!(
        "✓ Registered project '{}' [{}] at {}",
        name,
        scope.as_str(),
        path
    );
    if let Some(base) = args.base_branch.as_deref().filter(|b| !b.trim().is_empty()) {
        println!("  Default base branch: {base}");
    }
    if !Project::new(name, &path, scope).is_git() {
        println!(
            "  Note: not a git repository; sessions open directly in this folder \
             (no worktree per session, branches, or diff view)."
        );
    }
    Ok(())
}

async fn remove(profile: &str, profile_explicit: bool, args: ProjectRemoveArgs) -> Result<()> {
    let scope = scope_or_default(args.scope, profile_explicit);
    let target = match scope {
        ProjectScope::Global => crate::daemon::ProjectTarget::Global,
        ProjectScope::Profile => crate::daemon::ProjectTarget::Profile {
            profile: resolve_existing_profile(profile)?,
        },
    };
    // The Daemon resolves a path in its own cwd, so a path-like argument is
    // anchored here; a bare name still looks up by name.
    let name_or_path = if args.name_or_path.contains(std::path::is_separator)
        || matches!(args.name_or_path.as_str(), "." | "..")
    {
        std::path::absolute(&args.name_or_path)?
            .to_string_lossy()
            .into_owned()
    } else {
        args.name_or_path.clone()
    };
    super::mutate_project(crate::daemon::ProjectMutation::Remove {
        target,
        name_or_path,
    })
    .await?;
    println!(
        "✓ Removed project '{}' [{}]",
        args.name_or_path,
        scope.as_str()
    );
    Ok(())
}

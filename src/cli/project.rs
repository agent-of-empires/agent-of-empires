//! `aoe project` subcommands: manage the project registry used by the

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum, ValueHint};
use reqwest::Method;
use std::path::PathBuf;

use crate::daemon::transport::path_segment;
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

#[tracing::instrument(target = "cli.project", skip_all, fields(profile = %profile, profile_explicit))]
pub async fn run(profile: &str, profile_explicit: bool, command: ProjectCommands) -> Result<()> {
    match command {
        ProjectCommands::List(_) => bail!("internal error: runtime read reached local dispatch"),
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

fn scope_or_default(arg: Option<ScopeArg>, profile_explicit: bool) -> ProjectScope {
    match arg {
        Some(ScopeArg::Global) => ProjectScope::Global,
        Some(ScopeArg::Profile) => ProjectScope::Profile,
        None => resolve_default_scope(profile_explicit),
    }
}

/// The profile a write targets: the one named with `-p`, else the Daemon's
/// default, since the Daemon's store is the authority.
async fn target_profile(profile: &str, profile_explicit: bool) -> Result<String> {
    if profile_explicit && !profile.is_empty() {
        return Ok(profile.to_string());
    }
    let profiles = super::daemon_client()
        .await?
        .mutate(Method::GET, "/api/profiles", None)
        .await?;
    profiles
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["is_default"] == true)
        .and_then(|p| p["name"].as_str())
        .map(str::to_string)
        .context("the Daemon reported no default profile")
}

async fn add(profile: &str, profile_explicit: bool, args: ProjectAddArgs) -> Result<()> {
    let scope = scope_or_default(args.scope, profile_explicit);
    // The Daemon resolves a path in its own working directory, so it is
    // anchored here, where the user typed it. Whether it exists is the
    // Daemon's call.
    let path = std::path::absolute(&args.path)?;
    let body = serde_json::json!({
        "profile": target_profile(profile, profile_explicit).await?,
        "scope": scope.as_str(),
        "path": path.to_string_lossy(),
        "name": args.name,
        "allow_override": args.allow_override,
        "default_base_branch": args.base_branch,
    });
    let value = super::runtime_write(Method::POST, "/api/projects", Some(body)).await?;
    let saved: crate::daemon::ProjectResponse = serde_json::from_value(value)?;
    println!(
        "✓ Registered project '{}' [{}] at {}",
        saved.name,
        scope.as_str(),
        saved.path
    );
    if let Some(base) = &saved.default_base_branch {
        println!("  Default base branch: {base}");
    }
    // Only checkable where the path exists on this machine.
    let local = std::path::Path::new(&saved.path);
    if local.is_dir() && !Project::new(saved.name, &saved.path, scope).is_git() {
        println!(
            "  Note: not a git repository; sessions open directly in this folder \
             (no worktree per session, branches, or diff view)."
        );
    }
    Ok(())
}

async fn remove(profile: &str, profile_explicit: bool, args: ProjectRemoveArgs) -> Result<()> {
    let scope = scope_or_default(args.scope, profile_explicit);
    // The raw argument stays a candidate name; the Daemon also gets it anchored
    // here, because it would otherwise resolve a path in its own cwd.
    let anchored = std::path::absolute(&args.name_or_path)?;
    let mut path = format!(
        "/api/projects/{}?scope={}&path={}",
        path_segment(&args.name_or_path)?,
        scope.as_str(),
        percent_encoding::utf8_percent_encode(
            &anchored.to_string_lossy(),
            percent_encoding::NON_ALPHANUMERIC
        )
    );
    if scope == ProjectScope::Profile {
        let profile = target_profile(profile, profile_explicit).await?;
        path.push_str(&format!(
            "&profile={}",
            percent_encoding::utf8_percent_encode(&profile, percent_encoding::NON_ALPHANUMERIC)
        ));
    }
    let value = super::runtime_write(Method::DELETE, &path, None).await?;
    let removed: crate::daemon::ProjectResponse = serde_json::from_value(value)?;
    println!(
        "✓ Removed project '{}' [{}] (was at {})",
        removed.name,
        scope.as_str(),
        removed.path
    );
    Ok(())
}

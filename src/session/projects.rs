//! Project registry: saved repo paths the user can pick from when creating
//! a multi-repo session. Two scopes:
//! - Global: `<app_dir>/projects.json`, visible from every profile.
//! - Profile: `<app_dir>/profiles/{profile}/projects.json`, visible only inside that profile.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::warn;

use super::{get_app_dir, get_profile_dir};

/// Distinct failure modes for registry mutations. The web layer maps these to
/// HTTP status codes (Conflict → 409, NotFound → 404, Other → 500); CLI/TUI
/// callers convert via `Into<anyhow::Error>` and surface the message verbatim.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// A project with the same name or canonical path already exists in the
    /// target scope, or in the other scope when `allow_override` is false.
    #[error("{0}")]
    Conflict(String),

    /// `remove` could not find a project matching the given name or path in
    /// the requested scope.
    #[error("{0}")]
    NotFound(String),

    /// Any other failure (I/O, JSON parse, missing app dir).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<std::io::Error> for RegistryError {
    fn from(e: std::io::Error) -> Self {
        RegistryError::Other(e.into())
    }
}

impl From<serde_json::Error> for RegistryError {
    fn from(e: serde_json::Error) -> Self {
        RegistryError::Other(e.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectScope {
    Global,
    Profile,
}

impl ProjectScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectScope::Global => "global",
            ProjectScope::Profile => "profile",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub path: String,
    /// Default base branch for new worktree branches created against this
    /// project's repo, whether it is the launch repo or an extra repo in a
    /// multi-repo workspace. An explicit session base wins; when `None`,
    /// resolution falls back to the global/profile `worktree.default_base_branch`,
    /// then the repo's detected default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    /// Populated by the loader; not persisted.
    #[serde(skip, default = "default_scope")]
    pub scope: ProjectScope,
}

fn default_scope() -> ProjectScope {
    ProjectScope::Global
}

impl Project {
    pub fn new(name: impl Into<String>, path: impl Into<String>, scope: ProjectScope) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            default_base_branch: None,
            scope,
        }
    }

    /// Set the project's default base branch, treating an empty/whitespace
    /// string as "unset".
    pub fn with_base_branch(mut self, base: Option<String>) -> Self {
        self.default_base_branch = base.map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
        self
    }
}

fn global_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("projects.json"))
}

fn profile_path(profile: &str) -> Result<PathBuf> {
    Ok(get_profile_dir(profile)?.join("projects.json"))
}

fn read_file(path: &Path, scope: ProjectScope) -> Result<Vec<Project>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut projects: Vec<Project> = serde_json::from_str(&content)?;
    for p in &mut projects {
        p.scope = scope;
    }
    Ok(projects)
}

fn write_file(path: &Path, projects: &[Project]) -> Result<()> {
    let content = serde_json::to_string_pretty(projects)?;
    super::atomic_write(path, content.as_bytes())?;
    Ok(())
}

/// Load global registry only.
pub fn load_global() -> Result<Vec<Project>> {
    read_file(&global_path()?, ProjectScope::Global)
}

/// Load profile-scoped registry only.
pub fn load_profile(profile: &str) -> Result<Vec<Project>> {
    read_file(&profile_path(profile)?, ProjectScope::Profile)
}

/// Load union of global + profile, deduped by canonical path. Profile entries
/// shadow global ones with the same path.
pub fn load_merged(profile: &str) -> Result<Vec<Project>> {
    let global = load_global().unwrap_or_else(|e| {
        warn!("Failed to load global projects: {}", e);
        Vec::new()
    });
    let profile = load_profile(profile).unwrap_or_else(|e| {
        warn!("Failed to load profile projects: {}", e);
        Vec::new()
    });

    let mut merged: Vec<Project> = Vec::new();
    let mut seen_paths: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for p in global.into_iter().chain(profile) {
        let canonical = canonical_key(&p.path);
        if let Some(&idx) = seen_paths.get(&canonical) {
            // Profile shadows global on path collision.
            if p.scope == ProjectScope::Profile {
                merged[idx] = p;
            }
        } else {
            seen_paths.insert(canonical, merged.len());
            merged.push(p);
        }
    }
    Ok(merged)
}

pub(crate) fn canonical_key(path: &str) -> String {
    PathBuf::from(path)
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// Replace the contents of one scope's registry file.
pub fn save_scope(profile: &str, scope: ProjectScope, projects: &[Project]) -> Result<()> {
    let path = match scope {
        ProjectScope::Global => global_path()?,
        ProjectScope::Profile => profile_path(profile)?,
    };
    write_file(&path, projects)
}

/// Append a project to the given scope.
///
/// Errors if:
/// - a project with the same name or canonical path already exists in the
///   target scope (always; overriding within a scope makes no sense), or
/// - the canonical path already exists in the *other* scope and
///   `allow_override` is false. Pass `allow_override = true` to deliberately
///   shadow a global entry from a profile (or vice versa).
pub fn add(
    profile: &str,
    scope: ProjectScope,
    mut project: Project,
    allow_override: bool,
) -> std::result::Result<Project, RegistryError> {
    project.scope = scope;
    let path_buf = PathBuf::from(&project.path);
    let canonical = path_buf.canonicalize().unwrap_or_else(|_| path_buf.clone());
    project.path = canonical.to_string_lossy().to_string();

    let mut existing = match scope {
        ProjectScope::Global => load_global().map_err(RegistryError::Other)?,
        ProjectScope::Profile => load_profile(profile).map_err(RegistryError::Other)?,
    };

    for p in &existing {
        if p.name.eq_ignore_ascii_case(&project.name) {
            return Err(RegistryError::Conflict(format!(
                "Project '{}' already registered in {} scope (as '{}')",
                project.name,
                scope.as_str(),
                p.name,
            )));
        }
        if canonical_key(&p.path) == canonical_key(&project.path) {
            return Err(RegistryError::Conflict(format!(
                "Path '{}' already registered as '{}' in {} scope",
                project.path,
                p.name,
                scope.as_str()
            )));
        }
    }

    if !allow_override {
        let other_scope = match scope {
            ProjectScope::Global => ProjectScope::Profile,
            ProjectScope::Profile => ProjectScope::Global,
        };
        let other = match other_scope {
            ProjectScope::Global => load_global().unwrap_or_default(),
            ProjectScope::Profile => load_profile(profile).unwrap_or_default(),
        };
        for p in &other {
            if canonical_key(&p.path) == canonical_key(&project.path) {
                return Err(RegistryError::Conflict(format!(
                    "Path '{}' is already registered as '{}' in {} scope.\n\
                     Tip: remove it first with `aoe project remove {} --scope {}`,\n\
                     or pass `--allow-override` to keep both entries (the profile entry shadows the global entry in merged views).",
                    project.path,
                    p.name,
                    other_scope.as_str(),
                    p.name,
                    other_scope.as_str(),
                )));
            }
        }
    }

    existing.push(project.clone());
    save_scope(profile, scope, &existing).map_err(RegistryError::Other)?;
    Ok(project)
}

/// Remove the entry matching `name_or_path` from the given scope. Returns the
/// removed project, or errors if no match was found.
pub fn remove(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
) -> std::result::Result<Project, RegistryError> {
    let mut existing = match scope {
        ProjectScope::Global => load_global().map_err(RegistryError::Other)?,
        ProjectScope::Profile => load_profile(profile).map_err(RegistryError::Other)?,
    };

    let canonical_target = canonical_key(name_or_path);
    let idx = existing
        .iter()
        .position(|p| {
            p.name.eq_ignore_ascii_case(name_or_path) || canonical_key(&p.path) == canonical_target
        })
        .ok_or_else(|| {
            RegistryError::NotFound(format!(
                "No project '{}' in {} scope",
                name_or_path,
                scope.as_str()
            ))
        })?;
    let removed = existing.remove(idx);
    save_scope(profile, scope, &existing).map_err(RegistryError::Other)?;
    Ok(removed)
}

/// Set or clear the default base branch on the entry matching `name_or_path`
/// in the given scope. `base` is normalized via `Project::with_base_branch`
/// (trimmed; empty becomes unset). Returns the updated project, or `NotFound`
/// if no entry matches.
///
/// This is a read-modify-write over the scope's registry file. There is no
/// optimistic-concurrency guard; for a single-user local tool last-writer-wins
/// across racing `aoe` processes is acceptable.
pub fn update_base_branch(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
    base: Option<String>,
) -> std::result::Result<Project, RegistryError> {
    let mut existing = match scope {
        ProjectScope::Global => load_global().map_err(RegistryError::Other)?,
        ProjectScope::Profile => load_profile(profile).map_err(RegistryError::Other)?,
    };

    let canonical_target = canonical_key(name_or_path);
    let idx = existing
        .iter()
        .position(|p| {
            p.name.eq_ignore_ascii_case(name_or_path) || canonical_key(&p.path) == canonical_target
        })
        .ok_or_else(|| {
            RegistryError::NotFound(format!(
                "No project '{}' in {} scope",
                name_or_path,
                scope.as_str()
            ))
        })?;

    existing[idx] = existing[idx].clone().with_base_branch(base);
    let updated = existing[idx].clone();
    save_scope(profile, scope, &existing).map_err(RegistryError::Other)?;
    Ok(updated)
}

/// Resolve a list of project names against the merged registry. Errors on the
/// first unknown name with the available names listed.
pub fn resolve_names(profile: &str, names: &[String]) -> Result<Vec<Project>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let merged = load_merged(profile)?;
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        let project = merged
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                let available: Vec<String> = merged.iter().map(|p| p.name.clone()).collect();
                anyhow::anyhow!(
                    "Unknown project '{}'. Available: {}",
                    name,
                    if available.is_empty() {
                        "<none registered>".to_string()
                    } else {
                        available.join(", ")
                    }
                )
            })?;
        resolved.push(project.clone());
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn setup(temp: &Path) {
        std::env::set_var("HOME", temp);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"));
    }

    #[test]
    fn with_base_branch_trims_and_treats_empty_as_unset() {
        let p = Project::new("r", "/tmp/r", ProjectScope::Global);
        assert_eq!(p.clone().with_base_branch(None).default_base_branch, None);
        assert_eq!(
            p.clone()
                .with_base_branch(Some("  ".to_string()))
                .default_base_branch,
            None
        );
        assert_eq!(
            p.with_base_branch(Some("  develop ".to_string()))
                .default_base_branch,
            Some("develop".to_string())
        );
    }

    #[test]
    #[serial]
    fn default_base_branch_persists_through_add_and_load() -> Result<()> {
        let temp = tempdir()?;
        setup(temp.path());
        let repo = temp.path().join("repoBase");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("repoBase", repo.to_string_lossy(), ProjectScope::Global)
                .with_base_branch(Some("develop".to_string())),
            false,
        )?;

        let loaded = load_global()?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].default_base_branch.as_deref(), Some("develop"));
        Ok(())
    }

    #[test]
    #[serial]
    fn update_base_branch_sets_clears_and_reports_not_found() -> Result<()> {
        let temp = tempdir()?;
        setup(temp.path());
        let repo = temp.path().join("repoUpd");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("repoUpd", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )?;

        // Set, looking the project up by name.
        let updated = update_base_branch(
            "default",
            ProjectScope::Global,
            "repoUpd",
            Some("develop".into()),
        )?;
        assert_eq!(updated.default_base_branch.as_deref(), Some("develop"));
        assert_eq!(
            load_global()?[0].default_base_branch.as_deref(),
            Some("develop")
        );

        // Whitespace clears it back to unset, looking up by canonical path.
        let cleared = update_base_branch(
            "default",
            ProjectScope::Global,
            &repo.to_string_lossy(),
            Some("   ".into()),
        )?;
        assert_eq!(cleared.default_base_branch, None);
        assert_eq!(load_global()?[0].default_base_branch, None);

        // Unknown project is a NotFound.
        assert!(matches!(
            update_base_branch("default", ProjectScope::Global, "nope", Some("x".into())),
            Err(RegistryError::NotFound(_))
        ));
        Ok(())
    }

    #[test]
    #[serial]
    fn add_then_load_global() -> Result<()> {
        let temp = tempdir()?;
        setup(temp.path());
        let repo = temp.path().join("repoA");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("repoA", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )?;

        let loaded = load_global()?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "repoA");
        assert_eq!(loaded[0].scope, ProjectScope::Global);
        Ok(())
    }

    #[test]
    #[serial]
    fn profile_shadows_global_on_path_collision() -> Result<()> {
        let temp = tempdir()?;
        setup(temp.path());
        let repo = temp.path().join("repoX");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("global-name", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )?;
        add(
            "default",
            ProjectScope::Profile,
            Project::new(
                "profile-name",
                repo.to_string_lossy(),
                ProjectScope::Profile,
            ),
            true,
        )?;

        let merged = load_merged("default")?;
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "profile-name");
        assert_eq!(merged[0].scope, ProjectScope::Profile);
        Ok(())
    }

    #[test]
    #[serial]
    fn duplicate_name_rejected_within_scope() -> Result<()> {
        let temp = tempdir()?;
        setup(temp.path());
        let repo1 = temp.path().join("r1");
        let repo2 = temp.path().join("r2");
        let _ = git2::Repository::init(&repo1);
        let _ = git2::Repository::init(&repo2);

        add(
            "default",
            ProjectScope::Global,
            Project::new("dup", repo1.to_string_lossy(), ProjectScope::Global),
            false,
        )?;
        let err = add(
            "default",
            ProjectScope::Global,
            Project::new("dup", repo2.to_string_lossy(), ProjectScope::Global),
            false,
        );
        assert!(err.is_err());
        Ok(())
    }

    #[test]
    #[serial]
    fn name_matching_is_case_insensitive() -> Result<()> {
        let temp = tempdir()?;
        setup(temp.path());
        let repo1 = temp.path().join("Mixed");
        let repo2 = temp.path().join("Other");
        let _ = git2::Repository::init(&repo1);
        let _ = git2::Repository::init(&repo2);

        add(
            "default",
            ProjectScope::Global,
            Project::new("MixedCase", repo1.to_string_lossy(), ProjectScope::Global),
            false,
        )?;

        // Add with same name, different case → rejected.
        let err = add(
            "default",
            ProjectScope::Global,
            Project::new("mixedcase", repo2.to_string_lossy(), ProjectScope::Global),
            false,
        );
        assert!(err.is_err(), "duplicate name (different case) should error");

        // Resolve via lowercase finds the original.
        let resolved = resolve_names("default", &["MIXEDCASE".to_string()])?;
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "MixedCase");

        // Remove via lowercase succeeds.
        let removed = remove("default", ProjectScope::Global, "mixedcase")?;
        assert_eq!(removed.name, "MixedCase");
        Ok(())
    }

    #[test]
    #[serial]
    fn cross_scope_path_collision_blocked_by_default() -> Result<()> {
        let temp = tempdir()?;
        setup(temp.path());
        let repo = temp.path().join("repoZ");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("first", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )?;
        let err = add(
            "default",
            ProjectScope::Profile,
            Project::new("second", repo.to_string_lossy(), ProjectScope::Profile),
            false,
        );
        assert!(
            err.is_err(),
            "cross-scope dup should error without override"
        );
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("--allow-override") && msg.contains("global"),
            "error should mention --allow-override and the other scope, got: {msg}"
        );

        // With override, succeeds.
        add(
            "default",
            ProjectScope::Profile,
            Project::new("second", repo.to_string_lossy(), ProjectScope::Profile),
            true,
        )?;
        Ok(())
    }

    #[test]
    #[serial]
    fn resolve_names_errors_on_unknown() -> Result<()> {
        let temp = tempdir()?;
        setup(temp.path());
        let err = resolve_names("default", &["nonesuch".to_string()]);
        assert!(err.is_err());
        Ok(())
    }

    #[test]
    #[serial]
    fn remove_round_trip() -> Result<()> {
        let temp = tempdir()?;
        setup(temp.path());
        let repo = temp.path().join("repoR");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("repoR", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )?;
        let removed = remove("default", ProjectScope::Global, "repoR")?;
        assert_eq!(removed.name, "repoR");
        let loaded = load_global()?;
        assert!(loaded.is_empty());
        Ok(())
    }
}

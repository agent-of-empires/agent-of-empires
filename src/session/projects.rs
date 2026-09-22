//! Project registry: saved repo paths the user can pick from when creating a multi-repo session.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::warn;

use super::{get_app_dir, get_profile_dir_path};

/// Distinct failure modes for registry mutations.
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

/// Per-project overrides for otherwise-global settings. Every field is
/// `None` when the project doesn't override that setting, so resolution
/// falls through to the global/profile default. Add a field here to make
/// a new global toggle project-overridable; existing call sites that
/// resolve overrides (`find_by_canonical_path`, `resolve_smart_rename_config`)
/// don't need to change shape, only the new call site that consults the
/// new field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectOverrides {
    /// Overrides `worktree.enabled` (create-worktree-by-default) for new
    /// sessions launched against this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_enabled: Option<bool>,
    /// Overrides `session.smart_rename` (agent-driven auto-naming) for
    /// sessions launched against this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_rename: Option<bool>,
}

impl ProjectOverrides {
    pub fn is_empty(&self) -> bool {
        self.worktree_enabled.is_none() && self.smart_rename.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub path: String,
    /// Default base branch for new worktree branches created against this project's repo, whether
    /// it is the launch repo or an extra repo in a multi-repo workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    /// Whether this project shows as an empty (sessionless) header in the sidebar / project view.
    #[serde(default = "default_pinned")]
    pub pinned: bool,
    /// Per-project overrides for otherwise-global settings (worktree
    /// default, smart rename, ...). See [`ProjectOverrides`].
    #[serde(default, skip_serializing_if = "ProjectOverrides::is_empty")]
    pub overrides: ProjectOverrides,
    /// Populated by the loader; not persisted.
    #[serde(skip, default = "default_scope")]
    pub scope: ProjectScope,
}

fn default_scope() -> ProjectScope {
    ProjectScope::Global
}

fn default_pinned() -> bool {
    true
}

impl Project {
    pub fn new(name: impl Into<String>, path: impl Into<String>, scope: ProjectScope) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            default_base_branch: None,
            pinned: false,
            overrides: ProjectOverrides::default(),
            scope,
        }
    }

    /// Set the project's default base branch, treating an empty/whitespace
    /// string as "unset".
    pub fn with_base_branch(mut self, base: Option<String>) -> Self {
        self.default_base_branch = base.map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
        self
    }

    /// Set the pin flag (whether the project shows as a sessionless header).
    pub fn with_pinned(mut self, pinned: bool) -> Self {
        self.pinned = pinned;
        self
    }

    pub fn with_overrides(mut self, overrides: ProjectOverrides) -> Self {
        self.overrides = overrides;
        self
    }

    /// Whether this project's path is currently a git repository (a working tree, a bare repo, or a
    /// linked worktree).
    pub fn is_git(&self) -> bool {
        let path = PathBuf::from(&self.path);
        let canonical = path.canonicalize().unwrap_or(path);
        crate::git::GitWorktree::is_git_repo(&canonical)
    }
}

fn global_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("projects.json"))
}

fn profile_path(profile: &str) -> Result<PathBuf> {
    Ok(get_profile_dir_path(profile)?.join("projects.json"))
}

fn registry_path(profile: &str, scope: ProjectScope) -> Result<PathBuf> {
    match scope {
        ProjectScope::Global => global_path(),
        ProjectScope::Profile => profile_path(profile),
    }
}

/// Parse a registry file's content, stamping the (non-persisted) scope on
/// every entry so callers see where each project came from.
fn parse_projects(content: &str, scope: ProjectScope) -> Result<Vec<Project>> {
    let mut projects: Vec<Project> = serde_json::from_str(content)?;
    for p in &mut projects {
        p.scope = scope;
    }
    Ok(projects)
}

fn read_file(path: &Path, scope: ProjectScope) -> Result<Vec<Project>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    parse_projects(&content, scope)
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

/// Display label for a repo path: its final path segment, with readable fallbacks for the root and
/// empty cases.
pub fn repo_label(path: &str) -> String {
    let p = Path::new(path);
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            if path == "/" || path.is_empty() {
                "(root)".to_string()
            } else {
                path.to_string()
            }
        })
}

/// A registered project that has no live session keeping its header alive in
/// the project-grouped view, so a surface can render it as an empty header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpopulatedProject {
    /// Header label (the repo basename), matching [`repo_label`].
    pub label: String,
    /// Canonical repo path, used to unpin the entry and to launch new
    /// sessions under it.
    pub path: String,
}

/// Given the set of project-header labels that already have at least one live session and the
/// registered projects, return the registered projects whose header would otherwise be invisible.
pub fn unpopulated_projects(
    populated_labels: &HashSet<String>,
    registered: &[Project],
) -> Vec<UnpopulatedProject> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in registered {
        // Only pinned projects surface as empty headers.
        if !p.pinned {
            continue;
        }
        let label = repo_label(&p.path);
        if populated_labels.contains(&label) || !seen.insert(canonical_key(&p.path)) {
            continue;
        }
        out.push(UnpopulatedProject {
            label,
            path: p.path.clone(),
        });
    }
    out
}

/// Read-modify-write one scope's registry under the file's sidecar lock, so two concurrent mutators
/// (e.g. parallel `aoe project add`) cannot each do load -> check -> save and silently drop the
/// other's registration.
fn locked_update_scope<R>(
    profile: &str,
    scope: ProjectScope,
    mutate: impl FnOnce(&mut Vec<Project>) -> std::result::Result<R, RegistryError>,
) -> std::result::Result<R, RegistryError> {
    let path = registry_path(profile, scope)?;
    super::storage::locked_update(
        &path,
        |content| parse_projects(content, scope),
        |projects| Ok(serde_json::to_string_pretty(projects)?),
        mutate,
    )
    .map_err(RegistryError::Other)?
}

/// `base_name`, or the first free `"{base_name}-N"` (N >= 2) in `scope`. For auto-derived names
/// only: an explicit name that collides must stay a conflict.
pub fn unique_name(profile: &str, scope: ProjectScope, base_name: &str) -> String {
    let existing = match scope {
        ProjectScope::Global => load_global().unwrap_or_default(),
        ProjectScope::Profile => load_profile(profile).unwrap_or_default(),
    };
    if !existing
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(base_name))
    {
        return base_name.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base_name}-{n}");
        if !existing
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&candidate))
        {
            return candidate;
        }
        n += 1;
    }
}

/// Append a project to the given scope.
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

    locked_update_scope(profile, scope, |existing| {
        for p in existing.iter() {
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
        Ok(project)
    })
}

/// Remove the entry matching `name_or_path` from the given scope. Returns the
/// removed project, or errors if no match was found.
pub fn remove(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
) -> std::result::Result<Project, RegistryError> {
    let canonical_target = canonical_key(name_or_path);
    locked_update_scope(profile, scope, |existing| {
        let idx = existing
            .iter()
            .position(|p| {
                p.name.eq_ignore_ascii_case(name_or_path)
                    || canonical_key(&p.path) == canonical_target
            })
            .ok_or_else(|| {
                RegistryError::NotFound(format!(
                    "No project '{}' in {} scope",
                    name_or_path,
                    scope.as_str()
                ))
            })?;
        Ok(existing.remove(idx))
    })
}

/// Edit the entry matching `name_or_path` in the given scope under the registry lock.
fn update_entry(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
    mutate: impl FnOnce(&mut Project),
) -> std::result::Result<Project, RegistryError> {
    let canonical_target = canonical_key(name_or_path);
    locked_update_scope(profile, scope, |existing| {
        let entry = existing
            .iter_mut()
            .find(|p| {
                p.name.eq_ignore_ascii_case(name_or_path)
                    || canonical_key(&p.path) == canonical_target
            })
            .ok_or_else(|| {
                RegistryError::NotFound(format!(
                    "No project '{}' in {} scope",
                    name_or_path,
                    scope.as_str()
                ))
            })?;
        mutate(entry);
        Ok(entry.clone())
    })
}

/// Set or clear the default base branch on the entry matching `name_or_path` in the given scope.
pub fn update_base_branch(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
    base: Option<String>,
) -> std::result::Result<Project, RegistryError> {
    update_entry(profile, scope, name_or_path, |p| {
        *p = p.clone().with_base_branch(base)
    })
}

/// Set the pin flag on the entry matching `name_or_path` in the given scope.
pub fn set_pinned(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
    pinned: bool,
) -> std::result::Result<Project, RegistryError> {
    update_entry(profile, scope, name_or_path, |p| p.pinned = pinned)
}

/// Look up the merged-registry entry (profile shadows global) whose path
/// canonicalizes to `path`, if any. Used to resolve per-project overrides
/// at call sites that only have a filesystem path in hand (no pre-built
/// canonical-path map).
pub fn find_by_canonical_path(profile: &str, path: &Path) -> Option<Project> {
    let target = canonical_key(&path.to_string_lossy());
    load_merged(profile)
        .ok()?
        .into_iter()
        .find(|p| canonical_key(&p.path) == target)
}

/// Edit the override bundle on the entry matching `name_or_path` in the given scope, under the
/// registry lock.
pub fn update_overrides(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
    mutate: impl FnOnce(&mut ProjectOverrides),
) -> std::result::Result<Project, RegistryError> {
    update_entry(profile, scope, name_or_path, |p| mutate(&mut p.overrides))
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
    use crate::session::test_support::isolate_app_dir_at;
    use serial_test::serial;
    use tempfile::tempdir;

    #[test]
    fn repo_label_uses_basename_with_root_fallbacks() {
        assert_eq!(repo_label("/home/me/myrepo"), "myrepo");
        assert_eq!(repo_label("/home/me/myrepo/"), "myrepo");
        assert_eq!(repo_label("/"), "(root)");
        assert_eq!(repo_label(""), "(root)");
    }

    #[test]
    fn unpopulated_projects_keeps_pinned_empty_projects_by_path() {
        let pinned =
            |name: &str, path: &str, scope| Project::new(name, path, scope).with_pinned(true);
        let registered = vec![
            pinned("alpha", "/work/alpha", ProjectScope::Global),
            pinned("beta", "/work/beta", ProjectScope::Global),
            pinned("beta", "/work/beta", ProjectScope::Profile),
            pinned("beta-other", "/other/beta", ProjectScope::Profile),
            Project::new("saved", "/work/saved", ProjectScope::Global),
        ];
        let populated: HashSet<String> = ["alpha".to_string()].into_iter().collect();

        let empties = unpopulated_projects(&populated, &registered);
        let paths: Vec<&str> = empties.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["/work/beta", "/other/beta"],
            "populated and unpinned drop out; the same path appears once"
        );
        assert!(empties.iter().all(|p| p.label == "beta"));

        let all_populated: HashSet<String> = ["beta".to_string(), "alpha".to_string()]
            .into_iter()
            .collect();
        assert!(unpopulated_projects(&all_populated, &registered).is_empty());
    }

    #[test]
    fn new_project_defaults_unpinned_legacy_json_defaults_pinned() {
        assert!(!Project::new("r", "/tmp/r", ProjectScope::Global).pinned);
        let legacy = r#"[{"name":"r","path":"/tmp/r"}]"#;
        let parsed: Vec<Project> = serde_json::from_str(legacy).unwrap();
        assert!(parsed[0].pinned);
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
    fn is_git_probes_filesystem_per_call() {
        let temp = tempdir().expect("tempdir");
        let dir = temp.path().join("workspace");
        std::fs::create_dir_all(&dir).expect("create dir");

        let project = Project::new("workspace", dir.to_string_lossy(), ProjectScope::Global);
        assert!(!project.is_git());

        git2::Repository::init(&dir).expect("git init");
        assert!(project.is_git());
    }

    #[test]
    #[serial]
    fn default_base_branch_persists_through_add_and_load() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
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
        let _app_dir = isolate_app_dir_at(temp.path());
        let repo = temp.path().join("repoUpd");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("repoUpd", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )?;

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

        let cleared = update_base_branch(
            "default",
            ProjectScope::Global,
            &repo.to_string_lossy(),
            Some("   ".into()),
        )?;
        assert_eq!(cleared.default_base_branch, None);
        assert_eq!(load_global()?[0].default_base_branch, None);

        assert!(matches!(
            update_base_branch("default", ProjectScope::Global, "nope", Some("x".into())),
            Err(RegistryError::NotFound(_))
        ));
        Ok(())
    }

    #[test]
    #[serial]
    fn set_pinned_toggles_without_removing_entry() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        let repo = temp.path().join("repoPin");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("repoPin", repo.to_string_lossy(), ProjectScope::Global).with_pinned(true),
            false,
        )?;
        assert!(load_global()?[0].pinned);

        let unpinned = set_pinned("default", ProjectScope::Global, "repoPin", false)?;
        assert!(!unpinned.pinned);
        let loaded = load_global()?;
        assert_eq!(loaded.len(), 1);
        assert!(!loaded[0].pinned);

        let repinned = set_pinned(
            "default",
            ProjectScope::Global,
            &repo.to_string_lossy(),
            true,
        )?;
        assert!(repinned.pinned);
        assert!(load_global()?[0].pinned);

        assert!(matches!(
            set_pinned("default", ProjectScope::Global, "nope", true),
            Err(RegistryError::NotFound(_))
        ));
        Ok(())
    }

    #[test]
    #[serial]
    fn add_then_load_global() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
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
        let _app_dir = isolate_app_dir_at(temp.path());
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
        let _app_dir = isolate_app_dir_at(temp.path());
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
        let _app_dir = isolate_app_dir_at(temp.path());
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

        let err = add(
            "default",
            ProjectScope::Global,
            Project::new("mixedcase", repo2.to_string_lossy(), ProjectScope::Global),
            false,
        );
        assert!(err.is_err(), "duplicate name (different case) should error");

        let resolved = resolve_names("default", &["MIXEDCASE".to_string()])?;
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "MixedCase");

        let removed = remove("default", ProjectScope::Global, "mixedcase")?;
        assert_eq!(removed.name, "MixedCase");
        Ok(())
    }

    #[test]
    #[serial]
    fn cross_scope_path_collision_blocked_by_default() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
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
        let _app_dir = isolate_app_dir_at(temp.path());
        let err = resolve_names("default", &["nonesuch".to_string()]);
        assert!(err.is_err());
        Ok(())
    }

    #[test]
    #[serial]
    fn remove_round_trip() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
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

    #[test]
    #[serial]
    fn update_overrides_sets_and_clears_individual_fields() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        let repo = temp.path().join("demo");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("demo", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )?;

        let updated = update_overrides("default", ProjectScope::Global, "demo", |ov| {
            ov.worktree_enabled = Some(true);
        })?;
        assert_eq!(updated.overrides.worktree_enabled, Some(true));
        assert_eq!(updated.overrides.smart_rename, None);

        let updated = update_overrides("default", ProjectScope::Global, "demo", |ov| {
            ov.smart_rename = Some(false);
        })?;
        assert_eq!(updated.overrides.worktree_enabled, Some(true));
        assert_eq!(updated.overrides.smart_rename, Some(false));

        let updated = update_overrides("default", ProjectScope::Global, "demo", |ov| {
            ov.worktree_enabled = None;
        })?;
        assert_eq!(updated.overrides.worktree_enabled, None);
        assert_eq!(updated.overrides.smart_rename, Some(false));
        Ok(())
    }

    #[test]
    #[serial]
    fn find_by_canonical_path_matches_registered_project() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        let repo = temp.path().join("demo");
        let _ = git2::Repository::init(&repo);

        add(
            "default",
            ProjectScope::Global,
            Project::new("demo", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )?;

        let found = find_by_canonical_path("default", repo.as_path());
        assert_eq!(found.map(|p| p.name), Some("demo".to_string()));
        assert!(find_by_canonical_path("default", Path::new("/nope")).is_none());
        Ok(())
    }

    #[test]
    #[serial]
    fn unique_name_suffixes_on_collision() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        let repo_a = temp.path().join("repoA");
        let _ = git2::Repository::init(&repo_a);

        add(
            "default",
            ProjectScope::Global,
            Project::new("demo", repo_a.to_string_lossy(), ProjectScope::Global),
            false,
        )?;
        assert_eq!(
            unique_name("default", ProjectScope::Global, "demo"),
            "demo-2"
        );
        assert_eq!(
            unique_name("default", ProjectScope::Global, "other"),
            "other"
        );
        Ok(())
    }
}

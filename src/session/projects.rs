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
    #[serial]
    fn registry_crud_by_name_or_path() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        let repo = temp.path().join("Mixed");
        let other = temp.path().join("Other");
        let _ = git2::Repository::init(&repo);
        let _ = git2::Repository::init(&other);
        let global = ProjectScope::Global;
        let path = repo.to_string_lossy().to_string();

        add(
            "default",
            global,
            Project::new("MixedCase", &*path, global)
                .with_pinned(true)
                .with_base_branch(Some("  develop ".to_string())),
            false,
        )?;
        let loaded = load_global()?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            (loaded[0].name.as_str(), loaded[0].scope, loaded[0].pinned),
            ("MixedCase", global, true)
        );
        assert_eq!(loaded[0].default_base_branch.as_deref(), Some("develop"));

        for name in ["MixedCase", "mixedcase"] {
            let dup = Project::new(name, other.to_string_lossy(), global);
            assert!(add("default", global, dup, false).is_err(), "{name}");
        }
        assert_eq!(unique_name("default", global, "mixedcase"), "mixedcase-2");
        assert_eq!(unique_name("default", global, "other"), "other");
        assert_eq!(
            resolve_names("default", &["MIXEDCASE".into()])?[0].name,
            "MixedCase"
        );
        assert!(resolve_names("default", &["nonesuch".into()]).is_err());
        assert_eq!(
            find_by_canonical_path("default", repo.as_path()).map(|p| p.name),
            Some("MixedCase".to_string())
        );
        assert!(find_by_canonical_path("default", Path::new("/nope")).is_none());

        let cleared = update_base_branch("default", global, &path, Some("   ".into()))?;
        assert_eq!(cleared.default_base_branch, None);
        assert_eq!(load_global()?[0].default_base_branch, None);
        assert!(!set_pinned("default", global, "mixedcase", false)?.pinned);
        assert!(!load_global()?[0].pinned);

        let updated = update_overrides("default", global, "MixedCase", |ov| {
            ov.worktree_enabled = Some(true);
            ov.smart_rename = Some(false);
        })?;
        assert_eq!(updated.overrides.worktree_enabled, Some(true));
        let updated = update_overrides("default", global, "MixedCase", |ov| {
            ov.worktree_enabled = None;
        })?;
        assert_eq!(updated.overrides.worktree_enabled, None);
        assert_eq!(updated.overrides.smart_rename, Some(false));

        assert!(matches!(
            update_base_branch("default", global, "nope", Some("x".into())),
            Err(RegistryError::NotFound(_))
        ));
        assert!(matches!(
            set_pinned("default", global, "nope", true),
            Err(RegistryError::NotFound(_))
        ));
        assert_eq!(remove("default", global, "mixedcase")?.name, "MixedCase");
        assert!(load_global()?.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn cross_scope_path_collision_needs_override_and_profile_shadows_global() -> Result<()> {
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
        let second = || Project::new("second", repo.to_string_lossy(), ProjectScope::Profile);
        let msg = add("default", ProjectScope::Profile, second(), false)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("--allow-override") && msg.contains("global"),
            "error should mention --allow-override and the other scope, got: {msg}"
        );

        add("default", ProjectScope::Profile, second(), true)?;
        let merged = load_merged("default")?;
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "second");
        assert_eq!(merged[0].scope, ProjectScope::Profile);
        Ok(())
    }
}

//! Project registry: saved repo paths the user can pick from when creating
//! a multi-repo session. Two scopes:
//! - Global: `<app_dir>/projects.json`, visible from every profile.
//! - Profile: `<app_dir>/profiles/{profile}/projects.json`, visible only inside that profile.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

use super::{get_app_dir, get_profile_dir_path};

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
    /// Default base branch for new worktree branches created against this
    /// project's repo, whether it is the launch repo or an extra repo in a
    /// multi-repo workspace. An explicit session base wins; when `None`,
    /// resolution falls back to the global/profile `worktree.default_base_branch`,
    /// then the repo's detected default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    /// Whether this project shows as an empty (sessionless) header in the
    /// sidebar / project view. A registry entry is the "saved project" (it
    /// feeds the Projects view and the new-session wizard); the pin is the
    /// separate decision to keep its header visible without sessions. Unpin
    /// clears this flag but keeps the entry; only an explicit remove deletes
    /// the entry. See #2208.
    ///
    /// Missing in JSON written before #2208 deserializes to `true`: every
    /// registered project was implicitly pinned then, so an upgrade preserves
    /// the existing headers rather than silently hiding them. New entries
    /// (`Project::new`, the create API, `aoe project add`) default to `false`,
    /// so saving a project no longer forces a sidebar header.
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

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct ProjectPatch {
    /// Absent preserves the value; present None clears it.
    #[serde(
        rename = "default_base_branch",
        skip_serializing_if = "Option::is_none"
    )]
    pub base_branch: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// Absent preserves the override; present None restores inheritance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_enabled: Option<Option<bool>>,
    /// Absent preserves the override; present None restores inheritance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smart_rename: Option<Option<bool>>,
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
        self.default_base_branch = normalize_base_branch(base);
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

    /// Whether this project's path is currently a git repository (a working
    /// tree, a bare repo, or a linked worktree). This is the single source of
    /// truth for the registry-level "is this project git-backed?" question;
    /// the registration gates (CLI, web API, TUI) all route through here.
    ///
    /// Probed fresh from the filesystem on every call rather than stored on the
    /// struct: a path's git status can change after registration (a later
    /// `git init`, a clone into the dir, or a deleted `.git`), so the
    /// filesystem is the only reliable source of truth.
    pub fn is_git(&self) -> bool {
        let path = PathBuf::from(&self.path);
        let canonical = path.canonicalize().unwrap_or(path);
        crate::git::GitWorktree::is_git_repo(&canonical)
    }
}

fn normalize_base_branch(base: Option<String>) -> Option<String> {
    base.and_then(|mut base| {
        base.truncate(base.trim_end().len());
        let start = base.len() - base.trim_start().len();
        base.drain(..start);
        (!base.is_empty()).then_some(base)
    })
}

fn global_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("projects.json"))
}

fn profile_path(profile: &str) -> Result<PathBuf> {
    Ok(get_profile_dir_path(profile)?.join("projects.json"))
}

/// The complete committed registry and its retained replacement slot.
#[derive(Debug)]
pub struct ProjectCommit<R> {
    pub result: R,
    pub projects: Vec<Project>,
    pub(crate) target: super::anchored_fs::ResolvedDataFile,
}

impl<R> ProjectCommit<R> {
    pub(crate) fn map_result<S>(self, map: impl FnOnce(R) -> S) -> ProjectCommit<S> {
        ProjectCommit {
            result: map(self.result),
            projects: self.projects,
            target: self.target,
        }
    }
}

pub(crate) fn open_registry(profile: Option<&str>) -> Result<super::anchored_fs::ResolvedDataFile> {
    super::anchored_fs::ResolvedDataFile::open(&match profile {
        Some(profile) => profile_path(profile)?,
        None => global_path()?,
    })
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
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
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
    Ok(merge_project_scopes(
        load_global()?,
        load_profile(profile)?,
        |project| canonical_key(&project.path),
    ))
}

/// Profile rows replace matching global rows without changing their positions.
pub(crate) fn merge_project_scopes<T, K: Eq + std::hash::Hash>(
    global: impl IntoIterator<Item = T>,
    profile: impl IntoIterator<Item = T>,
    key: impl Fn(&T) -> K,
) -> Vec<T> {
    use std::collections::hash_map::Entry;
    let global = global.into_iter();
    let profile = profile.into_iter();
    let capacity = global.size_hint().0.saturating_add(profile.size_hint().0);
    let mut merged = Vec::with_capacity(capacity);
    let mut positions = std::collections::HashMap::with_capacity(capacity);
    for (project, shadow) in global
        .map(|project| (project, false))
        .chain(profile.map(|project| (project, true)))
    {
        match positions.entry(key(&project)) {
            Entry::Occupied(entry) => {
                if shadow {
                    merged[*entry.get()] = project;
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(merged.len());
                merged.push(project);
            }
        }
    }
    merged
}

pub(crate) fn canonical_key<'a>(path: impl Into<std::borrow::Cow<'a, str>>) -> String {
    let path = path.into();
    Path::new(path.as_ref())
        .canonicalize()
        .map(|path| {
            path.into_os_string()
                .into_string()
                .unwrap_or_else(|path| path.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|_| path.into_owned())
}

/// Display label for a repo path: its final path segment, with readable
/// fallbacks for the root and empty cases. This is the header a project
/// renders under in the project-grouped view, so a registered project and
/// the sessions living in the same repo collapse under one header.
///
/// Shared so the TUI's session-derived grouping and the empty-project
/// injection below agree on the label, and so a future server endpoint can
/// reuse the same derivation instead of re-implementing it in TypeScript.
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

/// Given the set of project-header labels that already have at least one live
/// session and the registered projects, return the registered projects whose
/// header would otherwise be invisible. Deduped by canonical path (the stable
/// repo identity), so two repos that merely share a basename are not folded
/// into one entry. A registered project whose label collides with a populated
/// header is omitted, the populated header already carries it (and the pin
/// indicator is derived separately, against the header's own repo path).
///
/// Pure and side-effect free so it can be unit-tested directly and reused by
/// any surface that wants to show pinned-but-empty projects.
pub fn unpopulated_projects(
    populated_labels: &HashSet<String>,
    registered: &[Project],
) -> Vec<UnpopulatedProject> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in registered {
        // Only pinned projects surface as empty headers. An unpinned entry is
        // saved (Projects view / wizard) but not forced into the sidebar; check
        // it before `seen.insert` so an unpinned entry never consumes the slot
        // a pinned entry for the same path would. See #2208.
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

// Freeze aliases before acquiring flocks; lock global before profile.
fn locked_update_scope<R>(
    profile: &str,
    scope: ProjectScope,
    check_other_scope: bool,
    mutate: impl FnOnce(&mut Vec<Project>, &[Project]) -> std::result::Result<R, RegistryError>,
) -> std::result::Result<ProjectCommit<R>, RegistryError> {
    use super::storage::LockedDataFile;

    let global = open_registry(None)?;
    let profile_file = if scope == ProjectScope::Profile || check_other_scope {
        match open_registry(Some(profile)) {
            Ok(file) => Some(file),
            Err(error)
                if scope == ProjectScope::Global
                    && error.downcast_ref::<nix::errno::Errno>()
                        == Some(&nix::errno::Errno::ENOENT) =>
            {
                None
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    let shared = profile_file
        .as_ref()
        .map(|file| global.same_target(file))
        .transpose()?
        .unwrap_or(false);
    let global = LockedDataFile::lock(global)?;
    let (profile_file, _shared_profile) = if shared {
        (None, profile_file)
    } else {
        (profile_file.map(LockedDataFile::lock).transpose()?, None)
    };
    let target = if scope == ProjectScope::Profile {
        profile_file.as_ref().unwrap_or(&global)
    } else {
        &global
    };
    let other = if check_other_scope {
        let (file, other_scope) = match scope {
            ProjectScope::Profile => (Some(&global), ProjectScope::Global),
            ProjectScope::Global => (
                if shared {
                    Some(&global)
                } else {
                    profile_file.as_ref()
                },
                ProjectScope::Profile,
            ),
        };
        match file.map(LockedDataFile::read).transpose()?.flatten() {
            Some(content) if !content.trim().is_empty() => parse_projects(&content, other_scope)?,
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let (result, projects) = target
        .update(
            |content| parse_projects(content, scope),
            |projects| Ok(serde_json::to_string_pretty(projects)?),
            |projects| mutate(projects, &other),
        )
        .map_err(RegistryError::Other)??;
    let target = match (scope, profile_file) {
        (ProjectScope::Profile, Some(profile)) => profile.into_target(),
        _ => global.into_target(),
    };
    Ok(ProjectCommit {
        result,
        projects,
        target,
    })
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
) -> std::result::Result<ProjectCommit<usize>, RegistryError> {
    project.scope = scope;
    let path_buf = PathBuf::from(&project.path);
    let canonical = path_buf.canonicalize().unwrap_or(path_buf);
    project.path = canonical.to_string_lossy().to_string();

    locked_update_scope(profile, scope, !allow_override, |existing, other| {
        for p in existing.iter() {
            if p.name.eq_ignore_ascii_case(&project.name) {
                return Err(RegistryError::Conflict(format!(
                    "Project '{}' already registered in {} scope (as '{}')",
                    project.name,
                    scope.as_str(),
                    p.name,
                )));
            }
            if canonical_key(&p.path) == project.path {
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
            for p in other {
                if canonical_key(&p.path) == project.path {
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

        let index = existing.len();
        existing.push(project);
        Ok(index)
    })
}

/// Return the removed project and the complete committed registry.
pub fn remove(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
) -> std::result::Result<ProjectCommit<Project>, RegistryError> {
    let canonical_target = canonical_key(name_or_path);
    locked_update_scope(profile, scope, false, |existing, _| {
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

/// Update supplied fields in one commit; unpinning keeps the saved project.
/// Returns the row index and the complete committed registry.
pub fn update(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
    patch: ProjectPatch,
) -> std::result::Result<ProjectCommit<usize>, RegistryError> {
    let canonical_target = canonical_key(name_or_path);
    locked_update_scope(profile, scope, false, |existing, _| {
        let idx = existing
            .iter()
            .position(|project| {
                project.name.eq_ignore_ascii_case(name_or_path)
                    || canonical_key(&project.path) == canonical_target
            })
            .ok_or_else(|| {
                RegistryError::NotFound(format!(
                    "No project '{}' in {} scope",
                    name_or_path,
                    scope.as_str()
                ))
            })?;
        if let Some(base) = patch.base_branch {
            existing[idx].default_base_branch = normalize_base_branch(base);
        }
        if let Some(pinned) = patch.pinned {
            existing[idx].pinned = pinned;
        }
        if let Some(enabled) = patch.worktree_enabled {
            existing[idx].overrides.worktree_enabled = enabled;
        }
        if let Some(enabled) = patch.smart_rename {
            existing[idx].overrides.smart_rename = enabled;
        }
        Ok(idx)
    })
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

/// Edit the entry matching `name_or_path` in the given scope under the registry lock.
fn update_entry(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
    mutate: impl FnOnce(&mut Project),
) -> std::result::Result<Project, RegistryError> {
    let canonical_target = canonical_key(name_or_path);
    locked_update_scope(profile, scope, false, |existing, _| {
        let entry = existing
            .iter_mut()
            .find(|project| {
                project.name.eq_ignore_ascii_case(name_or_path)
                    || canonical_key(&project.path) == canonical_target
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
    .map(|commit| commit.result)
}

/// Look up the merged-registry entry (profile shadows global) whose path
/// canonicalizes to `path`, if any. Used to resolve per-project overrides
/// at call sites that only have a filesystem path in hand (no pre-built
/// canonical-path map).
pub fn find_by_canonical_path(profile: &str, path: &Path) -> Option<Project> {
    let target = canonical_key(path.to_string_lossy());
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
    #[serial]
    fn metadata_update_waits_for_registry_writer_and_preserves_its_entries() {
        let _guard = crate::session::test_support::isolate_app_dir();
        add(
            "default",
            ProjectScope::Global,
            Project::new("target", "/tmp/project-target", ProjectScope::Global),
            false,
        )
        .unwrap();
        std::thread::scope(|scope| {
            let (finished, completion) = std::sync::mpsc::channel();
            let mut writer = None;
            let held =
                locked_update_scope("default", ProjectScope::Global, false, |projects, _| {
                    writer = Some(scope.spawn(move || {
                        let result = update(
                            "default",
                            ProjectScope::Global,
                            "target",
                            ProjectPatch {
                                base_branch: Some(Some(" release ".into())),
                                pinned: Some(true),
                                ..Default::default()
                            },
                        );
                        let _ = finished.send(());
                        result
                    }));
                    let escaped = completion
                        .recv_timeout(std::time::Duration::from_millis(250))
                        .is_ok();
                    projects.push(Project::new(
                        "peer",
                        "/tmp/project-peer",
                        ProjectScope::Global,
                    ));
                    Ok(escaped)
                });
            writer.unwrap().join().unwrap().unwrap();
            let ProjectCommit {
                result: escaped,
                projects: committed,
                ..
            } = held.unwrap();
            assert!(!escaped, "metadata writer bypassed the registry flock");
            assert!(!committed[0].pinned);
            assert!(committed[0].default_base_branch.is_none());
        });
        let projects = load_global().unwrap();
        let target = projects
            .iter()
            .find(|project| project.name == "target")
            .unwrap();
        assert!(target.pinned);
        assert_eq!(target.default_base_branch.as_deref(), Some("release"));
        assert!(projects.iter().any(|project| project.name == "peer"));
    }

    #[test]
    #[serial]
    fn registry_add_waits_for_opposite_scope_before_checking_conflicts() {
        let _guard = crate::session::test_support::isolate_app_dir();
        fs::create_dir_all(profile_path("default").unwrap().parent().unwrap()).unwrap();
        std::thread::scope(|scope| {
            let (finished, completion) = std::sync::mpsc::channel();
            let mut writer = None;
            let held = super::super::storage::LockedDataFile::open(&global_path().unwrap())
                .unwrap()
                .update(
                    |content| parse_projects(content, ProjectScope::Global),
                    |projects| Ok(serde_json::to_string_pretty(projects)?),
                    |projects| {
                        writer = Some(scope.spawn(move || {
                            let result = add(
                                "default",
                                ProjectScope::Profile,
                                Project::new(
                                    "profile",
                                    "/tmp/shared-project",
                                    ProjectScope::Profile,
                                ),
                                false,
                            );
                            let _ = finished.send(());
                            result
                        }));
                        let _ = completion.recv_timeout(std::time::Duration::from_millis(250));
                        projects.push(Project::new(
                            "global",
                            "/tmp/shared-project",
                            ProjectScope::Global,
                        ));
                        Ok::<_, RegistryError>(())
                    },
                );
            let result = writer.unwrap().join().unwrap();
            held.unwrap().unwrap();
            assert!(
                matches!(result, Err(RegistryError::Conflict(_))),
                "opposite-scope registration ignored the in-flight writer: {result:?}"
            );
        });
        assert!(load_profile("default").unwrap().is_empty());
    }

    #[test]
    #[serial]
    fn merged_registry_refuses_a_corrupt_scope() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let paths = [global_path().unwrap(), profile_path("default").unwrap()];
        for path in &paths {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "[]").unwrap();
        }
        for path in paths {
            fs::write(&path, "invalid registry").unwrap();
            assert!(
                load_merged("default").is_err(),
                "corrupt registry {} was advertised as complete",
                path.display()
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "invalid registry");
            fs::write(path, "[]").unwrap();
        }
    }

    #[test]
    fn repo_label_uses_basename_with_root_fallbacks() {
        assert_eq!(repo_label("/home/me/myrepo"), "myrepo");
        assert_eq!(repo_label("/home/me/myrepo/"), "myrepo");
        assert_eq!(repo_label("/"), "(root)");
        assert_eq!(repo_label(""), "(root)");
    }

    #[test]
    fn unpopulated_projects_skips_populated_and_keys_on_path() {
        let registered = vec![
            Project::new("alpha", "/work/alpha", ProjectScope::Global).with_pinned(true),
            Project::new("beta", "/work/beta", ProjectScope::Global).with_pinned(true),
            // Same basename as the first beta entry but a distinct repo: it
            // must NOT be folded away by the shared basename, the identity is
            // the path.
            Project::new("beta-other", "/other/beta", ProjectScope::Profile).with_pinned(true),
        ];
        // `alpha` has a live session keeping its header alive, so only the
        // two distinct beta repos surface as empty headers.
        let populated: HashSet<String> = ["alpha".to_string()].into_iter().collect();

        let empties = unpopulated_projects(&populated, &registered);
        let paths: Vec<&str> = empties.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, vec!["/work/beta", "/other/beta"]);
        assert!(empties.iter().all(|p| p.label == "beta"));
    }

    #[test]
    fn unpopulated_projects_dedupes_same_path() {
        let registered = vec![
            Project::new("beta", "/work/beta", ProjectScope::Global).with_pinned(true),
            // Same canonical path registered again (e.g. global + profile
            // shadow): collapse to a single header.
            Project::new("beta", "/work/beta", ProjectScope::Profile).with_pinned(true),
        ];
        let populated: HashSet<String> = HashSet::new();
        let empties = unpopulated_projects(&populated, &registered);
        assert_eq!(empties.len(), 1);
        assert_eq!(empties[0].path, "/work/beta");
    }

    #[test]
    fn unpopulated_projects_empty_when_all_populated() {
        let registered =
            vec![Project::new("alpha", "/work/alpha", ProjectScope::Global).with_pinned(true)];
        let populated: HashSet<String> = ["alpha".to_string()].into_iter().collect();
        assert!(unpopulated_projects(&populated, &registered).is_empty());
    }

    #[test]
    fn unpopulated_projects_skips_unpinned() {
        // A saved-but-unpinned project (the new default) is not forced into the
        // sidebar; only pinned ones surface as empty headers. See #2208.
        let registered = vec![
            Project::new("pinned", "/work/pinned", ProjectScope::Global).with_pinned(true),
            Project::new("saved", "/work/saved", ProjectScope::Global),
        ];
        let populated: HashSet<String> = HashSet::new();
        let empties = unpopulated_projects(&populated, &registered);
        let paths: Vec<&str> = empties.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, vec!["/work/pinned"]);
    }

    #[test]
    fn legacy_registry_entries_remain_pinned() {
        // Preserve headers for registries written before the pin field existed.
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

        // Whitespace clears it back to unset, looking the project up by path.
        let ProjectCommit {
            result: index,
            projects: cleared,
            ..
        } = update(
            "default",
            global,
            &path,
            ProjectPatch {
                base_branch: Some(Some("   ".into())),
                ..Default::default()
            },
        )?;
        assert_eq!(cleared[index].default_base_branch, None);
        // Unpin keeps the saved project (the #2208 behaviour): the row
        // survives the patch and only the flag flips.
        let ProjectCommit {
            result: index,
            projects: unpinned,
            ..
        } = update(
            "default",
            global,
            "mixedcase",
            ProjectPatch {
                pinned: Some(false),
                ..Default::default()
            },
        )?;
        assert!(!unpinned[index].pinned);
        assert_eq!(load_global()?.len(), 1);
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

        // Unknown project is a NotFound.
        assert!(matches!(
            update(
                "default",
                ProjectScope::Global,
                "nope",
                ProjectPatch {
                    base_branch: Some(Some("x".into())),
                    ..Default::default()
                }
            ),
            Err(RegistryError::NotFound(_))
        ));

        assert!(matches!(
            update(
                "default",
                global,
                "nope",
                ProjectPatch {
                    pinned: Some(true),
                    ..Default::default()
                }
            ),
            Err(RegistryError::NotFound(_))
        ));
        assert_eq!(
            remove("default", global, "mixedcase")?.result.name,
            "MixedCase"
        );
        assert!(load_global()?.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn cross_scope_path_collision_needs_override_and_profile_shadows_global() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        fs::create_dir_all(profile_path("default")?.parent().unwrap())?;
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

        // With override, succeeds and the profile row shadows the global one.
        add("default", ProjectScope::Profile, second(), true)?;
        let merged = load_merged("default")?;
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "second");
        assert_eq!(merged[0].scope, ProjectScope::Profile);
        Ok(())
    }
}

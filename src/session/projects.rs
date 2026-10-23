//! Project registry: saved repo paths the user can pick from when creating
//! a multi-repo session. Two scopes:
//! - Global: `<app_dir>/projects.json`, visible from every profile.
//! - Profile: `<app_dir>/profiles/{profile}/projects.json`, visible only inside that profile.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;

use super::{get_app_dir_path, get_profile_dir_path};

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

    /// A guard refused the write under the registry locks.
    #[error("{0}")]
    Forbidden(String),

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
    /// Overrides `sandbox.enabled_by_default` (start in container by default)
    /// for new sessions launched against this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_enabled: Option<bool>,
    /// Plugin settings declared `per_project`, keyed by plugin id then setting key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_settings: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

impl ProjectOverrides {
    pub fn is_empty(&self) -> bool {
        self.worktree_enabled.is_none()
            && self.smart_rename.is_none()
            && self.sandbox_enabled.is_none()
            && self.plugin_settings.is_empty()
    }

    /// Set (`Some`) or clear (`None`) one plugin setting, dropping emptied plugin entries.
    pub fn set_plugin_setting(
        &mut self,
        plugin_id: &str,
        key: &str,
        value: Option<serde_json::Value>,
    ) {
        match value {
            Some(value) => {
                self.plugin_settings
                    .entry(plugin_id.to_string())
                    .or_default()
                    .insert(key.to_string(), value);
            }
            None => {
                if let Some(settings) = self.plugin_settings.get_mut(plugin_id) {
                    settings.remove(key);
                    if settings.is_empty() {
                        self.plugin_settings.remove(plugin_id);
                    }
                }
            }
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

#[derive(Debug, Default, PartialEq)]
pub struct ProjectPatch {
    /// Absent preserves the value; present None clears it.
    pub base_branch: Option<Option<String>>,
    pub pinned: Option<bool>,
    /// Absent preserves the override; present None restores inheritance.
    pub worktree_enabled: Option<Option<bool>>,
    pub smart_rename: Option<Option<bool>>,
    pub sandbox_enabled: Option<Option<bool>>,
    pub plugin_settings: BTreeMap<String, BTreeMap<String, Option<serde_json::Value>>>,
}

impl Serialize for ProjectPatch {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Overrides<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            worktree_enabled: Option<&'a Option<bool>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            smart_rename: Option<&'a Option<bool>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            sandbox_enabled: Option<&'a Option<bool>>,
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            plugin_settings: &'a BTreeMap<String, BTreeMap<String, Option<serde_json::Value>>>,
        }
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            default_base_branch: Option<&'a Option<String>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pinned: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            overrides: Option<Overrides<'a>>,
        }
        let overrides = (self.worktree_enabled.is_some()
            || self.smart_rename.is_some()
            || self.sandbox_enabled.is_some()
            || !self.plugin_settings.is_empty())
        .then_some(Overrides {
            worktree_enabled: self.worktree_enabled.as_ref(),
            smart_rename: self.smart_rename.as_ref(),
            sandbox_enabled: self.sandbox_enabled.as_ref(),
            plugin_settings: &self.plugin_settings,
        });
        Body {
            default_base_branch: self.base_branch.as_ref(),
            pinned: self.pinned,
            overrides,
        }
        .serialize(serializer)
    }
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
    Ok(get_app_dir_path()?.join("projects.json"))
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
    pub(crate) effect: Option<EffectiveChange>,
}

impl<R> ProjectCommit<R> {
    pub(crate) fn map_result<S>(self, map: impl FnOnce(R) -> S) -> ProjectCommit<S> {
        ProjectCommit {
            result: map(self.result),
            projects: self.projects,
            target: self.target,
            effect: self.effect,
        }
    }
}

impl<R> ProjectCommit<(R, Option<EffectiveChange>)> {
    fn with_effect(self) -> ProjectCommit<R> {
        ProjectCommit {
            result: self.result.0,
            effect: self.result.1,
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
    let result = (|| {
        let mut file = match crate::process::open_read_nonblocking(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            anyhow::bail!("{} is not a regular file", path.display());
        }
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }
        parse_projects(&content, scope)
    })();
    result.map_err(|error: anyhow::Error| {
        error.context(crate::session::LoadSource(path.to_path_buf()))
    })
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
/// shadow global ones with the same path. Unreadable registries are errors,
/// so failed reads cannot silently discard per-project defaults.
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

// Freeze aliases before acquiring flocks; lock global before profile.
fn locked_update_scope<R>(
    profile: &str,
    scope: ProjectScope,
    check_other_scope: bool,
    mutate: impl FnOnce(
        &mut Vec<Project>,
        &mut Vec<Project>,
        bool,
    ) -> std::result::Result<R, RegistryError>,
) -> std::result::Result<ProjectCommit<R>, RegistryError> {
    use super::storage::LockedDataFile;

    if scope == ProjectScope::Profile {
        super::get_profile_dir(profile)?;
    } else {
        super::get_app_dir()?;
    }
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
    let mut other = if check_other_scope {
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
            |projects| mutate(projects, &mut other, shared),
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
        effect: None,
    })
}

/// [`add`] that also reports what the new entry changes in the plugin values in effect at its path.
/// `guard` sees that change under the registry locks and can veto the write, so the decision and
/// the write cannot be separated by another writer.
pub fn add_guarded(
    profile: &str,
    scope: ProjectScope,
    mut project: Project,
    allow_override: bool,
    guard: impl FnOnce(&EffectiveChange) -> std::result::Result<(), RegistryError>,
) -> std::result::Result<ProjectCommit<usize>, RegistryError> {
    project.scope = scope;
    let path_buf = PathBuf::from(&project.path);
    let canonical = path_buf.canonicalize().unwrap_or(path_buf);
    project.path = canonical.to_string_lossy().to_string();

    locked_update_both(profile, scope, |global, scoped, shared| {
        let before = effective_in(scope, shared, global, scoped, &project.path);
        let (existing, other, other_scope) = match scope {
            ProjectScope::Global => (&mut *global, &*scoped, ProjectScope::Profile),
            ProjectScope::Profile => (&mut *scoped, &*global, ProjectScope::Global),
        };
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
            for p in other {
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

        let index = existing.len();
        existing.push(project.clone());
        let change = EffectiveChange {
            path: project.path.clone(),
            before,
            after: effective_in(scope, shared, global, scoped, &project.path),
        };
        guard(&change)?;
        Ok((index, Some(change)))
    }).map(ProjectCommit::with_effect)
}

/// Return the removed project and the complete committed registry.
pub fn remove(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
) -> std::result::Result<ProjectCommit<Project>, RegistryError> {
    let canonical_target = canonical_key(name_or_path);
    locked_update_scope(profile, scope, false, |existing, _, _| {
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
    let changes_plugin_settings = !patch.plugin_settings.is_empty();
    locked_update_scope(
        profile,
        scope,
        changes_plugin_settings,
        |existing, other, shared| {
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
            let values = |target: &[Project], other: &[Project], path: &str| match scope {
                ProjectScope::Global => effective_in(scope, shared, target, other, path),
                ProjectScope::Profile => effective_in(scope, shared, other, target, path),
            };
            let before = changes_plugin_settings.then(|| {
                let path = existing[idx].path.clone();
                let before = values(existing, other, &path);
                (path, before)
            });
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
            if let Some(enabled) = patch.sandbox_enabled {
                existing[idx].overrides.sandbox_enabled = enabled;
            }
            for (plugin, settings) in patch.plugin_settings {
                for (key, value) in settings {
                    existing[idx]
                        .overrides
                        .set_plugin_setting(&plugin, &key, value);
                }
            }
            let effect = before.map(|(path, before)| {
                let after = values(existing, other, &path);
                EffectiveChange {
                    path,
                    before,
                    after,
                }
            });
            Ok((idx, effect))
        },
    )
    .map(ProjectCommit::with_effect)
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
    locked_update_scope(profile, scope, false, |existing, _, _| {
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
    project: Project,
    allow_override: bool,
) -> std::result::Result<ProjectCommit<usize>, RegistryError> {
    add_guarded(profile, scope, project, allow_override, |_| Ok(()))
}

/// [`remove`] that also reports what removal changes in the plugin values in effect at the entry's
/// path, which can reveal those of an entry it shadowed. `guard` sees the entry and that change
/// under the registry locks and can veto the write.
pub fn remove_guarded(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
    guard: impl FnOnce(&Project, &EffectiveChange) -> std::result::Result<(), RegistryError>,
) -> std::result::Result<ProjectCommit<Project>, RegistryError> {
    let canonical_target = canonical_key(name_or_path);
    locked_update_both(profile, scope, |global, scoped, shared| {
        let list = match scope {
            ProjectScope::Global => &*global,
            ProjectScope::Profile => &*scoped,
        };
        let idx = list
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
        let entry = list[idx].clone();
        let before = effective_in(scope, shared, global, scoped, &entry.path);
        match scope {
            ProjectScope::Global => global.remove(idx),
            ProjectScope::Profile => scoped.remove(idx),
        };
        let change = EffectiveChange {
            path: entry.path.clone(),
            before,
            after: effective_in(scope, shared, global, scoped, &entry.path),
        };
        guard(&entry, &change)?;
        Ok((entry, Some(change)))
    })
    .map(ProjectCommit::with_effect)
}

/// [`update_overrides`] that also reports what the edit changes in the plugin values in effect at
/// the entry's path (an edit to a shadowed entry changes none).
pub fn update_overrides_with_effect(
    profile: &str,
    scope: ProjectScope,
    name_or_path: &str,
    mutate: impl FnOnce(&mut ProjectOverrides),
) -> std::result::Result<(Project, EffectiveChange), RegistryError> {
    let canonical_target = canonical_key(name_or_path);
    locked_update_both(profile, scope, |global, scoped, shared| {
        let list = match scope {
            ProjectScope::Global => &*global,
            ProjectScope::Profile => &*scoped,
        };
        let idx = list
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
        let path = list[idx].path.clone();
        let before = effective_in(scope, shared, global, scoped, &path);
        let entry = match scope {
            ProjectScope::Global => &mut global[idx],
            ProjectScope::Profile => &mut scoped[idx],
        };
        mutate(&mut entry.overrides);
        let entry = entry.clone();
        let change = EffectiveChange {
            path,
            before,
            after: effective_in(scope, shared, global, scoped, &entry.path),
        };
        Ok((entry, change))
    })
    .map(|commit| commit.result)
}

/// Read-modify-write both registries under their locks, taken in a fixed order, so a decision
/// based on the merged view and the write it guards cannot be interleaved by another writer. An
/// unreadable registry fails the whole edit.
fn locked_update_both<R>(
    profile: &str,
    scope: ProjectScope,
    mutate: impl FnOnce(
        &mut Vec<Project>,
        &mut Vec<Project>,
        bool,
    ) -> std::result::Result<R, RegistryError>,
) -> std::result::Result<ProjectCommit<R>, RegistryError> {
    locked_update_scope(
        profile,
        scope,
        true,
        |existing, other, shared| match scope {
            ProjectScope::Global => mutate(existing, other, shared),
            ProjectScope::Profile => mutate(other, existing, shared),
        },
    )
}

/// Per-project plugin setting values by plugin id, then setting key.
pub type PluginValues = BTreeMap<String, BTreeMap<String, serde_json::Value>>;

/// The per-project plugin values in effect at one path before and after a registry edit. A
/// profile entry shadows a global one, so adding or removing an entry can change them without
/// touching its own values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveChange {
    pub path: String,
    pub before: PluginValues,
    pub after: PluginValues,
}

impl EffectiveChange {
    pub fn is_noop(&self) -> bool {
        self.before == self.after
    }

    /// Plugin ids with the setting keys whose effective value differs.
    pub fn changed_keys(&self) -> Vec<(String, Vec<String>)> {
        let empty = BTreeMap::new();
        let ids: std::collections::BTreeSet<&String> =
            self.before.keys().chain(self.after.keys()).collect();
        ids.into_iter()
            .filter_map(|id| {
                let before = self.before.get(id).unwrap_or(&empty);
                let after = self.after.get(id).unwrap_or(&empty);
                let keys: Vec<String> = before
                    .keys()
                    .chain(after.keys())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .filter(|k| before.get(*k) != after.get(*k))
                    .cloned()
                    .collect();
                (!keys.is_empty()).then(|| (id.clone(), keys))
            })
            .collect()
    }
}

fn effective_in(
    scope: ProjectScope,
    shared: bool,
    global: &[Project],
    profile: &[Project],
    path: &str,
) -> PluginValues {
    let key = canonical_key(path);
    let selected = if shared {
        match scope {
            ProjectScope::Global => global
                .iter()
                .find(|project| canonical_key(&project.path) == key),
            ProjectScope::Profile => profile
                .iter()
                .find(|project| canonical_key(&project.path) == key),
        }
    } else {
        profile
            .iter()
            .find(|project| canonical_key(&project.path) == key)
            .or_else(|| {
                global
                    .iter()
                    .find(|project| canonical_key(&project.path) == key)
            })
    };
    selected
        .map(|project| project.overrides.plugin_settings.clone())
        .unwrap_or_default()
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

/// Like [`find_by_canonical_path`], but an unreadable registry is an error rather than "not
/// registered", for callers whose defaults depend on the project's overrides.
pub fn find_by_canonical_path_strict(profile: &str, path: &Path) -> Result<Option<Project>> {
    let target = canonical_key(path.to_string_lossy());
    Ok(load_merged(profile)?
        .into_iter()
        .find(|p| canonical_key(&p.path) == target))
}

/// Resolve the effective `smart_rename` override for a session: `session_cfg`'s resolved
/// `scratch_smart_rename` for scratch sessions (which have no stable path to key a registry entry
/// on, so the setting lives on `Config` instead), otherwise the registered project's override at
/// `repo_path`, if any.
pub fn resolve_smart_rename_override(
    profile: &str,
    scratch: bool,
    repo_path: &Path,
    session_cfg: &crate::session::config::SessionConfig,
) -> Option<bool> {
    if scratch {
        return session_cfg.scratch_smart_rename.as_override();
    }
    find_by_canonical_path(profile, repo_path).and_then(|p| p.overrides.smart_rename)
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
    use std::fs;
    use tempfile::tempdir;

    #[test]
    #[serial]
    fn readonly_registries_do_not_create_paths_but_mutation_does() {
        let home = tempdir().unwrap();
        let _guard = isolate_app_dir_at(home.path());
        let app = super::super::get_profile_dir_path("explicit")
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(load_global().unwrap().is_empty());
        assert!(load_profile("explicit").unwrap().is_empty());
        assert!(!app.exists());
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for scope in [ProjectScope::Global, ProjectScope::Profile] {
            add(
                "explicit",
                scope,
                Project::new(scope.as_str(), repo.to_string_lossy(), scope),
                scope == ProjectScope::Profile,
            )
            .unwrap();
            if scope == ProjectScope::Global {
                assert!(!super::super::get_profile_dir_path("explicit")
                    .unwrap()
                    .exists());
            }
        }
        assert_eq!(load_global().unwrap()[0].name, "global");
        assert_eq!(load_profile("explicit").unwrap()[0].name, "profile");
    }

    #[cfg(unix)]
    #[test]
    fn nonregular_project_registries_are_refused_without_blocking() {
        const CHILD: &str = "AOE_PROJECT_FIFO_TEST_CHILD";
        const ENTERED: &str = "AOE_PROJECT_FIFO_TEST_ENTERED";
        let thread = std::thread::current();
        let test = thread.name().unwrap();
        if std::env::var(CHILD).as_deref() != Ok(test) {
            let home = tempdir().unwrap();
            let entered = home.path().join("entered");
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .env("TMPDIR", home.path())
                .args(["--exact", test, "--nocapture", "--test-threads=1"])
                .env(CHILD, test)
                .env(ENTERED, &entered)
                .stdin(std::process::Stdio::null());
            let output = crate::process::run_with_timeout_process_group(
                &mut command,
                std::time::Duration::from_secs(10),
            )
            .unwrap()
            .expect("project registry read blocked in the isolated child");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(fs::read_to_string(entered).unwrap(), test);
            return;
        }
        fs::write(std::env::var_os(ENTERED).unwrap(), test).unwrap();
        let home = tempdir().unwrap();
        let fifo = home.path().join("fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let alias = home.path().join("alias");
        std::os::unix::fs::symlink(&fifo, &alias).unwrap();
        for scope in [ProjectScope::Global, ProjectScope::Profile] {
            assert!(read_file(&fifo, scope).is_err());
            assert!(read_file(&alias, scope).is_err());
        }
        let _writer = nix::fcntl::open(
            &fifo,
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NONBLOCK,
            nix::sys::stat::Mode::empty(),
        )
        .unwrap();
        assert!(read_file(&fifo, ProjectScope::Profile).is_err());
        fs::remove_file(&alias).unwrap();
        let regular = home.path().join("regular");
        fs::write(&regular, br#"[{"name":"saved","path":"/repo"}]"#).unwrap();
        std::os::unix::fs::symlink(&regular, &alias).unwrap();
        let projects = read_file(&alias, ProjectScope::Profile).unwrap();
        assert_eq!(projects[0].name, "saved");
        assert_eq!(projects[0].path, "/repo");
    }

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
                locked_update_scope("default", ProjectScope::Global, false, |projects, _, _| {
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

    #[cfg(unix)]
    #[test]
    #[serial]
    fn unreadable_project_registries_are_not_successful_empty_results() {
        let home = tempdir().unwrap();
        let _guard = isolate_app_dir_at(home.path());
        for scope in [ProjectScope::Global, ProjectScope::Profile] {
            let path = match scope {
                ProjectScope::Global => global_path(),
                ProjectScope::Profile => profile_path("explicit"),
            }
            .unwrap();
            assert!(read_file(&path, scope).unwrap().is_empty());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "[]").unwrap();
            assert!(read_file(&path, scope).unwrap().is_empty());
            fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(path.file_name().unwrap(), &path).unwrap();
            let error = read_file(&path, scope).unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(libc::ELOOP)
            );
        }
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
            ov.sandbox_enabled = Some(true);
        })?;
        assert_eq!(updated.overrides.worktree_enabled, Some(true));
        assert_eq!(updated.overrides.sandbox_enabled, Some(true));
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

    #[test]
    #[serial]
    fn plugin_settings_persist_and_profile_entry_shadows_global() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        let repo = temp.path().join("repoP");
        let _ = git2::Repository::init(&repo);
        let set = |scope, name: &str, value| {
            let project = Project::new(name, repo.to_string_lossy(), scope);
            add("default", scope, project, true)?;
            update_overrides("default", scope, name, |ov| {
                ov.set_plugin_setting("acme.kit", "depth", Some(value))
            })
        };
        let depth = |p: Option<Project>| {
            p.and_then(|p| {
                p.overrides.plugin_settings["acme.kit"]
                    .get("depth")
                    .cloned()
            })
        };

        set(ProjectScope::Global, "g", serde_json::json!(1))?;
        assert_eq!(
            depth(find_by_canonical_path("default", &repo)),
            Some(serde_json::json!(1))
        );
        set(ProjectScope::Profile, "p", serde_json::json!(2))?;
        assert_eq!(
            depth(find_by_canonical_path("default", &repo)),
            Some(serde_json::json!(2)),
            "the profile registry wins over the global one"
        );

        let mut ov = ProjectOverrides::default();
        ov.set_plugin_setting("acme.kit", "depth", Some(serde_json::json!(3)));
        assert!(!ov.is_empty());
        ov.set_plugin_setting("acme.kit", "depth", None);
        assert!(
            ov.is_empty(),
            "clearing the last key drops the plugin entry"
        );
        Ok(())
    }

    #[test]
    #[serial]
    fn registry_edits_report_the_effective_plugin_values_they_change() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        let repo = temp.path().join("repoG");
        let _ = git2::Repository::init(&repo);
        let path = repo.to_string_lossy().to_string();
        let depth = |v: i64| {
            BTreeMap::from([(
                "acme.kit".to_string(),
                BTreeMap::from([("depth".to_string(), serde_json::json!(v))]),
            )])
        };
        let entry = |name: &str, scope| Project::new(name, path.clone(), scope);

        let mut global = entry("g", ProjectScope::Global);
        global.overrides.plugin_settings = depth(7);
        add("default", ProjectScope::Global, global, false)?;

        // An empty profile entry shadows the global values, hiding them.
        let shadow = entry("p", ProjectScope::Profile);
        let change = add_guarded("default", ProjectScope::Profile, shadow, true, |_| Ok(()))?
            .effect
            .expect("guarded add effect");
        assert_eq!(
            (change.before.clone(), change.after.clone()),
            (depth(7), PluginValues::new())
        );
        assert_eq!(
            change.changed_keys(),
            vec![("acme.kit".to_string(), vec!["depth".to_string()])]
        );

        // Removing the shadow reveals them again, though the removed entry holds none.
        let commit = remove_guarded("default", ProjectScope::Profile, "p", |_, _| Ok(()))?;
        let change = commit.effect.expect("guarded removal effect");
        assert!(commit.result.overrides.plugin_settings.is_empty());
        assert_eq!(
            (change.before.clone(), change.after.clone()),
            (PluginValues::new(), depth(7))
        );
        assert!(!change.is_noop());

        // The shadow is gone again, so removing the global entry now drops the values for good.
        let change = remove_guarded("default", ProjectScope::Global, "g", |entry, change| {
            assert!(!entry.overrides.plugin_settings.is_empty());
            assert!(!change.is_noop());
            Ok(())
        })?
        .effect
        .expect("guarded removal effect");
        assert_eq!(change.after, PluginValues::new());
        assert!(matches!(
            remove_guarded("default", ProjectScope::Global, "missing", |_, _| Ok(())),
            Err(RegistryError::NotFound(_))
        ));

        // A guard's veto leaves the registry untouched, and an unreadable registry fails the edit.
        let kept = entry("kept", ProjectScope::Global);
        let refusal = add_guarded("default", ProjectScope::Global, kept, false, |_| {
            Err(RegistryError::Forbidden("no".into()))
        });
        assert!(matches!(refusal, Err(RegistryError::Forbidden(_))));
        assert!(load_global()?.is_empty());
        fs::write(global_path()?, "{ not json")?;
        assert!(remove_guarded("default", ProjectScope::Profile, "p", |_, _| Ok(())).is_err());
        assert!(add_guarded(
            "default",
            ProjectScope::Profile,
            entry("x", ProjectScope::Profile),
            false,
            |_| Ok(())
        )
        .is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn a_profile_edit_does_not_rewrite_an_unwritable_global_registry() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        let (one, two) = (temp.path().join("one"), temp.path().join("two"));
        let _ = git2::Repository::init(&one);
        let _ = git2::Repository::init(&two);
        let project = |name: &str, path: &std::path::Path| {
            Project::new(name, path.to_string_lossy(), ProjectScope::Profile)
        };
        // Both registries and their lock files exist and are readable; only new files in the
        // global registry's directory are refused.
        add(
            "default",
            ProjectScope::Global,
            Project::new("g", one.to_string_lossy(), ProjectScope::Global),
            false,
        )?;
        add("default", ProjectScope::Profile, project("p0", &one), true)?;
        let dir = global_path()?.parent().expect("registry dir").to_path_buf();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555))?;
        let writable = fs::File::create(dir.join("probe")).is_ok();

        let added = add("default", ProjectScope::Profile, project("p1", &two), false);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
        if writable {
            return Ok(()); // root ignores directory permissions, so nothing to prove
        }
        added?;
        let names: Vec<String> = load_profile("default")?
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["p0", "p1"]);
        Ok(())
    }

    #[test]
    #[serial]
    fn a_guard_sees_the_state_a_concurrent_writer_committed() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());
        let repo = temp.path().join("repoC");
        let _ = git2::Repository::init(&repo);
        add(
            "default",
            ProjectScope::Global,
            Project::new("g", repo.to_string_lossy(), ProjectScope::Global),
            false,
        )?;
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        // The writer holds the registry locks while it prepares the protected value; the remover
        // starts only once it does, so it must wait for the commit and then see the new value.
        let removal = std::thread::scope(|s| {
            let writer = s.spawn(move || {
                update_overrides_with_effect("default", ProjectScope::Global, "g", |ov| {
                    held_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    ov.set_plugin_setting("acme.kit", "depth", Some(serde_json::json!(1)));
                })
            });
            held_rx.recv().unwrap();
            let remover = s.spawn(|| {
                remove_guarded("default", ProjectScope::Global, "g", |entry, change| {
                    if entry.overrides.plugin_settings.is_empty() && change.is_noop() {
                        return Ok(());
                    }
                    Err(RegistryError::Forbidden("needs elevation".into()))
                })
            });
            release_tx.send(()).unwrap();
            writer.join().unwrap().expect("writer commits");
            remover.join().unwrap()
        });

        assert!(matches!(removal, Err(RegistryError::Forbidden(_))));
        let kept = find_by_canonical_path("default", &repo).expect("entry survives");
        assert_eq!(
            kept.overrides.plugin_settings["acme.kit"]["depth"],
            serde_json::json!(1)
        );
        Ok(())
    }

    #[test]
    #[serial]
    fn merged_load_fails_on_an_unreadable_registry() -> Result<()> {
        let temp = tempdir()?;
        let _app_dir = isolate_app_dir_at(temp.path());

        // A registry that does not exist is empty, not broken.
        assert!(load_merged("default")?.is_empty());

        for path in [global_path()?, profile_path("default")?] {
            fs::create_dir_all(path.parent().expect("registry dir"))?;
            fs::write(&path, "{ not json")?;
            assert!(load_merged("default").is_err(), "{}", path.display());
            assert!(find_by_canonical_path_strict("default", temp.path()).is_err());
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn resolve_smart_rename_override_uses_session_cfg_for_scratch_sessions() -> Result<()> {
        use crate::session::config::{ScratchSmartRenameMode, SessionConfig};

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
        update_overrides("default", ProjectScope::Global, "demo", |ov| {
            ov.smart_rename = Some(false);
        })?;

        let mut session_cfg = SessionConfig {
            scratch_smart_rename: ScratchSmartRenameMode::On,
            ..Default::default()
        };

        assert_eq!(
            resolve_smart_rename_override("default", false, &repo, &session_cfg),
            Some(false),
            "non-scratch session resolves the registered project's override, ignoring session_cfg"
        );
        assert_eq!(
            resolve_smart_rename_override("default", true, &repo, &session_cfg),
            Some(true),
            "scratch session ignores repo_path and resolves session_cfg's scratch_smart_rename instead"
        );

        session_cfg.scratch_smart_rename = ScratchSmartRenameMode::Inherit;
        assert_eq!(
            resolve_smart_rename_override("default", true, &repo, &session_cfg),
            None,
            "Inherit defers to the resolved smart_rename toggle"
        );
        Ok(())
    }
}

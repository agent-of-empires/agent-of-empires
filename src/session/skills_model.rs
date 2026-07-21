//! Always-compiled, plugin-facing model of the agent-skill set (#2984).
//!
//! A "skill" is a `SKILL.md` folder (the Claude Code / kimi concept): YAML
//! frontmatter (`name`, `description`, plus optional metadata) between `---`
//! fences, then a markdown body, living in a per-skill directory. AoE has never
//! had a Rust model for these; they were only bulk-copied into sandboxes
//! (`src/session/container_config.rs`). This module is the single resolver the
//! plugin host RPCs (`src/plugin/host_api.rs`) read and mutate.
//!
//! Two provenance layers, mirroring [`super::mcp_model::McpProvenance`]:
//! host-discovered skills in each agent's own skills dir (`~/.claude/skills`,
//! `~/.kimi-code/skills`) are READ-ONLY; the AoE-managed store at
//! `<app_dir>/skills` is the only WRITABLE layer. Editing a host-discovered
//! skill requires adopting it into the managed store first.
//!
//! Identity is the skill's DIRECTORY name, never the frontmatter `name` (which
//! is mutable display metadata and can collide or diverge). The same directory
//! name can exist under several provenances, so read/adopt/propagate are always
//! source-qualified. This module does NOT define precedence/shadowing between
//! layers: [`discover`] returns every source-qualified entry as-is.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

/// Reject a `SKILL.md` larger than this before parsing, so a pathological file
/// cannot make the host read an unbounded amount into memory.
pub const MAX_SKILL_MD_BYTES: u64 = 1024 * 1024;

/// Host-discovered skill dirs, keyed by agent registry name. The `agent` string
/// is both the provenance label and the `skills.propagate` target key. Same
/// small-const-table pattern as [`super::mcp_model`]'s `native_config_for`.
const AGENT_SKILL_DIRS: &[(&str, &str)] =
    &[("claude", ".claude/skills"), ("kimi", ".kimi-code/skills")];

/// Where a skill was discovered. The read-only host layers carry the agent key;
/// the single writable layer is [`SkillProvenance::AoeManaged`]. Serializes to a
/// tagged object (`{ "kind": "agent-native", "agent": "claude" }` /
/// `{ "kind": "aoe-managed" }`) so it round-trips as both `skills.list` output
/// and a source-qualified `skills.read` / `skills.adopt` parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SkillProvenance {
    AgentNative { agent: String },
    AoeManaged,
}

impl SkillProvenance {
    /// The provenance string shown in logs, e.g. `agent-native:claude`,
    /// `aoe-managed`.
    pub fn label(&self) -> String {
        match self {
            SkillProvenance::AgentNative { agent } => format!("agent-native:{agent}"),
            SkillProvenance::AoeManaged => "aoe-managed".to_string(),
        }
    }

    /// Only the AoE-managed layer accepts writes; host-discovered skills are
    /// read-only and must be adopted before editing.
    pub fn is_writable(&self) -> bool {
        matches!(self, SkillProvenance::AoeManaged)
    }
}

/// One discovered skill's list-safe metadata: its identity (`directory`), its
/// frontmatter `name`/`description`, and where it came from. The body is not
/// included; `skills.read` returns that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSkill {
    pub provenance: SkillProvenance,
    pub directory: String,
    pub name: String,
    pub description: String,
}

/// A skill read in full: its metadata plus the raw `SKILL.md` content.
#[derive(Debug, Clone)]
pub struct ReadSkill {
    pub provenance: SkillProvenance,
    pub directory: String,
    pub name: String,
    pub description: String,
    pub content: String,
}

/// A skills store operation that failed for a caller-attributable reason. The
/// plugin host maps each variant to a JSON-RPC code: [`Self::ReadOnly`] to
/// `FORBIDDEN`, [`Self::Io`] to `INTERNAL_ERROR`, everything else to
/// `INVALID_PARAMS`.
#[derive(Debug)]
pub enum SkillError {
    /// Bad directory/agent name, unparseable content, or a name/directory
    /// mismatch: the caller's input is wrong.
    InvalidInput(String),
    /// No skill with that identity exists.
    NotFound(String),
    /// The managed destination already exists; the operation never overwrites.
    Collision(String),
    /// The target is a host-discovered (read-only) skill; adopt it first.
    ReadOnly(String),
    /// A filesystem failure the caller cannot fix.
    Io(anyhow::Error),
}

impl From<std::io::Error> for SkillError {
    fn from(e: std::io::Error) -> Self {
        SkillError::Io(e.into())
    }
}

/// Frontmatter fields AoE reads. Unknown keys (`version`, `author`, `metadata`,
/// vendor blocks) are ignored on read and dropped on scaffold.
#[derive(Debug, Serialize, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

/// A parsed `SKILL.md`: the two required frontmatter fields plus the verbatim
/// markdown body.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedSkill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Parse a `SKILL.md`: an opening `---` fence on the first line, a closing `---`
/// line, YAML frontmatter with non-empty `name` and `description`, then the
/// verbatim body. An optional UTF-8 BOM and CRLF line endings are tolerated.
pub fn parse_skill_md(content: &str) -> Result<ParsedSkill> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let after_open = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .context("SKILL.md must begin with a \"---\" frontmatter fence")?;
    let (frontmatter, body) = split_closing_fence(after_open)
        .context("SKILL.md frontmatter is not closed by a \"---\" line")?;
    let fm: Frontmatter = serde_yaml::from_str(frontmatter)
        .context("failed to parse SKILL.md frontmatter as YAML")?;
    if fm.name.trim().is_empty() {
        bail!("SKILL.md frontmatter \"name\" is empty");
    }
    if fm.description.trim().is_empty() {
        bail!("SKILL.md frontmatter \"description\" is empty");
    }
    Ok(ParsedSkill {
        name: fm.name,
        description: fm.description,
        body: body.to_string(),
    })
}

/// Split the post-opening-fence text at the first line that is exactly `---`
/// (CRLF tolerated), returning `(frontmatter, body)`. `None` if no closing fence
/// exists.
fn split_closing_fence(after_open: &str) -> Option<(&str, &str)> {
    let mut idx = 0;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((&after_open[..idx], &after_open[idx + line.len()..]));
        }
        idx += line.len();
    }
    None
}

/// The AoE-managed skills store directory, `<app_dir>/skills`. This is the only
/// writable layer and one of the two roots the `fs.*` RPCs may touch.
pub fn managed_skills_dir() -> Result<PathBuf> {
    Ok(super::get_app_dir()?.join("skills"))
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("could not resolve home dir for skills discovery")
}

/// Discover every skill across all host-discovered agent dirs and the managed
/// store, source-qualified and sorted deterministically (by provenance label,
/// then directory). A malformed or unreadable skill warns and is skipped; it
/// never fails the whole scan. Roots are injected so tests need no real `$HOME`.
pub fn discover(home: &Path, app_dir: &Path) -> Vec<DiscoveredSkill> {
    let mut out = Vec::new();
    for (agent, rel) in AGENT_SKILL_DIRS {
        collect_from_dir(
            &home.join(rel),
            &SkillProvenance::AgentNative {
                agent: (*agent).to_string(),
            },
            &mut out,
        );
    }
    collect_from_dir(
        &app_dir.join("skills"),
        &SkillProvenance::AoeManaged,
        &mut out,
    );
    out.sort_by(|a, b| {
        a.provenance
            .label()
            .cmp(&b.provenance.label())
            .then_with(|| a.directory.cmp(&b.directory))
    });
    out
}

/// Convenience wrapper resolving the real `$HOME` and app dir.
pub fn discover_all() -> Result<Vec<DiscoveredSkill>> {
    Ok(discover(&home_dir()?, &super::get_app_dir()?))
}

/// Enumerate immediate child dirs of `root` that hold a `SKILL.md`, parse each,
/// and push the metadata. Symlinked children, dot-directories (including our own
/// `.tmp-*` staging dirs), and symlinked `SKILL.md` files are skipped.
fn collect_from_dir(root: &Path, provenance: &SkillProvenance, out: &mut Vec<DiscoveredSkill>) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!(target: "session.skills", root = %root.display(), error = %e, "failed to read skills dir");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {}
            _ => continue,
        }
        let skill_md = entry.path().join("SKILL.md");
        match std::fs::symlink_metadata(&skill_md) {
            Ok(m) if m.file_type().is_file() => {}
            _ => continue,
        }
        let content = match read_file_capped(&skill_md, MAX_SKILL_MD_BYTES) {
            Ok(c) => c,
            Err(e) => {
                warn!(target: "session.skills", path = %skill_md.display(), error = %e, "failed to read SKILL.md");
                continue;
            }
        };
        match parse_skill_md(&content) {
            Ok(parsed) => out.push(DiscoveredSkill {
                provenance: provenance.clone(),
                directory: name,
                name: parsed.name,
                description: parsed.description,
            }),
            Err(e) => {
                warn!(target: "session.skills", path = %skill_md.display(), error = %e, "skipping malformed SKILL.md");
            }
        }
    }
}

/// Read one source-qualified skill in full.
pub fn read_skill(
    home: &Path,
    app_dir: &Path,
    provenance: &SkillProvenance,
    directory: &str,
) -> Result<ReadSkill, SkillError> {
    validate_dir_name(directory)?;
    let root = skill_root_for(home, app_dir, provenance)?;
    let dir = resolve_skill_dir(&root, directory)?;
    let skill_md = dir.join("SKILL.md");
    match std::fs::symlink_metadata(&skill_md) {
        Ok(m) if m.file_type().is_file() => {}
        Ok(_) => return Err(SkillError::NotFound(directory.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SkillError::NotFound(directory.to_string()))
        }
        Err(e) => return Err(e.into()),
    }
    let content = read_file_capped(&skill_md, MAX_SKILL_MD_BYTES)
        .map_err(|e| SkillError::InvalidInput(e.to_string()))?;
    let parsed = parse_skill_md(&content).map_err(|e| SkillError::InvalidInput(e.to_string()))?;
    Ok(ReadSkill {
        provenance: provenance.clone(),
        directory: directory.to_string(),
        name: parsed.name,
        description: parsed.description,
        content,
    })
}

/// Create a new managed skill with a scaffolded `SKILL.md` (frontmatter `name`
/// equal to the directory). Rejects an unsafe name or a collision; never
/// overwrites. Built in a staging dir and renamed into place.
pub fn create_skill(
    app_dir: &Path,
    directory: &str,
    description: Option<&str>,
) -> Result<(), SkillError> {
    validate_dir_name(directory)?;
    let managed = app_dir.join("skills");
    let final_path = managed.join(directory);
    if final_path.exists() {
        return Err(SkillError::Collision(directory.to_string()));
    }
    let description = description
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or("Describe when this skill should be used.");
    let content = scaffold(directory, description).map_err(SkillError::Io)?;
    if content.len() as u64 > MAX_SKILL_MD_BYTES {
        return Err(SkillError::InvalidInput(
            "scaffolded SKILL.md exceeds the size limit".to_string(),
        ));
    }
    parse_skill_md(&content).map_err(|e| SkillError::InvalidInput(e.to_string()))?;
    std::fs::create_dir_all(&managed)?;
    let staging = new_staging_dir(&managed)?;
    let result = (|| {
        std::fs::write(staging.join("SKILL.md"), &content)?;
        std::fs::rename(&staging, &final_path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result.map_err(Into::into)
}

/// Overwrite a managed skill's `SKILL.md` with validated content. A
/// host-discovered target is [`SkillError::ReadOnly`] (adopt first); an unknown
/// one is [`SkillError::NotFound`]. Content must parse; the frontmatter `name`
/// need not equal the directory (identity is the folder, and discovery already
/// allows the two to diverge), so an adopted skill whose name differs from its
/// directory stays editable.
pub fn edit_skill(
    home: &Path,
    app_dir: &Path,
    directory: &str,
    content: &str,
) -> Result<(), SkillError> {
    validate_dir_name(directory)?;
    let managed_root = app_dir.join("skills");
    if !managed_root.join(directory).exists() {
        return Err(absent_write_target(home, directory));
    }
    // The managed dir exists: confirm it is a real, in-store directory (not a
    // symlink pointing at a host path) before writing SKILL.md into it.
    let managed_dir = resolve_skill_dir(&managed_root, directory)?;
    let managed_md = managed_dir.join("SKILL.md");
    if !managed_md.is_file() {
        return Err(absent_write_target(home, directory));
    }
    if content.len() as u64 > MAX_SKILL_MD_BYTES {
        return Err(SkillError::InvalidInput(
            "SKILL.md is too large".to_string(),
        ));
    }
    parse_skill_md(content).map_err(|e| SkillError::InvalidInput(e.to_string()))?;
    write_atomic(&managed_md, content)?;
    Ok(())
}

/// Delete a managed skill directory. A host-discovered target is
/// [`SkillError::ReadOnly`]; an unknown one is [`SkillError::NotFound`]; a
/// symlinked managed entry is refused.
pub fn delete_skill(home: &Path, app_dir: &Path, directory: &str) -> Result<(), SkillError> {
    validate_dir_name(directory)?;
    let managed_path = app_dir.join("skills").join(directory);
    let meta = match std::fs::symlink_metadata(&managed_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(absent_write_target(home, directory))
        }
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        return Err(SkillError::InvalidInput(
            "refusing to delete a symlinked skill entry".to_string(),
        ));
    }
    std::fs::remove_dir_all(&managed_path)?;
    Ok(())
}

/// Copy a host-discovered skill into the managed store, leaving the original
/// untouched. `dest` defaults to the source directory name. Rejects adopting an
/// already-managed skill, an unknown source, or a colliding destination;
/// symlinks in the source tree are refused. Copied through a staging dir.
pub fn adopt_skill(
    home: &Path,
    app_dir: &Path,
    source: &SkillProvenance,
    directory: &str,
    dest: Option<&str>,
) -> Result<String, SkillError> {
    validate_dir_name(directory)?;
    let agent = match source {
        SkillProvenance::AgentNative { agent } => agent,
        SkillProvenance::AoeManaged => {
            return Err(SkillError::InvalidInput(
                "cannot adopt an already AoE-managed skill".to_string(),
            ))
        }
    };
    let src_dir = resolve_skill_dir(&agent_skill_dir(home, agent)?, directory)?;
    validate_skill_md_at(&src_dir, directory)?;
    let dest_name = dest.unwrap_or(directory);
    validate_dir_name(dest_name)?;
    let managed = app_dir.join("skills");
    let final_path = managed.join(dest_name);
    if final_path.exists() {
        return Err(SkillError::Collision(dest_name.to_string()));
    }
    std::fs::create_dir_all(&managed)?;
    let staging = new_staging_dir(&managed)?;
    let result = copy_tree_no_symlinks(&src_dir, &staging)
        .and_then(|()| std::fs::rename(&staging, &final_path).map_err(Into::into));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result.map_err(SkillError::Io)?;
    Ok(dest_name.to_string())
}

/// Copy a managed skill into a target agent's host skills dir. The minimal host
/// primitive behind `plugin-skills#4`: managed source only, known agent key
/// only, destination must not already exist (no overwrite, no merge). Copied
/// through a staging dir; symlinks in the source are refused. Marker/dedupe and
/// opt-in policy are deliberately NOT handled here.
pub fn propagate_skill(
    home: &Path,
    app_dir: &Path,
    directory: &str,
    agent: &str,
) -> Result<(), SkillError> {
    validate_dir_name(directory)?;
    let src = resolve_skill_dir(&app_dir.join("skills"), directory)?;
    validate_skill_md_at(&src, directory)?;
    let target_root = agent_skill_dir(home, agent)?;
    let dest = target_root.join(directory);
    if dest.exists() {
        return Err(SkillError::Collision(format!("{agent}:{directory}")));
    }
    std::fs::create_dir_all(&target_root)?;
    let staging = new_staging_dir(&target_root)?;
    let result = copy_tree_no_symlinks(&src, &staging)
        .and_then(|()| std::fs::rename(&staging, &dest).map_err(Into::into));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result.map_err(SkillError::Io)
}

/// The host skills dir for a known agent key, or [`SkillError::InvalidInput`]
/// for an unsupported agent.
fn agent_skill_dir(home: &Path, agent: &str) -> Result<PathBuf, SkillError> {
    AGENT_SKILL_DIRS
        .iter()
        .find(|(a, _)| *a == agent)
        .map(|(_, rel)| home.join(rel))
        .ok_or_else(|| SkillError::InvalidInput(format!("unsupported skills agent {agent:?}")))
}

/// The designated root that a source-qualified skill's directory must live
/// under (the agent's host skills dir, or the managed store).
fn skill_root_for(
    home: &Path,
    app_dir: &Path,
    provenance: &SkillProvenance,
) -> Result<PathBuf, SkillError> {
    match provenance {
        SkillProvenance::AgentNative { agent } => agent_skill_dir(home, agent),
        SkillProvenance::AoeManaged => Ok(app_dir.join("skills")),
    }
}

/// Read a file as UTF-8, refusing more than `max` bytes. Reads through one
/// handle and rejects an overflow byte, so a file that grows after a metadata
/// check cannot slip past the bound (the metadata-then-read TOCTOU).
pub fn read_file_capped(path: &Path, max: u64) -> Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        bail!("file exceeds the {max}-byte limit");
    }
    String::from_utf8(buf).context("file is not valid UTF-8")
}

/// Resolve `root/directory` to a real, non-symlink directory that canonicalizes
/// beneath `root`. This is the guard that stops a symlinked skill directory
/// (e.g. a `<app_dir>/skills/<dir>` symlink pointing at a host path) from
/// letting read/edit/adopt/propagate escape the designated store.
fn resolve_skill_dir(root: &Path, directory: &str) -> Result<PathBuf, SkillError> {
    // Reject a symlinked or non-directory root FIRST. Otherwise, if `root`
    // itself is a symlink pointing outside, both `root` and `root/directory`
    // canonicalize beneath the attacker target and the `starts_with` check below
    // would spuriously pass.
    match std::fs::symlink_metadata(root) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            return Err(SkillError::InvalidInput(
                "skills store root is not a real directory".to_string(),
            ))
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SkillError::NotFound(directory.to_string()))
        }
        Err(e) => return Err(e.into()),
    }
    let dir = root.join(directory);
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SkillError::NotFound(directory.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(SkillError::InvalidInput(format!(
            "skill {directory:?} is not a real directory"
        )));
    }
    let canon_root = std::fs::canonicalize(root)?;
    let canon_dir = std::fs::canonicalize(&dir)?;
    if !canon_dir.starts_with(&canon_root) {
        return Err(SkillError::InvalidInput(format!(
            "skill {directory:?} resolves outside its store"
        )));
    }
    Ok(dir)
}

/// Confirm `dir/SKILL.md` is a regular (non-symlink) file that stays within the
/// byte cap and parses, before an adopt/propagate finalizes. Keeps the store
/// from committing a skill that discovery would skip and `read_skill` reject.
fn validate_skill_md_at(dir: &Path, directory: &str) -> Result<(), SkillError> {
    let md = dir.join("SKILL.md");
    match std::fs::symlink_metadata(&md) {
        Ok(m) if m.file_type().is_file() => {}
        Ok(_) => return Err(SkillError::NotFound(directory.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SkillError::NotFound(directory.to_string()))
        }
        Err(e) => return Err(e.into()),
    }
    let content = read_file_capped(&md, MAX_SKILL_MD_BYTES)
        .map_err(|e| SkillError::InvalidInput(e.to_string()))?;
    parse_skill_md(&content).map_err(|e| SkillError::InvalidInput(e.to_string()))?;
    Ok(())
}

/// Classify a write whose managed target does not exist: a host-discovered
/// skill of the same directory is read-only (adopt first), otherwise it is
/// simply absent.
fn absent_write_target(home: &Path, directory: &str) -> SkillError {
    for (_, rel) in AGENT_SKILL_DIRS {
        if home.join(rel).join(directory).join("SKILL.md").is_file() {
            return SkillError::ReadOnly(format!(
                "skill {directory:?} is host-discovered and read-only; adopt it first"
            ));
        }
    }
    SkillError::NotFound(directory.to_string())
}

/// A fresh, uniquely named staging dir under `parent`, created empty. Renamed
/// into its final place by the caller; the `.tmp-` prefix keeps discovery from
/// ever surfacing a half-built skill.
fn new_staging_dir(parent: &Path) -> Result<PathBuf, SkillError> {
    let path = parent.join(format!(".tmp-{}", Uuid::new_v4()));
    std::fs::create_dir(&path)?;
    Ok(path)
}

/// Recursively copy `src` into `dst`, refusing symlinks and special files so a
/// skill package can never smuggle a link that escapes the destination tree.
fn copy_tree_no_symlinks(src: &Path, dst: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_symlink() {
            bail!("refusing to copy symlink {}", from.display());
        } else if ft.is_dir() {
            copy_tree_no_symlinks(&from, &to)?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to)?;
        } else {
            bail!("refusing to copy special file {}", from.display());
        }
    }
    Ok(())
}

/// Atomically replace `path`'s contents by writing a sibling temp file and
/// renaming over it.
fn write_atomic(path: &Path, content: &str) -> Result<(), SkillError> {
    let parent = path
        .parent()
        .ok_or_else(|| SkillError::Io(anyhow::anyhow!("SKILL.md path has no parent")))?;
    let tmp = parent.join(format!(".tmp-{}", Uuid::new_v4()));
    std::fs::write(&tmp, content)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

/// Build a scaffolded `SKILL.md` with the frontmatter emitted by `serde_yaml`
/// (so a `description` with YAML-significant characters is quoted correctly).
fn scaffold(directory: &str, description: &str) -> Result<String> {
    let fm = serde_yaml::to_string(&Frontmatter {
        name: directory.to_string(),
        description: description.to_string(),
    })?;
    Ok(format!("---\n{fm}---\n\n# {directory}\n\n{description}\n"))
}

/// A skill directory name is the on-disk identity and is joined onto host paths,
/// so it is confined to a conservative portable grammar: 1..=64 chars of ASCII
/// alphanumerics plus `-` and `_`. This forbids `.`, `..`, path separators, and
/// leading dots, which is what keeps a name from escaping its store.
fn validate_dir_name(name: &str) -> Result<(), SkillError> {
    if name.is_empty() || name.len() > 64 {
        return Err(SkillError::InvalidInput(format!(
            "skill name {name:?} must be 1..=64 characters"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(SkillError::InvalidInput(format!(
            "skill name {name:?} may contain only ASCII letters, digits, '-', and '_'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, directory: &str, name: &str, description: &str) {
        let d = dir.join(directory);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn parse_extracts_fields_and_preserves_body() {
        let p =
            parse_skill_md("---\nname: foo\ndescription: does foo\n---\n\n# Foo\n\nbody text\n")
                .unwrap();
        assert_eq!(p.name, "foo");
        assert_eq!(p.description, "does foo");
        assert_eq!(p.body, "\n# Foo\n\nbody text\n");
    }

    #[test]
    fn parse_tolerates_crlf_and_bom() {
        let p = parse_skill_md("\u{feff}---\r\nname: foo\r\ndescription: d\r\n---\r\nbody\r\n")
            .unwrap();
        assert_eq!(p.name, "foo");
        assert_eq!(p.body, "body\r\n");
    }

    #[test]
    fn parse_rejects_missing_or_unclosed_fence_and_empty_fields() {
        assert!(parse_skill_md("no frontmatter here").is_err());
        assert!(parse_skill_md("---\nname: foo\ndescription: d\n").is_err());
        assert!(parse_skill_md("---\nname: \"\"\ndescription: d\n---\n").is_err());
        assert!(parse_skill_md("---\nname: foo\n---\n").is_err());
    }

    #[test]
    fn discover_is_source_qualified_and_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        write_skill(&home.join(".claude/skills"), "review", "review", "d");
        write_skill(&home.join(".kimi-code/skills"), "review", "review", "d");
        write_skill(&app.join("skills"), "mine", "mine", "d");

        let found = discover(&home, &app);
        let ids: Vec<(String, String)> = found
            .iter()
            .map(|s| (s.provenance.label(), s.directory.clone()))
            .collect();
        // Sorted by provenance label then directory; the two "review" folders
        // coexist under different provenances (no shadow-merge).
        assert_eq!(
            ids,
            vec![
                ("agent-native:claude".to_string(), "review".to_string()),
                ("agent-native:kimi".to_string(), "review".to_string()),
                ("aoe-managed".to_string(), "mine".to_string()),
            ]
        );
    }

    #[test]
    fn discover_skips_malformed_without_failing_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        write_skill(&app.join("skills"), "good", "good", "d");
        let bad = app.join("skills").join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("SKILL.md"), "not frontmatter").unwrap();

        let found = discover(tmp.path(), &app);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].directory, "good");
    }

    #[test]
    fn create_then_read_round_trips_as_managed() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().to_path_buf();
        create_skill(&app, "my-skill", Some("use for testing")).unwrap();

        let read = read_skill(tmp.path(), &app, &SkillProvenance::AoeManaged, "my-skill").unwrap();
        assert_eq!(read.name, "my-skill");
        assert_eq!(read.description, "use for testing");
        assert!(read.content.contains("name: my-skill"));

        // Collision is refused.
        assert!(matches!(
            create_skill(&app, "my-skill", None),
            Err(SkillError::Collision(_))
        ));
    }

    #[test]
    fn create_rejects_unsafe_names() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["..", ".", "a/b", "has space", "", &"x".repeat(65)] {
            assert!(matches!(
                create_skill(tmp.path(), bad, None),
                Err(SkillError::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn edit_allows_name_diverging_from_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().to_path_buf();
        create_skill(&app, "s", None).unwrap();
        // The frontmatter name need not match the directory: identity is the
        // folder, so an edit that keeps a divergent name succeeds.
        let diverging = "---\nname: other\ndescription: d\n---\n\nbody\n";
        edit_skill(tmp.path(), &app, "s", diverging).unwrap();
        assert_eq!(
            read_skill(tmp.path(), &app, &SkillProvenance::AoeManaged, "s")
                .unwrap()
                .name,
            "other"
        );
        // Malformed content is still refused.
        assert!(matches!(
            edit_skill(tmp.path(), &app, "s", "not frontmatter"),
            Err(SkillError::InvalidInput(_))
        ));
    }

    #[test]
    fn adopt_with_diverging_name_stays_editable() {
        // A host skill whose frontmatter name differs from its directory (which
        // the folder-identity model allows), adopted into the managed store,
        // must remain editable while keeping that divergent name.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        write_skill(&home.join(".claude/skills"), "review", "Code Review", "d");

        adopt_skill(
            &home,
            &app,
            &SkillProvenance::AgentNative {
                agent: "claude".to_string(),
            },
            "review",
            None,
        )
        .unwrap();
        // The adopted copy keeps the source's divergent name, not the directory.
        assert_eq!(
            read_skill(&home, &app, &SkillProvenance::AoeManaged, "review")
                .unwrap()
                .name,
            "Code Review"
        );
        // Editing it while preserving that name succeeds (previously rejected).
        let edited = "---\nname: Code Review\ndescription: updated\n---\n\nnew body\n";
        edit_skill(&home, &app, "review", edited).unwrap();
        assert_eq!(
            read_skill(&home, &app, &SkillProvenance::AoeManaged, "review")
                .unwrap()
                .description,
            "updated"
        );
    }

    #[test]
    fn adopt_copies_and_leaves_original_then_edit_host_is_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        write_skill(&home.join(".claude/skills"), "review", "review", "d");

        let src = home.join(".claude/skills/review/SKILL.md");
        let before = std::fs::read_to_string(&src).unwrap();

        let dest = adopt_skill(
            &home,
            &app,
            &SkillProvenance::AgentNative {
                agent: "claude".to_string(),
            },
            "review",
            None,
        )
        .unwrap();
        assert_eq!(dest, "review");
        assert!(app.join("skills/review/SKILL.md").is_file());
        // Host original untouched.
        assert_eq!(std::fs::read_to_string(&src).unwrap(), before);

        // Editing the managed copy works; editing a host-only skill is FORBIDDEN.
        edit_skill(
            &home,
            &app,
            "review",
            "---\nname: review\ndescription: d2\n---\n\nb\n",
        )
        .unwrap();
        std::fs::remove_dir_all(app.join("skills/review")).unwrap();
        assert!(matches!(
            edit_skill(
                &home,
                &app,
                "review",
                "---\nname: review\ndescription: d\n---\n\nb\n"
            ),
            Err(SkillError::ReadOnly(_))
        ));
    }

    #[test]
    fn adopt_rejects_managed_source_and_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        write_skill(&home.join(".claude/skills"), "review", "review", "d");
        create_skill(&app, "review", None).unwrap();

        assert!(matches!(
            adopt_skill(&home, &app, &SkillProvenance::AoeManaged, "review", None),
            Err(SkillError::InvalidInput(_))
        ));
        assert!(matches!(
            adopt_skill(
                &home,
                &app,
                &SkillProvenance::AgentNative {
                    agent: "claude".to_string()
                },
                "review",
                None
            ),
            Err(SkillError::Collision(_))
        ));
    }

    #[test]
    fn propagate_lands_in_target_and_refuses_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        create_skill(&app, "shared", Some("d")).unwrap();

        propagate_skill(&home, &app, "shared", "kimi").unwrap();
        assert!(home.join(".kimi-code/skills/shared/SKILL.md").is_file());

        // No overwrite: a second propagate to the same target is a collision.
        assert!(matches!(
            propagate_skill(&home, &app, "shared", "kimi"),
            Err(SkillError::Collision(_))
        ));
        // Unknown agent is rejected.
        assert!(matches!(
            propagate_skill(&home, &app, "shared", "codex"),
            Err(SkillError::InvalidInput(_))
        ));
        // Missing managed source is NotFound.
        assert!(matches!(
            propagate_skill(&home, &app, "absent", "kimi"),
            Err(SkillError::NotFound(_))
        ));
    }

    #[test]
    fn delete_managed_and_refuses_host_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        create_skill(&app, "gone", None).unwrap();
        delete_skill(&home, &app, "gone").unwrap();
        assert!(!app.join("skills/gone").exists());

        write_skill(&home.join(".claude/skills"), "hostonly", "hostonly", "d");
        assert!(matches!(
            delete_skill(&home, &app, "hostonly"),
            Err(SkillError::ReadOnly(_))
        ));
        assert!(matches!(
            delete_skill(&home, &app, "nope"),
            Err(SkillError::NotFound(_))
        ));
    }

    #[test]
    fn create_rejects_oversized_scaffold() {
        let tmp = tempfile::tempdir().unwrap();
        let huge = "x".repeat((MAX_SKILL_MD_BYTES + 10) as usize);
        assert!(matches!(
            create_skill(tmp.path(), "big", Some(&huge)),
            Err(SkillError::InvalidInput(_))
        ));
        assert!(!tmp.path().join("skills/big").exists());
    }

    #[test]
    fn adopt_and_propagate_reject_oversized_source() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        // A host skill whose SKILL.md exceeds the cap must not be adoptable.
        let d = home.join(".claude/skills/big");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!(
                "---\nname: big\ndescription: {}\n---\n",
                "x".repeat((MAX_SKILL_MD_BYTES + 10) as usize)
            ),
        )
        .unwrap();
        assert!(matches!(
            adopt_skill(
                &home,
                &app,
                &SkillProvenance::AgentNative {
                    agent: "claude".to_string()
                },
                "big",
                None
            ),
            Err(SkillError::InvalidInput(_))
        ));
        assert!(!app.join("skills/big").exists());
    }

    #[cfg(unix)]
    #[test]
    fn store_ops_reject_symlinked_skill_directory() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");

        // An out-of-store dir holding a valid SKILL.md the attacker wants reached.
        let outside = tmp.path().join("outside");
        write_skill(&outside, "target", "target", "d");

        // Symlink `<app>/skills/evil` -> the outside/target dir.
        let managed = app.join("skills");
        std::fs::create_dir_all(&managed).unwrap();
        symlink(outside.join("target"), managed.join("evil")).unwrap();

        // edit must refuse to write through the symlinked managed dir.
        assert!(matches!(
            edit_skill(
                &home,
                &app,
                "evil",
                "---\nname: evil\ndescription: d\n---\n"
            ),
            Err(SkillError::InvalidInput(_))
        ));
        // The outside SKILL.md is untouched.
        assert_eq!(
            std::fs::read_to_string(outside.join("target/SKILL.md")).unwrap(),
            "---\nname: target\ndescription: d\n---\n\n# target\n\nbody\n"
        );

        // adopt must refuse a symlinked host source dir too.
        let host_skills = home.join(".claude/skills");
        std::fs::create_dir_all(&host_skills).unwrap();
        symlink(outside.join("target"), host_skills.join("evil")).unwrap();
        assert!(matches!(
            adopt_skill(
                &home,
                &app,
                &SkillProvenance::AgentNative {
                    agent: "claude".to_string()
                },
                "evil",
                None
            ),
            Err(SkillError::InvalidInput(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_rejects_symlinked_store_root() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");

        // A valid skill outside the store, reachable only if the store root is
        // trusted as a symlink.
        let outside = tmp.path().join("outside");
        write_skill(&outside, "target", "target", "d");

        // Make the managed store root itself a symlink pointing outside.
        std::fs::create_dir_all(&app).unwrap();
        symlink(&outside, app.join("skills")).unwrap();

        assert!(matches!(
            read_skill(&home, &app, &SkillProvenance::AoeManaged, "target"),
            Err(SkillError::InvalidInput(_))
        ));
    }
}

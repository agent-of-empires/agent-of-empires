//! Always-compiled model of skills discovered by AoE.
//!
//! A "skill" is a `SKILL.md` folder used by supported coding agents: YAML
//! frontmatter (`name`, `description`, plus optional metadata) between `---`
//! fences, then a markdown body, living in a per-skill directory. AoE has never
//! had a Rust model for these; they were only bulk-copied into sandboxes
//! (`src/session/container_config.rs`). This module is the single resolver used
//! by the server, CLI, and plugin host.
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

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;
use uuid::Uuid;

/// Reject a `SKILL.md` larger than this before parsing, so a pathological file
/// cannot make the host read an unbounded amount into memory.
pub const MAX_SKILL_MD_BYTES: u64 = 1024 * 1024;

const MAX_SKILL_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SKILL_PACKAGE_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SKILL_PACKAGE_FILES: usize = 1024;
const MAX_SKILL_PACKAGE_DEPTH: usize = 16;

/// A physical host directory from which AoE discovers skills. `consumers` names
/// every agent that can load the directory; it does not change skill identity.
/// `primary_agent` is the single agent this root is the canonical home for, and
/// is what [`sync_for_agent`] keys on: an agent that reads several roots
/// (opencode reads three) must still receive one copy, not one per readable
/// root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRoot {
    pub id: &'static str,
    pub label: &'static str,
    pub relative_path: &'static str,
    pub consumers: &'static [&'static str],
    pub primary_agent: &'static str,
    pub legacy: bool,
}

const SKILL_ROOTS: &[SkillRoot] = &[
    SkillRoot {
        id: "claude-user",
        label: "Claude",
        relative_path: ".claude/skills",
        consumers: &["claude", "opencode"],
        primary_agent: "claude",
        legacy: false,
    },
    SkillRoot {
        id: "agents-standard",
        label: "Agent Skills",
        relative_path: ".agents/skills",
        consumers: &["codex", "opencode"],
        primary_agent: "codex",
        legacy: false,
    },
    SkillRoot {
        id: "gemini-user",
        label: "Gemini",
        relative_path: ".gemini/skills",
        consumers: &["gemini"],
        primary_agent: "gemini",
        legacy: false,
    },
    SkillRoot {
        id: "opencode-user",
        label: "OpenCode",
        relative_path: ".config/opencode/skills",
        consumers: &["opencode"],
        primary_agent: "opencode",
        legacy: false,
    },
    SkillRoot {
        id: "kimi-legacy",
        label: "Kimi (legacy)",
        relative_path: ".kimi-code/skills",
        consumers: &["kimi"],
        primary_agent: "kimi",
        legacy: true,
    },
];

pub fn skill_roots() -> &'static [SkillRoot] {
    SKILL_ROOTS
}

pub fn skill_root(id: &str) -> Option<&'static SkillRoot> {
    SKILL_ROOTS.iter().find(|root| root.id == id)
}

/// The root an agent's managed skills are written to. `None` for an agent with
/// no known skills location, which is most of the registry.
pub fn primary_root_for_agent(agent: &str) -> Option<&'static SkillRoot> {
    SKILL_ROOTS.iter().find(|root| root.primary_agent == agent)
}

/// Where a skill was discovered. The read-only host layers carry a root key;
/// the single writable layer is [`SkillProvenance::AoeManaged`]. Serializes to a
/// tagged object (`{ "kind": "external", "root": "claude-user" }` /
/// `{ "kind": "aoe-managed" }`) so it round-trips as both `skills.list` output
/// and a source-qualified `skills.read` / `skills.adopt` parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SkillProvenance {
    External { root: String },
    AoeManaged,
}

impl SkillProvenance {
    /// The provenance string shown in logs, e.g. `external:claude-user`,
    /// `aoe-managed`.
    pub fn label(&self) -> String {
        match self {
            SkillProvenance::External { root } => format!("external:{root}"),
            SkillProvenance::AoeManaged => "aoe-managed".to_string(),
        }
    }

    /// Only the AoE-managed layer accepts writes; host-discovered skills are
    /// read-only and must be adopted before editing.
    pub fn is_writable(&self) -> bool {
        matches!(self, SkillProvenance::AoeManaged)
    }

    /// The short source name shown to a user, e.g. `AoE`, `Claude`, `Gemini`.
    /// Distinct from [`Self::label`], which is the stable machine-facing
    /// string. The web surfaces derive the same wording from the root registry
    /// they already fetch, so a skill reads the same in the TUI panel, the
    /// skills manager, the slash picker, and its tool card (#3052).
    pub fn source_label(&self) -> &'static str {
        match self {
            SkillProvenance::AoeManaged => "AoE",
            SkillProvenance::External { root } => skill_root(root)
                .map(|entry| entry.label)
                // An unknown root cannot be labelled, but it is still a real
                // directory on disk, so say so rather than showing nothing.
                .unwrap_or("Unknown"),
        }
    }
}

/// One discovered skill's list-safe metadata: its identity (`directory`), its
/// frontmatter `name`/`description`, and where it came from. The body is not
/// included; `skills.read` returns that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSkill {
    pub provenance: SkillProvenance,
    pub directory: String,
    pub name: String,
    pub description: String,
}

/// A skill read in full: its metadata plus the raw `SKILL.md` content.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
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

/// Discover every skill across all host-discovered roots and the managed
/// store, source-qualified and sorted deterministically (by provenance label,
/// then directory). A malformed or unreadable skill warns and is skipped; it
/// never fails the whole scan. Roots are injected so tests need no real `$HOME`.
pub fn discover(home: &Path, app_dir: &Path) -> Vec<DiscoveredSkill> {
    let mut out = Vec::new();
    for root in SKILL_ROOTS {
        collect_from_dir(
            &home.join(root.relative_path),
            &SkillProvenance::External {
                root: root.id.to_string(),
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
///
/// A host directory carrying a valid [`PROPAGATION_MARKER`] for this root is a
/// copy AoE propagated from the managed store, so it is skipped too: it is the
/// same logical skill as the managed entry and listing both would double-count
/// it. A marker that is malformed, or bound to another root or directory, does
/// not count; that directory is listed as an ordinary host skill rather than
/// being hidden on unverified metadata.
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
        if let SkillProvenance::External { root: root_id } = provenance {
            if marker_at(&entry.path(), root_id, &name).is_some() {
                continue;
            }
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
        rename_staging(&staging, &final_path, directory)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
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
    let managed_root = app_dir.join("skills");
    let managed_path = match resolve_skill_dir(&managed_root, directory) {
        Ok(path) => path,
        Err(SkillError::NotFound(_)) => return Err(absent_write_target(home, directory)),
        Err(error) => return Err(error),
    };
    validate_skill_md_at(&managed_path, directory)?;
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
    let root = match source {
        SkillProvenance::External { root } => root,
        SkillProvenance::AoeManaged => {
            return Err(SkillError::InvalidInput(
                "cannot adopt an already AoE-managed skill".to_string(),
            ))
        }
    };
    let src_dir = resolve_skill_dir(&external_skill_dir(home, root)?, directory)?;
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
        .map_err(SkillError::Io)
        .and_then(|()| {
            // Adopting a copy AoE propagated produces an ordinary managed
            // skill, not one carrying a deployment marker. Leaving the marker in
            // the store would let the source claim ownership of a host
            // directory it never deployed.
            std::fs::remove_file(staging.join(PROPAGATION_MARKER)).or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            })?;
            rename_staging(&staging, &final_path, dest_name)
        });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result?;
    Ok(dest_name.to_string())
}

/// Marker AoE writes inside every propagated copy, naming the root and skill it
/// was deployed as and the digest of the package at deploy time. It is the only
/// thing that authorizes AoE to later replace or delete that directory, so it is
/// a reserved filename the managed store may never contain: a source package
/// carrying a hand-written marker could otherwise forge ownership of a host
/// directory it does not own.
pub const PROPAGATION_MARKER: &str = ".aoe-managed.json";

const MARKER_VERSION: u32 = 1;

/// Domain-separation header for [`package_digest`]. Bump when the hashed fields
/// change so an old digest cannot silently compare equal under new rules.
const PACKAGE_DIGEST_PREFIX: &[u8] = b"aoe-skill-package-v1\0";

#[derive(Debug, Serialize, Deserialize)]
struct PropagationMarker {
    version: u32,
    root: String,
    directory: String,
    digest: String,
}

/// What a sync did, or refused to do, to one skill under one root. A sync never
/// aborts on the first conflict, so a caller gets one of these per skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncOutcome {
    pub root: String,
    pub directory: String,
    pub status: SyncStatus,
    /// Why, for the statuses that need a reason. `None` otherwise.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncStatus {
    Created,
    Updated,
    Unchanged,
    Removed,
    /// The destination exists and is not ours to touch. Never an error: the
    /// user's file is intact, which is the point.
    Conflict,
    Error,
}

/// How a destination directory relates to AoE.
#[derive(Debug, PartialEq, Eq)]
enum Ownership {
    Absent,
    /// Ours, and byte-identical to what AoE deployed. Safe to replace or remove.
    Clean {
        digest: String,
    },
    /// Ours by marker, but the package changed since AoE wrote it. The user
    /// edited a propagated copy; preserve it.
    Drifted,
    /// Not ours: no marker, an unreadable or unsupported one, or one bound to a
    /// different root or directory. Never touch it.
    Foreign,
}

/// Deterministic `sha256:<hex>` over a skill package, excluding the propagation
/// marker so a deployed copy hashes equal to the source it came from.
///
/// Deliberately not [`crate::plugin::integrity::tree_hash`]: that is a plugin
/// primitive with its own domain prefix and exclusions, and it buffers whole
/// files, which would defeat the package byte limits skills enforce. This
/// streams and honours [`COPY_LIMITS`], matching what [`copy_tree_no_symlinks`]
/// would accept.
fn package_digest(dir: &Path) -> Result<String, SkillError> {
    let mut entries = Vec::new();
    collect_digest_entries(dir, dir, 0, &mut entries)?;
    entries.sort();

    let mut hasher = Sha256::new();
    hasher.update(PACKAGE_DIGEST_PREFIX);
    let mut budget = CopyBudget::default();
    for (rel, path) in &entries {
        budget.files += 1;
        if budget.files > COPY_LIMITS.files {
            return Err(SkillError::InvalidInput(
                "skill package exceeds the maximum file count".to_string(),
            ));
        }
        let remaining = COPY_LIMITS.total_bytes.saturating_sub(budget.bytes);
        let allowed = remaining.min(COPY_LIMITS.file_bytes);
        hasher.update(b"file\0");
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");

        let file = std::fs::File::open(path)?;
        let mut reader = file.take(allowed + 1);
        let mut buf = [0u8; 64 * 1024];
        let mut len: u64 = 0;
        loop {
            let read = reader.read(&mut buf)?;
            if read == 0 {
                break;
            }
            len += read as u64;
            if len > allowed {
                return Err(SkillError::InvalidInput(
                    "skill package exceeds a file or total byte limit".to_string(),
                ));
            }
            hasher.update(&buf[..read]);
        }
        hasher.update(len.to_le_bytes());
        budget.bytes += len;
    }

    let digest = hasher.finalize();
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Gather `(forward-slash relative path, absolute path)` for every file under
/// `dir`, skipping the marker at the package root. A symlink or special file is
/// an error, not a silent skip, so nothing that would be copied escapes the
/// digest.
fn collect_digest_entries(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), SkillError> {
    if depth > COPY_LIMITS.depth {
        return Err(SkillError::InvalidInput(
            "skill package exceeds the maximum directory depth".to_string(),
        ));
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if dir == root && entry.file_name() == PROPAGATION_MARKER {
            continue;
        }
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_symlink() {
            return Err(SkillError::InvalidInput(format!(
                "skill package contains a symlink ({})",
                path.display()
            )));
        }
        if file_type.is_dir() {
            collect_digest_entries(root, &path, depth + 1, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("entry path is under root")
                .to_str()
                .ok_or_else(|| {
                    SkillError::InvalidInput(format!(
                        "skill package has a non-UTF-8 path ({})",
                        path.display()
                    ))
                })?
                .replace('\\', "/");
            out.push((rel, path));
        } else {
            return Err(SkillError::InvalidInput(format!(
                "skill package contains a special file ({})",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Read `dest`'s marker and confirm it binds to exactly this root and directory.
/// A marker that is missing, unparseable, of an unsupported version, or copied
/// in from somewhere else is not ours.
fn marker_at(dest: &Path, root_id: &str, directory: &str) -> Option<PropagationMarker> {
    let path = dest.join(PROPAGATION_MARKER);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_file() => {}
        _ => return None,
    }
    let raw = read_file_capped(&path, MAX_SKILL_MD_BYTES).ok()?;
    let marker: PropagationMarker = serde_json::from_str(&raw).ok()?;
    if marker.version != MARKER_VERSION || marker.root != root_id || marker.directory != directory {
        return None;
    }
    Some(marker)
}

/// Classify a propagation destination. Anything AoE cannot positively prove it
/// deployed, unchanged, is [`Ownership::Foreign`] or [`Ownership::Drifted`] and
/// is left alone.
fn ownership(dest: &Path, root_id: &str, directory: &str) -> Ownership {
    match std::fs::symlink_metadata(dest) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => return Ownership::Foreign,
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ownership::Absent,
        Err(_) => return Ownership::Foreign,
    }
    let Some(marker) = marker_at(dest, root_id, directory) else {
        return Ownership::Foreign;
    };
    match package_digest(dest) {
        Ok(digest) if digest == marker.digest => Ownership::Clean { digest },
        // Unreadable or over-limit reads as drift: refuse to destroy what we
        // cannot verify.
        Ok(_) | Err(_) => Ownership::Drifted,
    }
}

/// Reconcile every AoE-managed skill into `target_dir`, attributing ownership to
/// `root_id`. `target_dir` is a skills root: a host one (`~/.claude/skills`) or
/// a sandbox's, so one reconciler serves both.
///
/// Creates what is missing, replaces what AoE deployed and has since changed at
/// the source, removes what AoE deployed and no longer has a source, and
/// preserves everything else. A per-skill failure is reported and does not stop
/// the rest.
///
/// `replace` names the skills the user has explicitly asked AoE to take over, so
/// a destination AoE does not own is overwritten instead of reported. Nothing
/// else grants that: a caller that passes an empty set cannot destroy user data,
/// which is why every automatic path does exactly that.
pub fn sync_skills_into(
    target_dir: &Path,
    app_dir: &Path,
    root_id: &str,
    options: &SyncOptions,
) -> Vec<SyncOutcome> {
    let managed_root = app_dir.join("skills");
    recover_abandoned(target_dir, ABANDONED_AFTER);
    let scoped = !options.only.is_empty();
    let mut out = Vec::new();
    let mut managed = Vec::new();
    for skill in list_managed(&managed_root) {
        managed.push(skill.clone());
        if scoped && !options.only.contains(&skill) {
            continue;
        }
        out.push(sync_one(
            &managed_root,
            target_dir,
            root_id,
            &skill,
            options.replace.contains(&skill),
        ));
    }
    // A scoped sync must not withdraw another skill's copies: the user asked
    // about one skill, so the rest of the root is none of its business.
    if !scoped {
        out.extend(remove_orphans(target_dir, root_id, &managed));
    }
    out.sort_by(|a, b| a.directory.cmp(&b.directory));
    out
}

/// What a sync is allowed to do beyond the default of "add what is missing and
/// keep what AoE already owns current".
///
/// The default is the safe one, which is what every automatic caller uses:
/// overwrite nothing it does not own, and consider every managed skill.
#[derive(Debug, Default, Clone)]
pub struct SyncOptions {
    /// Skills the user has explicitly asked AoE to take over, so a destination
    /// AoE does not own is overwritten instead of reported. Nothing else grants
    /// that, which is why an automatic path leaving this empty cannot destroy
    /// user data.
    pub replace: HashSet<String>,
    /// When non-empty, reconcile only these skills and leave every other one
    /// alone, including its orphans. This is what makes sharing a single skill
    /// a single-skill operation on disk rather than a full sync whose report is
    /// filtered afterwards.
    pub only: HashSet<String>,
}

impl SyncOptions {
    /// Reconcile one skill and nothing else.
    pub fn only(directory: &str) -> Self {
        Self {
            only: HashSet::from([directory.to_string()]),
            ..Self::default()
        }
    }
}

/// Managed skill directory names that would actually be discovered: a real
/// directory holding a parseable `SKILL.md`.
fn list_managed(managed_root: &Path) -> Vec<String> {
    let mut discovered = Vec::new();
    collect_from_dir(managed_root, &SkillProvenance::AoeManaged, &mut discovered);
    discovered.into_iter().map(|s| s.directory).collect()
}

fn sync_one(
    managed_root: &Path,
    target_dir: &Path,
    root_id: &str,
    directory: &str,
    replace: bool,
) -> SyncOutcome {
    let outcome = |status, message: Option<String>| SyncOutcome {
        root: root_id.to_string(),
        directory: directory.to_string(),
        status,
        message,
    };
    let src = match resolve_skill_dir(managed_root, directory) {
        Ok(path) => path,
        Err(e) => return outcome(SyncStatus::Error, Some(describe(e))),
    };
    let src_digest = match package_digest(&src) {
        Ok(d) => d,
        Err(e) => return outcome(SyncStatus::Error, Some(describe(e))),
    };
    let dest = target_dir.join(directory);
    let owned = ownership(&dest, root_id, directory);
    // The user asked for this one by name, so take it over. Installing renames
    // the existing entry aside first, which for a symlink moves the link and
    // leaves whatever it pointed at intact.
    if replace && matches!(owned, Ownership::Foreign | Ownership::Drifted) {
        return match install(&src, &dest, target_dir, root_id, directory, &src_digest) {
            Ok(()) => outcome(SyncStatus::Updated, Some("replaced on request".to_string())),
            Err(e) => outcome(SyncStatus::Error, Some(describe(e))),
        };
    }
    match owned {
        Ownership::Foreign => outcome(
            SyncStatus::Conflict,
            Some("a skill AoE does not manage already exists here".to_string()),
        ),
        Ownership::Drifted => outcome(
            SyncStatus::Conflict,
            Some("the propagated copy was edited in place; preserving it".to_string()),
        ),
        Ownership::Clean { digest } if digest == src_digest => outcome(SyncStatus::Unchanged, None),
        Ownership::Clean { .. } => {
            match install(&src, &dest, target_dir, root_id, directory, &src_digest) {
                Ok(()) => outcome(SyncStatus::Updated, None),
                Err(e) => outcome(SyncStatus::Error, Some(describe(e))),
            }
        }
        Ownership::Absent => {
            match install(&src, &dest, target_dir, root_id, directory, &src_digest) {
                Ok(()) => outcome(SyncStatus::Created, None),
                Err(e) => outcome(SyncStatus::Error, Some(describe(e))),
            }
        }
    }
}

/// Stage a copy of `src` beside `dest`, stamp the marker, then swap it in. An
/// existing `dest` is moved aside first and restored if the swap fails, so a
/// failure mid-update never leaves the root without the skill.
fn install(
    src: &Path,
    dest: &Path,
    target_dir: &Path,
    root_id: &str,
    directory: &str,
    digest: &str,
) -> Result<(), SkillError> {
    validate_skill_md_at(src, directory)?;
    std::fs::create_dir_all(target_dir)?;
    let staging = new_staging_dir(target_dir)?;
    let build = (|| -> Result<(), SkillError> {
        copy_tree_no_symlinks(src, &staging).map_err(SkillError::Io)?;
        let marker = PropagationMarker {
            version: MARKER_VERSION,
            root: root_id.to_string(),
            directory: directory.to_string(),
            digest: digest.to_string(),
        };
        let encoded = serde_json::to_string_pretty(&marker)
            .map_err(|e| SkillError::Io(anyhow::Error::new(e)))?;
        std::fs::write(staging.join(PROPAGATION_MARKER), encoded)?;
        Ok(())
    })();
    if let Err(e) = build {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    let backup = dest
        .exists()
        .then(|| target_dir.join(format!("{BACKUP_PREFIX}{}.{directory}", Uuid::new_v4())));
    if let Some(backup) = &backup {
        if let Err(e) = std::fs::rename(dest, backup) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e.into());
        }
    }
    match std::fs::rename(&staging, dest) {
        Ok(()) => {
            if let Some(backup) = &backup {
                let _ = std::fs::remove_dir_all(backup);
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            if let Some(backup) = &backup {
                // Put the user's directory back before surfacing the failure.
                let _ = std::fs::rename(backup, dest);
            }
            Err(e.into())
        }
    }
}

/// Drop copies AoE deployed whose managed source is gone. A drifted copy is kept
/// and reported: the user edited it, so deleting it would destroy their work.
fn remove_orphans(target_dir: &Path, root_id: &str, managed: &[String]) -> Vec<SyncOutcome> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(target_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || managed.contains(&name) {
            continue;
        }
        let outcome = |status, message: Option<String>| SyncOutcome {
            root: root_id.to_string(),
            directory: name.clone(),
            status,
            message,
        };
        match ownership(&entry.path(), root_id, &name) {
            Ownership::Clean { .. } => match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => out.push(outcome(SyncStatus::Removed, None)),
                Err(e) => out.push(outcome(SyncStatus::Error, Some(e.to_string()))),
            },
            Ownership::Drifted => out.push(outcome(
                SyncStatus::Conflict,
                Some(
                    "its managed source is gone but the copy was edited; preserving it".to_string(),
                ),
            )),
            // Not ours, so not an orphan. Silent: every hand-written skill in
            // the root would otherwise be reported on every sync.
            Ownership::Foreign | Ownership::Absent => {}
        }
    }
    out
}

fn describe(error: SkillError) -> String {
    match error {
        SkillError::InvalidInput(m)
        | SkillError::NotFound(m)
        | SkillError::Collision(m)
        | SkillError::ReadOnly(m) => m,
        SkillError::Io(e) => e.to_string(),
    }
}

/// Log what a background reconcile did. Conflicts warn (the user has a skill
/// AoE could not place, which they may want to resolve); routine work is debug,
/// so an opted-in launch does not narrate itself on every session.
pub fn log_sync_outcomes(context: &str, outcomes: &[SyncOutcome]) {
    for outcome in outcomes {
        match outcome.status {
            SyncStatus::Conflict | SyncStatus::Error => {
                warn!(
                    target: "session.skills",
                    context,
                    root = %outcome.root,
                    directory = %outcome.directory,
                    status = ?outcome.status,
                    message = outcome.message.as_deref().unwrap_or(""),
                    "skill not propagated"
                );
            }
            SyncStatus::Unchanged => {}
            _ => {
                tracing::debug!(
                    target: "session.skills",
                    context,
                    root = %outcome.root,
                    directory = %outcome.directory,
                    status = ?outcome.status,
                    "propagated skill"
                );
            }
        }
    }
}

/// Reconcile the managed store into one host root.
pub fn sync_root(
    home: &Path,
    app_dir: &Path,
    root_id: &str,
    options: &SyncOptions,
) -> Result<Vec<SyncOutcome>, SkillError> {
    let target = external_skill_dir(home, root_id)?;
    Ok(sync_skills_into(&target, app_dir, root_id, options))
}

/// Reconcile the managed store into every host root, so a skill authored once
/// is present for every agent AoE knows a skills location for.
pub fn sync_all_roots(home: &Path, app_dir: &Path, options: &SyncOptions) -> Vec<SyncOutcome> {
    SKILL_ROOTS
        .iter()
        .flat_map(|root| {
            sync_skills_into(&home.join(root.relative_path), app_dir, root.id, options)
        })
        .collect()
}

/// Reconcile the managed store into the one root `agent` is the primary
/// consumer of. `None` when AoE knows no skills location for that agent, which
/// is most of the agent registry.
pub fn sync_for_agent(home: &Path, app_dir: &Path, agent: &str) -> Option<Vec<SyncOutcome>> {
    let root = primary_root_for_agent(agent)?;
    // Never replaces: a session launching must not overwrite a skill the user
    // wrote by hand, no matter what the managed store contains.
    Some(sync_skills_into(
        &home.join(root.relative_path),
        app_dir,
        root.id,
        &SyncOptions::default(),
    ))
}

fn external_skill_dir(home: &Path, root: &str) -> Result<PathBuf, SkillError> {
    skill_root(root)
        .map(|entry| home.join(entry.relative_path))
        .ok_or_else(|| SkillError::InvalidInput(format!("unsupported skills root {root:?}")))
}

/// The designated root that a source-qualified skill's directory must live
/// under (the external host root, or the managed store).
fn skill_root_for(
    home: &Path,
    app_dir: &Path,
    provenance: &SkillProvenance,
) -> Result<PathBuf, SkillError> {
    match provenance {
        SkillProvenance::External { root } => external_skill_dir(home, root),
        SkillProvenance::AoeManaged => Ok(app_dir.join("skills")),
    }
}

/// Read a file as UTF-8, refusing more than `max` bytes. Reads through one
/// handle and rejects an overflow byte, so a file that grows after a metadata
/// check cannot slip past the bound (the metadata-then-read TOCTOU).
pub fn read_file_capped(path: &Path, max: u64) -> Result<String> {
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
    for root in SKILL_ROOTS {
        if home
            .join(root.relative_path)
            .join(directory)
            .join("SKILL.md")
            .is_file()
        {
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
    let path = parent.join(format!("{STAGING_PREFIX}{}", Uuid::new_v4()));
    std::fs::create_dir(&path)?;
    Ok(path)
}

/// Staging holds a copy of the source, so it is always reproducible and safe to
/// delete. A backup holds the directory that was in the destination, which for
/// a replaced skill is the user's own content and the only copy of it while the
/// swap is in flight; the two are named apart so recovery can tell them apart.
const STAGING_PREFIX: &str = ".tmp-stage-";
const BACKUP_PREFIX: &str = ".tmp-backup-";

/// How old a leftover has to be before it is treated as abandoned rather than
/// as another process's work in progress. A live swap lasts milliseconds.
const ABANDONED_AFTER: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Clean up after a process that died mid-swap.
///
/// A crash between staging and the final rename leaves a staging directory
/// behind; a crash between moving the destination aside and renaming staging
/// into place leaves a backup holding the only copy of what used to be there.
/// The first is litter. The second is the user's data sitting under a dot-name
/// nothing ever reads, so it is restored rather than swept.
///
/// Only leftovers older than `abandoned_after` are touched, so a reconcile
/// running concurrently in another process keeps its in-flight directories.
fn recover_abandoned(target_dir: &Path, abandoned_after: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(target_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_stage = name.starts_with(STAGING_PREFIX);
        let is_backup = name.starts_with(BACKUP_PREFIX);
        if !is_stage && !is_backup {
            continue;
        }
        let recent = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
            .map(|age| age < abandoned_after)
            .unwrap_or(true);
        if recent {
            continue;
        }
        if is_stage {
            let _ = std::fs::remove_dir_all(entry.path());
            continue;
        }
        // `.tmp-backup-<uuid>.<directory>`: recover the name it was moved from.
        // Split on '.', not '-': a uuid is full of hyphens and a directory name
        // may contain them too, while neither may contain a dot.
        let Some(directory) = name
            .strip_prefix(BACKUP_PREFIX)
            .and_then(|rest| rest.split_once('.'))
            .map(|(_uuid, directory)| directory)
        else {
            continue;
        };
        let dest = target_dir.join(directory);
        if dest.exists() {
            // The swap landed and only the cleanup was lost.
            let _ = std::fs::remove_dir_all(entry.path());
        } else if std::fs::rename(entry.path(), &dest).is_ok() {
            warn!(
                target: "session.skills",
                directory,
                root = %target_dir.display(),
                "restored a skill left behind by an interrupted sync"
            );
        }
    }
}

#[derive(Clone, Copy)]
struct CopyLimits {
    total_bytes: u64,
    file_bytes: u64,
    files: usize,
    depth: usize,
}

const COPY_LIMITS: CopyLimits = CopyLimits {
    total_bytes: MAX_SKILL_PACKAGE_BYTES,
    file_bytes: MAX_SKILL_PACKAGE_FILE_BYTES,
    files: MAX_SKILL_PACKAGE_FILES,
    depth: MAX_SKILL_PACKAGE_DEPTH,
};

#[derive(Default)]
struct CopyBudget {
    bytes: u64,
    files: usize,
}

/// Recursively copy `src` into `dst`, refusing links, special files, and
/// packages large enough to exhaust disk or memory through an adoption request.
fn copy_tree_no_symlinks(src: &Path, dst: &Path) -> Result<(), anyhow::Error> {
    copy_tree_bounded(src, dst, 0, &mut CopyBudget::default(), COPY_LIMITS)
}

fn copy_tree_bounded(
    src: &Path,
    dst: &Path,
    depth: usize,
    budget: &mut CopyBudget,
    limits: CopyLimits,
) -> Result<(), anyhow::Error> {
    if depth > limits.depth {
        bail!("skill package exceeds the maximum directory depth");
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_symlink() {
            bail!("refusing to copy symlink {}", from.display());
        } else if ft.is_dir() {
            copy_tree_bounded(&from, &to, depth + 1, budget, limits)?;
        } else if ft.is_file() {
            budget.files += 1;
            if budget.files > limits.files {
                bail!("skill package exceeds the maximum file count");
            }
            let remaining = limits.total_bytes.saturating_sub(budget.bytes);
            let allowed = remaining.min(limits.file_bytes);
            let input = std::fs::File::open(&from)?;
            let mut output = std::fs::File::create(&to)?;
            let copied = std::io::copy(&mut input.take(allowed + 1), &mut output)?;
            if copied > allowed {
                bail!("skill package exceeds a file or total byte limit");
            }
            output.flush()?;
            std::fs::set_permissions(&to, entry.metadata()?.permissions())?;
            budget.bytes += copied;
        } else {
            bail!("refusing to copy special file {}", from.display());
        }
    }
    Ok(())
}

fn rename_staging(staging: &Path, destination: &Path, collision: &str) -> Result<(), SkillError> {
    match std::fs::rename(staging, destination) {
        Ok(()) => Ok(()),
        Err(_) if std::fs::symlink_metadata(destination).is_ok() => {
            Err(SkillError::Collision(collision.to_string()))
        }
        Err(error) => Err(error.into()),
    }
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
        write_skill(&home.join(".agents/skills"), "standard", "standard", "d");
        write_skill(&home.join(".gemini/skills"), "gem", "gem", "d");
        write_skill(&home.join(".config/opencode/skills"), "open", "open", "d");
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
                ("aoe-managed".to_string(), "mine".to_string()),
                (
                    "external:agents-standard".to_string(),
                    "standard".to_string()
                ),
                ("external:claude-user".to_string(), "review".to_string()),
                ("external:gemini-user".to_string(), "gem".to_string()),
                ("external:kimi-legacy".to_string(), "review".to_string()),
                ("external:opencode-user".to_string(), "open".to_string()),
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
            &SkillProvenance::External {
                root: "claude-user".to_string(),
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
            &SkillProvenance::External {
                root: "claude-user".to_string(),
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
                &SkillProvenance::External {
                    root: "claude-user".to_string()
                },
                "review",
                None
            ),
            Err(SkillError::Collision(_))
        ));
    }

    fn status_of(outcomes: &[SyncOutcome], directory: &str) -> SyncStatus {
        outcomes
            .iter()
            .find(|o| o.directory == directory)
            .unwrap_or_else(|| panic!("no outcome for {directory:?} in {outcomes:?}"))
            .status
    }

    #[test]
    fn sync_creates_updates_and_leaves_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        create_skill(&app, "shared", Some("d")).unwrap();
        let target = home.join(".kimi-code/skills");

        let first = sync_skills_into(&target, &app, "kimi-legacy", &SyncOptions::default());
        assert_eq!(status_of(&first, "shared"), SyncStatus::Created);
        assert!(target.join("shared/SKILL.md").is_file());
        assert!(target.join("shared").join(PROPAGATION_MARKER).is_file());

        // Idempotent: nothing changed at the source, so nothing is rewritten.
        let second = sync_skills_into(&target, &app, "kimi-legacy", &SyncOptions::default());
        assert_eq!(status_of(&second, "shared"), SyncStatus::Unchanged);

        // Source edited: the clean copy is replaced.
        edit_skill(
            &home,
            &app,
            "shared",
            "---\nname: shared\ndescription: d2\n---\n\nnew body\n",
        )
        .unwrap();
        let third = sync_skills_into(&target, &app, "kimi-legacy", &SyncOptions::default());
        assert_eq!(status_of(&third, "shared"), SyncStatus::Updated);
        assert!(std::fs::read_to_string(target.join("shared/SKILL.md"))
            .unwrap()
            .contains("new body"));
    }

    #[test]
    fn sync_never_touches_what_aoe_did_not_deploy() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        let target = home.join(".claude/skills");
        create_skill(&app, "review", Some("d")).unwrap();

        // A hand-written host skill of the same name: preserved, reported.
        write_skill(&target, "review", "review", "the user's own");
        let out = sync_skills_into(&target, &app, "claude-user", &SyncOptions::default());
        assert_eq!(status_of(&out, "review"), SyncStatus::Conflict);
        assert!(std::fs::read_to_string(target.join("review/SKILL.md"))
            .unwrap()
            .contains("the user's own"));

        // A marker bound to a different root does not grant ownership here.
        std::fs::write(
            target.join("review").join(PROPAGATION_MARKER),
            r#"{"version":1,"root":"gemini-user","directory":"review","digest":"sha256:x"}"#,
        )
        .unwrap();
        assert_eq!(
            status_of(
                &sync_skills_into(&target, &app, "claude-user", &SyncOptions::default()),
                "review"
            ),
            SyncStatus::Conflict
        );

        // So does a malformed one.
        std::fs::write(target.join("review").join(PROPAGATION_MARKER), "not json").unwrap();
        assert_eq!(
            status_of(
                &sync_skills_into(&target, &app, "claude-user", &SyncOptions::default()),
                "review"
            ),
            SyncStatus::Conflict
        );
    }

    #[test]
    fn sync_preserves_a_propagated_copy_the_user_edited() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        let target = home.join(".claude/skills");
        create_skill(&app, "review", Some("d")).unwrap();
        sync_skills_into(&target, &app, "claude-user", &SyncOptions::default());

        // The user edits the propagated copy in place. AoE deployed it, but its
        // content is no longer what AoE deployed, so it is theirs now.
        std::fs::write(
            target.join("review/SKILL.md"),
            "---\nname: review\ndescription: d\n---\n\nmy edits\n",
        )
        .unwrap();

        // Source changes: still not overwritten.
        edit_skill(
            &home,
            &app,
            "review",
            "---\nname: review\ndescription: d3\n---\n\nupstream\n",
        )
        .unwrap();
        assert_eq!(
            status_of(
                &sync_skills_into(&target, &app, "claude-user", &SyncOptions::default()),
                "review"
            ),
            SyncStatus::Conflict
        );
        assert!(std::fs::read_to_string(target.join("review/SKILL.md"))
            .unwrap()
            .contains("my edits"));

        // Source deleted: still not deleted, because the edits would go with it.
        delete_skill(&home, &app, "review").unwrap();
        assert_eq!(
            status_of(
                &sync_skills_into(&target, &app, "claude-user", &SyncOptions::default()),
                "review"
            ),
            SyncStatus::Conflict
        );
        assert!(target.join("review/SKILL.md").is_file());
    }

    #[test]
    fn sync_leaves_a_symlinked_skill_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        let target = home.join(".claude/skills");
        create_skill(&app, "shared", Some("d")).unwrap();

        // Another manager (skillshare and friends) links its own store into the
        // agent's dir. AoE must not follow or replace that link.
        let other_store = tmp.path().join("other/shared");
        write_skill(
            &tmp.path().join("other"),
            "shared",
            "shared",
            "someone else's",
        );
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&other_store, target.join("shared")).unwrap();

        let out = sync_skills_into(&target, &app, "claude-user", &SyncOptions::default());
        assert_eq!(status_of(&out, "shared"), SyncStatus::Conflict);
        assert!(std::fs::symlink_metadata(target.join("shared"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(other_store.join("SKILL.md"))
            .unwrap()
            .contains("someone else's"));
    }

    /// Naming a skill takes it over, which is the only way past the
    /// never-overwrite rule. The automatic paths cannot reach it.
    #[test]
    fn replace_takes_over_only_what_the_user_named() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        let target = home.join(".claude/skills");
        for dir in ["taken", "spared"] {
            create_skill(&app, dir, Some("managed")).unwrap();
            write_skill(&target, dir, dir, "the user's own");
        }
        let replace = SyncOptions {
            replace: HashSet::from(["taken".to_string()]),
            ..Default::default()
        };

        let out = sync_skills_into(&target, &app, "claude-user", &replace);
        assert_eq!(status_of(&out, "taken"), SyncStatus::Updated);
        assert!(target.join("taken").join(PROPAGATION_MARKER).is_file());
        // Now AoE-owned, so ordinary syncs keep it current from here on.
        assert_eq!(
            status_of(
                &sync_skills_into(&target, &app, "claude-user", &SyncOptions::default()),
                "taken"
            ),
            SyncStatus::Unchanged
        );

        // Not named, so untouched even in the same run.
        assert_eq!(status_of(&out, "spared"), SyncStatus::Conflict);
        assert!(std::fs::read_to_string(target.join("spared/SKILL.md"))
            .unwrap()
            .contains("the user's own"));

        // A launching session passes no replacements, so it cannot take over
        // the skill the user still owns.
        sync_for_agent(&home, &app, "claude").unwrap();
        assert!(std::fs::read_to_string(target.join("spared/SKILL.md"))
            .unwrap()
            .contains("the user's own"));
    }

    /// Replacing a symlinked entry moves the link aside, so a skill managed by
    /// another tool keeps its store even when AoE takes over the name.
    #[test]
    fn replace_moves_a_symlink_without_touching_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        let target = home.join(".claude/skills");
        create_skill(&app, "shared", Some("managed")).unwrap();
        write_skill(
            &tmp.path().join("other"),
            "shared",
            "shared",
            "someone else's",
        );
        let other_store = tmp.path().join("other/shared");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&other_store, target.join("shared")).unwrap();

        let replace = SyncOptions {
            replace: HashSet::from(["shared".to_string()]),
            ..Default::default()
        };
        assert_eq!(
            status_of(
                &sync_skills_into(&target, &app, "claude-user", &replace),
                "shared"
            ),
            SyncStatus::Updated
        );
        assert!(!std::fs::symlink_metadata(target.join("shared"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(other_store.join("SKILL.md"))
            .unwrap()
            .contains("someone else's"));
    }

    /// A process killed mid-swap leaves either a staging copy (litter) or a
    /// backup holding the only copy of what used to be in the destination. The
    /// first is swept, the second is put back, and neither happens to a
    /// leftover young enough to belong to a sync still running elsewhere.
    #[test]
    fn abandoned_leftovers_are_swept_or_restored() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("skills");
        std::fs::create_dir_all(&target).unwrap();
        let stage = format!("{STAGING_PREFIX}{}", Uuid::new_v4());
        let backup = format!("{BACKUP_PREFIX}{}.review", Uuid::new_v4());
        write_skill(&target, &stage, "x", "d");
        write_skill(&target, &backup, "review", "the user's own");

        // A live swap lasts milliseconds, so nothing is old enough to touch.
        recover_abandoned(&target, std::time::Duration::from_secs(60 * 60));
        assert!(
            target.join(&stage).exists(),
            "a recent leftover belongs to a live sync"
        );
        assert!(target.join(&backup).exists());

        // Treat everything as abandoned.
        recover_abandoned(&target, std::time::Duration::ZERO);
        assert!(
            !target.join(&stage).exists(),
            "an abandoned staging copy is reproducible litter"
        );
        assert!(
            !target.join(&backup).exists(),
            "the backup is moved, not left behind"
        );
        assert!(
            std::fs::read_to_string(target.join("review/SKILL.md"))
                .unwrap()
                .contains("the user's own"),
            "the backup held the only copy, so it is restored under its own name"
        );
    }

    /// Sharing one skill must be a one-skill operation on disk, not a full sync
    /// whose report is filtered afterwards.
    #[test]
    fn a_scoped_sync_touches_only_the_named_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        let target = home.join(".claude/skills");
        create_skill(&app, "wanted", Some("d")).unwrap();
        create_skill(&app, "other", Some("d")).unwrap();
        // An orphan of a different skill, which a full sync would withdraw.
        sync_skills_into(&target, &app, "claude-user", &SyncOptions::default());
        delete_skill(&home, &app, "other").unwrap();

        let out = sync_skills_into(&target, &app, "claude-user", &SyncOptions::only("wanted"));
        assert_eq!(
            out.iter().map(|o| o.directory.as_str()).collect::<Vec<_>>(),
            vec!["wanted"],
            "a scoped sync reports only its own skill"
        );
        assert!(
            target.join("other").exists(),
            "another skill's orphan is not this skill's business"
        );
    }

    #[test]
    fn sync_removes_clean_orphans_only() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        let target = home.join(".gemini/skills");
        create_skill(&app, "doomed", Some("d")).unwrap();
        // A hand-written neighbour that must survive the orphan pass.
        write_skill(&target, "mine", "mine", "hand written");
        sync_skills_into(&target, &app, "gemini-user", &SyncOptions::default());
        assert!(target.join("doomed/SKILL.md").is_file());

        delete_skill(&home, &app, "doomed").unwrap();
        let out = sync_skills_into(&target, &app, "gemini-user", &SyncOptions::default());
        assert_eq!(status_of(&out, "doomed"), SyncStatus::Removed);
        assert!(!target.join("doomed").exists());
        // Not ours, never an orphan, and not even reported.
        assert!(target.join("mine/SKILL.md").is_file());
        assert!(!out.iter().any(|o| o.directory == "mine"));
    }

    #[test]
    fn propagated_copies_are_not_double_counted_by_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        create_skill(&app, "shared", Some("d")).unwrap();
        sync_skills_into(
            &home.join(".claude/skills"),
            &app,
            "claude-user",
            &SyncOptions::default(),
        );

        let found = discover(&home, &app);
        let shared: Vec<_> = found.iter().filter(|s| s.directory == "shared").collect();
        assert_eq!(shared.len(), 1, "expected one entry, got {shared:?}");
        assert_eq!(shared[0].provenance, SkillProvenance::AoeManaged);
    }

    #[test]
    fn adopting_a_propagated_copy_drops_its_deployment_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        create_skill(&app, "shared", Some("d")).unwrap();
        sync_skills_into(
            &home.join(".claude/skills"),
            &app,
            "claude-user",
            &SyncOptions::default(),
        );

        adopt_skill(
            &home,
            &app,
            &SkillProvenance::External {
                root: "claude-user".to_string(),
            },
            "shared",
            Some("forked"),
        )
        .unwrap();
        assert!(app.join("skills/forked/SKILL.md").is_file());
        assert!(
            !app.join("skills/forked").join(PROPAGATION_MARKER).exists(),
            "a managed skill must never carry a deployment marker"
        );
    }

    #[test]
    fn package_digest_ignores_the_marker_and_tracks_content() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pkg");
        write_skill(tmp.path(), "pkg", "pkg", "d");
        let bare = package_digest(&dir).unwrap();

        std::fs::write(dir.join(PROPAGATION_MARKER), "{}").unwrap();
        assert_eq!(package_digest(&dir).unwrap(), bare, "marker is excluded");

        std::fs::write(dir.join("extra.md"), "x").unwrap();
        assert_ne!(package_digest(&dir).unwrap(), bare, "other files count");
    }

    #[test]
    fn every_root_names_a_real_agent_and_owns_it_alone() {
        for root in SKILL_ROOTS {
            assert!(
                crate::agents::get_agent(root.primary_agent).is_some(),
                "{} names primary agent {:?}, which is not in the agent registry",
                root.id,
                root.primary_agent
            );
            assert!(
                root.consumers.contains(&root.primary_agent),
                "{} is primary for {:?} but does not list it as a consumer",
                root.id,
                root.primary_agent
            );
            // One root per agent, or sync_for_agent would silently pick one.
            assert_eq!(
                SKILL_ROOTS
                    .iter()
                    .filter(|r| r.primary_agent == root.primary_agent)
                    .count(),
                1,
                "{:?} is the primary agent of more than one root",
                root.primary_agent
            );
        }
    }

    #[test]
    fn agent_scoped_sync_targets_one_root_per_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let app = tmp.path().join("app");
        create_skill(&app, "shared", Some("d")).unwrap();

        // opencode reads three roots but is primary for exactly one, so it gets
        // one copy rather than one per readable root.
        sync_for_agent(&home, &app, "opencode").unwrap();
        assert!(home
            .join(".config/opencode/skills/shared/SKILL.md")
            .is_file());
        assert!(!home.join(".claude/skills/shared").exists());
        assert!(!home.join(".agents/skills/shared").exists());

        // An agent with no known skills location is not an error, just absent.
        assert!(sync_for_agent(&home, &app, "cursor").is_none());

        // Every root has a primary agent, so sync-all reaches all of them.
        sync_all_roots(&home, &app, &SyncOptions::default());
        for root in skill_roots() {
            assert!(
                home.join(root.relative_path).join("shared").exists(),
                "{} missing",
                root.id
            );
        }
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

        let malformed = app.join("skills/not-a-skill");
        std::fs::create_dir_all(&malformed).unwrap();
        assert!(matches!(
            delete_skill(&home, &app, "not-a-skill"),
            Err(SkillError::NotFound(_))
        ));
        assert!(malformed.exists());
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
                &SkillProvenance::External {
                    root: "claude-user".to_string()
                },
                "big",
                None
            ),
            Err(SkillError::InvalidInput(_))
        ));
        assert!(!app.join("skills/big").exists());
    }

    #[test]
    fn package_copy_enforces_file_count_byte_and_depth_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let limits = CopyLimits {
            total_bytes: 8,
            file_bytes: 8,
            files: 1,
            depth: 1,
        };

        let files_src = tmp.path().join("files-src");
        std::fs::create_dir_all(&files_src).unwrap();
        std::fs::write(files_src.join("one"), "1").unwrap();
        std::fs::write(files_src.join("two"), "2").unwrap();
        assert!(copy_tree_bounded(
            &files_src,
            &tmp.path().join("files-dst"),
            0,
            &mut CopyBudget::default(),
            limits,
        )
        .is_err());

        let bytes_src = tmp.path().join("bytes-src");
        std::fs::create_dir_all(&bytes_src).unwrap();
        std::fs::write(bytes_src.join("large"), "123456789").unwrap();
        assert!(copy_tree_bounded(
            &bytes_src,
            &tmp.path().join("bytes-dst"),
            0,
            &mut CopyBudget::default(),
            limits,
        )
        .is_err());

        let depth_src = tmp.path().join("depth-src");
        std::fs::create_dir_all(depth_src.join("one/two")).unwrap();
        assert!(copy_tree_bounded(
            &depth_src,
            &tmp.path().join("depth-dst"),
            0,
            &mut CopyBudget::default(),
            limits,
        )
        .is_err());
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
                &SkillProvenance::External {
                    root: "claude-user".to_string()
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

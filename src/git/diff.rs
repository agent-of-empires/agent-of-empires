//! Git diff computation module
//!
//! Provides functionality for computing diffs between branches/commits
//! and the working directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use similar::{ChangeTag, TextDiff};

use super::error::{GitError, Result};

/// Status of a file in the diff
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Conflicted,
}

impl FileStatus {
    /// Returns a single character indicator for the status
    pub fn indicator(&self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
            FileStatus::Untracked => '?',
            FileStatus::Conflicted => 'U',
        }
    }

    /// Returns a human-readable label
    pub fn label(&self) -> &'static str {
        match self {
            FileStatus::Added => "added",
            FileStatus::Modified => "modified",
            FileStatus::Deleted => "deleted",
            FileStatus::Renamed => "renamed",
            FileStatus::Copied => "copied",
            FileStatus::Untracked => "untracked",
            FileStatus::Conflicted => "conflicted",
        }
    }
}

/// Represents a file that has changed
#[derive(Debug, Clone)]
pub struct DiffFile {
    /// Path to the file (relative to repo root)
    pub path: PathBuf,
    /// Previous path if renamed
    pub old_path: Option<PathBuf>,
    /// Status of the change
    pub status: FileStatus,
    /// Number of lines added
    pub additions: usize,
    /// Number of lines deleted
    pub deletions: usize,
}

/// A single line in a diff with change information
#[derive(Debug, Clone)]
pub struct DiffLine {
    /// The type of change
    pub tag: ChangeTag,
    /// Line number in old file (None for insertions)
    pub old_line_num: Option<usize>,
    /// Line number in new file (None for deletions)
    pub new_line_num: Option<usize>,
    /// The actual content of the line
    pub content: String,
}

/// A hunk (group of changes) in a diff
#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// Starting line in old file
    pub old_start: usize,
    /// Number of lines in old file
    pub old_lines: usize,
    /// Starting line in new file
    pub new_start: usize,
    /// Number of lines in new file
    pub new_lines: usize,
    /// Lines in this hunk
    pub lines: Vec<DiffLine>,
}

/// Complete diff for a single file
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// The file being diffed
    pub file: DiffFile,
    /// Hunks of changes
    pub hunks: Vec<DiffHunk>,
    /// Whether this is a binary file
    pub is_binary: bool,
}

/// Compute the list of changed files between a base branch and the working directory.
/// Uses the merge-base of HEAD and the base branch, so only changes introduced
/// on the current branch are shown (matching GitHub PR diff behavior).
pub fn compute_changed_files(repo_path: &Path, base_branch: &str) -> Result<Vec<DiffFile>> {
    let repo = super::open_repo_at(repo_path)?;

    let base_tree = get_merge_base_tree(&repo, base_branch)?;

    // Create diff options
    let mut opts = git2::DiffOptions::new();
    opts.include_untracked(true);
    opts.recurse_untracked_dirs(true);

    // Get diff from base tree to working directory (includes index)
    let diff = repo.diff_tree_to_workdir_with_index(Some(&base_tree), Some(&mut opts))?;

    // Find renames/copies
    let mut find_opts = git2::DiffFindOptions::new();
    find_opts.renames(true);
    find_opts.copies(true);
    let mut diff = diff;
    diff.find_similar(Some(&mut find_opts))?;

    let mut files = Vec::new();
    let mut stats_map: HashMap<PathBuf, (usize, usize)> = HashMap::new();

    // First pass: collect stats
    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        if let Some(path) = delta.new_file().path().or(delta.old_file().path()) {
            let entry = stats_map.entry(path.to_path_buf()).or_insert((0, 0));
            match line.origin() {
                '+' => entry.0 += 1,
                '-' => entry.1 += 1,
                _ => {}
            }
        }
        true
    })?;

    // Second pass: collect files
    for delta in diff.deltas() {
        let status = match delta.status() {
            git2::Delta::Added => FileStatus::Added,
            git2::Delta::Deleted => FileStatus::Deleted,
            git2::Delta::Modified => FileStatus::Modified,
            git2::Delta::Renamed => FileStatus::Renamed,
            git2::Delta::Copied => FileStatus::Copied,
            git2::Delta::Untracked => FileStatus::Untracked,
            _ => continue,
        };

        let path = delta
            .new_file()
            .path()
            .or(delta.old_file().path())
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        let old_path = if status == FileStatus::Renamed || status == FileStatus::Copied {
            delta.old_file().path().map(|p| p.to_path_buf())
        } else {
            None
        };

        let (additions, deletions) = stats_map.get(&path).copied().unwrap_or((0, 0));

        files.push(DiffFile {
            path,
            old_path,
            status,
            additions,
            deletions,
        });
    }

    // Append conflicted files from the index (not visible via diff_tree_to_workdir_with_index)
    let index = repo.index()?;
    if index.has_conflicts() {
        for conflict in index.conflicts()? {
            let conflict = conflict?;
            let path = conflict
                .our
                .as_ref()
                .or(conflict.their.as_ref())
                .or(conflict.ancestor.as_ref())
                .and_then(|entry| std::str::from_utf8(&entry.path).ok())
                .map(PathBuf::from);

            if let Some(path) = path {
                if !files.iter().any(|f| f.path == path) {
                    files.push(DiffFile {
                        path,
                        old_path: None,
                        status: FileStatus::Conflicted,
                        additions: 0,
                        deletions: 0,
                    });
                }
            }
        }
    }

    // Sort by path for consistent ordering
    files.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(files)
}

/// Resolve a reference to a commit (branch name, tag, or commit hash)
fn get_commit_from_ref<'a>(
    repo: &'a git2::Repository,
    reference: &str,
) -> Result<git2::Commit<'a>> {
    let remote_ref = format!("origin/{}", reference);

    // Try as a local branch
    if let Ok(branch) = repo.find_branch(reference, git2::BranchType::Local) {
        let local_commit = branch.get().peel_to_commit()?;
        // When the local branch is strictly behind its `origin/<ref>`
        // tracking counterpart (local is a strict ancestor, no
        // divergence), diff against the remote tip instead. This matches
        // GitHub PR semantics and stops a stale local base from rendering
        // upstream churn as session changes. See #1029, #1951, #2164.
        if let Ok(remote) = repo.find_branch(&remote_ref, git2::BranchType::Remote) {
            if let Ok(remote_commit) = remote.get().peel_to_commit() {
                if local_commit.id() != remote_commit.id()
                    && repo
                        .graph_descendant_of(remote_commit.id(), local_commit.id())
                        .unwrap_or(false)
                {
                    return Ok(remote_commit);
                }
            }
        }
        return Ok(local_commit);
    }

    // Try as a remote branch
    if let Ok(branch) = repo.find_branch(&remote_ref, git2::BranchType::Remote) {
        return Ok(branch.get().peel_to_commit()?);
    }

    // Try as a reference/commit
    let obj = repo.revparse_single(reference)?;
    obj.peel_to_commit()
        .map_err(|_| GitError::BranchNotFound(reference.to_string()))
}

/// Get the merge-base tree between HEAD and the given reference.
/// This produces GitHub-style PR diffs: only changes introduced on the
/// current branch are shown, excluding new commits on the base branch
/// that haven't been merged in yet.
///
/// Falls back to the ref's tip tree if HEAD can't be resolved (e.g.
/// on an unborn branch or when comparing against HEAD itself).
fn get_merge_base_tree<'a>(repo: &'a git2::Repository, reference: &str) -> Result<git2::Tree<'a>> {
    let base_commit = get_commit_from_ref(repo, reference)?;

    // If we can resolve HEAD, compute the merge-base
    if let Ok(head_ref) = repo.head() {
        if let Ok(head_commit) = head_ref.peel_to_commit() {
            if let Ok(merge_base_oid) = repo.merge_base(head_commit.id(), base_commit.id()) {
                let merge_base_commit = repo.find_commit(merge_base_oid)?;
                return Ok(merge_base_commit.tree()?);
            }
        }
    }

    // Fallback: use the base branch tip directly (e.g. unborn branch,
    // or no common ancestor -- same as old behavior)
    Ok(base_commit.tree()?)
}

/// Check whether the merge-base between HEAD and the given base branch can
/// be computed. Returns `Some(warning)` if the diff will fall back to
/// comparing against the branch tip directly (which includes unrelated
/// changes from the base branch).
pub fn check_merge_base_status(repo_path: &Path, base_branch: &str) -> Option<String> {
    let repo = match super::open_repo_at(repo_path) {
        Ok(r) => r,
        Err(_) => return Some("Could not open repository.".to_string()),
    };

    let base_commit = match get_commit_from_ref(&repo, base_branch) {
        Ok(c) => c,
        Err(_) => {
            return Some(format!(
                "Branch '{}' not found. Diff may include unrelated changes.",
                base_branch
            ))
        }
    };

    let head_ref =
        match repo.head() {
            Ok(r) => r,
            Err(_) => return Some(
                "Could not resolve HEAD. Comparing against the tip of the base branch directly, \
                 which may include unrelated changes."
                    .to_string(),
            ),
        };

    let head_commit =
        match head_ref.peel_to_commit() {
            Ok(c) => c,
            Err(_) => return Some(
                "HEAD does not point to a commit. Comparing against the tip of the base branch \
                 directly, which may include unrelated changes."
                    .to_string(),
            ),
        };

    if head_commit.id() == base_commit.id() {
        // Same commit, no merge-base needed
        return None;
    }

    match repo.merge_base(head_commit.id(), base_commit.id()) {
        Ok(_) => None,
        Err(_) => Some(format!(
            "No common ancestor found between HEAD and '{}'. The branches have unrelated \
             histories, so the diff is comparing against the tip of '{}' directly and may \
             include unrelated changes.",
            base_branch, base_branch
        )),
    }
}

/// Compute the full diff for a specific file.
/// Uses the merge-base of HEAD and the base branch so only changes from
/// the current branch are shown.
pub fn compute_file_diff(
    repo_path: &Path,
    file_path: &Path,
    base_branch: &str,
    context_lines: usize,
) -> Result<FileDiff> {
    let repo = super::open_repo_at(repo_path)?;
    let workdir = repo.workdir().ok_or(GitError::NotAGitRepo)?.to_path_buf();

    let FileState {
        old_content,
        new_content,
        is_binary,
        status,
    } = read_file_state(&repo, &workdir, file_path, base_branch)?;

    if is_binary {
        return Ok(FileDiff {
            file: DiffFile {
                path: file_path.to_path_buf(),
                old_path: None,
                status,
                additions: 0,
                deletions: 0,
            },
            hunks: Vec::new(),
            is_binary: true,
        });
    }

    // Compute diff using similar
    let text_diff = TextDiff::from_lines(&old_content, &new_content);
    let mut hunks = Vec::new();
    let mut additions = 0;
    let mut deletions = 0;

    for group in text_diff.grouped_ops(context_lines) {
        let mut hunk_lines = Vec::new();
        let mut old_start = None;
        let mut new_start = None;
        let mut old_count = 0;
        let mut new_count = 0;

        for op in &group {
            for change in text_diff.iter_changes(op) {
                let tag = change.tag();
                let content = change.value().to_string();

                // Track line counts
                match tag {
                    ChangeTag::Delete => {
                        deletions += 1;
                        old_count += 1;
                    }
                    ChangeTag::Insert => {
                        additions += 1;
                        new_count += 1;
                    }
                    ChangeTag::Equal => {
                        old_count += 1;
                        new_count += 1;
                    }
                }

                // Track start lines
                if old_start.is_none() {
                    old_start = change.old_index();
                }
                if new_start.is_none() {
                    new_start = change.new_index();
                }

                hunk_lines.push(DiffLine {
                    tag,
                    old_line_num: change.old_index().map(|i| i + 1),
                    new_line_num: change.new_index().map(|i| i + 1),
                    content,
                });
            }
        }

        if !hunk_lines.is_empty() {
            hunks.push(DiffHunk {
                old_start: old_start.map(|i| i + 1).unwrap_or(1),
                old_lines: old_count,
                new_start: new_start.map(|i| i + 1).unwrap_or(1),
                new_lines: new_count,
                lines: hunk_lines,
            });
        }
    }

    Ok(FileDiff {
        file: DiffFile {
            path: file_path.to_path_buf(),
            old_path: None,
            status,
            additions,
            deletions,
        },
        hunks,
        is_binary: false,
    })
}

/// Raw old/new contents of a single file plus its status.
///
/// Feeds the contents-based diff endpoint, which lets the web client parse
/// and render diffs itself (via `@pierre/diffs`) instead of consuming
/// server-computed hunks. Additions/deletions are intentionally absent: the
/// renderer derives them, and the file-list endpoint already carries per-file
/// counts for the sidebar.
#[derive(Debug, Clone)]
pub struct FileContents {
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub status: FileStatus,
    pub old_content: String,
    pub new_content: String,
    /// Unified diff of old → new, computed server-side so the web client can
    /// parse it as text instead of re-running a (slow, main-thread) diff
    /// algorithm on the raw contents. Empty for binary files.
    pub patch: String,
    pub is_binary: bool,
}

/// Read the old (base-tree) and new (working-dir) contents of a single file
/// plus a server-computed unified diff of the two.
///
/// The diff is computed here, natively, precisely so the web client never
/// has to: `@pierre/diffs` parses the patch text and only offloads
/// highlighting to its worker pool.
pub fn compute_file_contents(
    repo_path: &Path,
    file_path: &Path,
    base_branch: &str,
) -> Result<FileContents> {
    let repo = super::open_repo_at(repo_path)?;
    let workdir = repo.workdir().ok_or(GitError::NotAGitRepo)?.to_path_buf();
    let state = read_file_state(&repo, &workdir, file_path, base_branch)?;
    // libgit2's xdiff, not the `similar` crate: xdiff is C compiled optimized
    // regardless of cargo profile and carries git's pathological-input
    // heuristics. `similar`'s Myers took ~20s on a +10k/-13k lockfile churn
    // in a debug build; xdiff handles the same input in milliseconds.
    let patch = if state.is_binary {
        String::new()
    } else {
        let mut opts = git2::DiffOptions::new();
        opts.context_lines(3);
        let mut patch = git2::Patch::from_buffers(
            state.old_content.as_bytes(),
            Some(file_path),
            state.new_content.as_bytes(),
            Some(file_path),
            Some(&mut opts),
        )?;
        String::from_utf8_lossy(&patch.to_buf()?).into_owned()
    };
    Ok(FileContents {
        path: file_path.to_path_buf(),
        old_path: None,
        status: state.status,
        old_content: state.old_content,
        new_content: state.new_content,
        patch,
        is_binary: state.is_binary,
    })
}

/// Intermediate result shared by [`compute_file_diff`] and
/// [`compute_file_contents`]: the raw old/new text, binary flag, and status.
struct FileState {
    old_content: String,
    new_content: String,
    is_binary: bool,
    status: FileStatus,
}

/// Read a file's base-tree and working-dir contents, detect binary, and
/// classify its status (added/deleted/modified/conflicted).
fn read_file_state(
    repo: &git2::Repository,
    workdir: &Path,
    file_path: &Path,
    base_branch: &str,
) -> Result<FileState> {
    let base_tree = get_merge_base_tree(repo, base_branch)?;

    // Get old content from base tree (as bytes first to check for binary)
    let old_bytes = get_blob_bytes(repo, &base_tree, file_path);
    let old_is_binary = old_bytes
        .as_ref()
        .map(|b| is_binary_bytes(b))
        .unwrap_or(false);

    // Get new content from working directory (as bytes first to check for binary)
    let full_path = workdir.join(file_path);
    let new_bytes = if full_path.exists() {
        std::fs::read(&full_path).ok()
    } else {
        None
    };
    let new_is_binary = new_bytes
        .as_ref()
        .map(|b| is_binary_bytes(b))
        .unwrap_or(false);

    let is_binary = old_is_binary || new_is_binary;

    // Convert to strings (safe now that we've checked for binary)
    let old_content = old_bytes
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    let new_content = new_bytes
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();

    // Determine file status
    let index = repo.index()?;
    let is_conflicted = index.has_conflicts()
        && index.conflicts()?.any(|c| {
            c.ok()
                .and_then(|c| c.our.or(c.their).or(c.ancestor))
                .and_then(|e| {
                    std::str::from_utf8(&e.path)
                        .ok()
                        .map(|p| Path::new(p) == file_path)
                })
                .unwrap_or(false)
        });

    let status = if is_conflicted {
        FileStatus::Conflicted
    } else if old_content.is_empty() && !new_content.is_empty() {
        FileStatus::Added
    } else if !old_content.is_empty() && new_content.is_empty() && !full_path.exists() {
        FileStatus::Deleted
    } else {
        FileStatus::Modified
    };

    Ok(FileState {
        old_content,
        new_content,
        is_binary,
        status,
    })
}

/// Get raw bytes of a blob from a tree by path
fn get_blob_bytes(repo: &git2::Repository, tree: &git2::Tree, path: &Path) -> Option<Vec<u8>> {
    let entry = tree.get_path(path).ok()?;
    let obj = entry.to_object(repo).ok()?;
    let blob = obj.as_blob()?;
    Some(blob.content().to_vec())
}

/// Check if raw bytes appear to be binary (null byte heuristic)
fn is_binary_bytes(content: &[u8]) -> bool {
    content.iter().take(8000).any(|&b| b == 0)
}

/// Full working-directory contents of a tracked file that has no diff against
/// the base branch (an agent-cited but unchanged file). See #1810.
#[derive(Debug, Clone)]
pub struct FullFileContents {
    pub content: String,
    pub is_binary: bool,
}

/// Read the full contents of an unchanged, agent-cited file for the full-file
/// fallback in the structured-view diff viewer. See #1810.
///
/// Membership is gated on the path being a tracked **blob** in `HEAD`. That
/// excludes `.git/` internals, gitignored secrets like `.env`, and submodule
/// gitlinks (a commit entry, not a blob), none of which should be readable
/// through this endpoint. The *contents* are then read from the working
/// directory via `canonical_path` (already canonicalized and containment-checked
/// by the caller) so what renders matches what the agent actually saw and any
/// symlink resolves through the same containment guard.
///
/// Returns `Ok(None)` when the path is not a tracked blob or is not a regular
/// file on disk, so the caller answers `404` without disclosing whether an
/// untracked file exists.
pub fn compute_unchanged_file_contents(
    repo_path: &Path,
    file_path: &Path,
    canonical_path: &Path,
) -> Result<Option<FullFileContents>> {
    let repo = super::open_repo_at(repo_path)?;
    // Unborn HEAD (no commits yet) means nothing is tracked.
    let head_tree = match repo.head().and_then(|h| h.peel_to_tree()) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    // Only serve tracked blobs. Directories (trees) and submodules (gitlink
    // commits) are rejected, as are paths absent from HEAD (untracked/ignored).
    match head_tree.get_path(file_path) {
        Ok(entry) if entry.kind() == Some(git2::ObjectType::Blob) => {}
        _ => return Ok(None),
    }
    // `canonical_path` is the already-resolved working-dir path; require a
    // regular file so a cited directory or special file yields 404, not a read
    // error.
    if !canonical_path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(canonical_path).map_err(GitError::IoError)?;
    let is_binary = is_binary_bytes(&bytes);
    let content = if is_binary {
        String::new()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok(Some(FullFileContents { content, is_binary }))
}

/// Get the content of a file from the working directory
pub fn get_working_file_content(repo_path: &Path, file_path: &Path) -> Result<String> {
    let repo = super::open_repo_at(repo_path)?;
    let workdir = repo.workdir().ok_or(GitError::NotAGitRepo)?;
    let full_path = workdir.join(file_path);

    std::fs::read_to_string(&full_path).map_err(GitError::IoError)
}

/// Save content to a file in the working directory
pub fn save_working_file_content(repo_path: &Path, file_path: &Path, content: &str) -> Result<()> {
    let repo = super::open_repo_at(repo_path)?;
    let workdir = repo.workdir().ok_or(GitError::NotAGitRepo)?;
    let full_path = workdir.join(file_path);

    // Create parent directories if needed
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&full_path, content).map_err(GitError::IoError)
}

/// List available branches in the repository
pub fn list_branches(repo_path: &Path) -> Result<Vec<String>> {
    let repo = super::open_repo_at(repo_path)?;
    let mut branches = Vec::new();

    // Local branches
    for branch in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = branch?;
        if let Some(name) = branch.name()? {
            branches.push(name.to_string());
        }
    }

    // Sort alphabetically, but put main/master first
    branches.sort_by(|a, b| {
        let a_is_main = a == "main" || a == "master";
        let b_is_main = b == "main" || b == "master";
        match (a_is_main, b_is_main) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });

    Ok(branches)
}

/// One entry returned by [`list_branches_with_remotes`].
#[derive(Debug, Clone)]
pub struct BranchEntry {
    /// Short branch name (e.g. `feature/x`). For remote-only branches
    /// the remote prefix (`origin/`) is stripped; pass the short name
    /// to `create_worktree` and it resolves the remote internally.
    pub name: String,
    /// True if the branch only exists on the remote (no matching local
    /// branch). The UI surfaces this so the user knows the new
    /// worktree will fetch + branch from the remote tip.
    pub remote_only: bool,
}

/// List local branches plus remote-only branches (stripped of their
/// remote prefix). Used by the worktree base-branch picker so users
/// can pick a teammate's branch they haven't fetched locally. See #948.
pub fn list_branches_with_remotes(repo_path: &Path) -> Result<Vec<BranchEntry>> {
    let repo = super::open_repo_at(repo_path)?;
    let mut locals: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut entries: Vec<BranchEntry> = Vec::new();

    for branch in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = branch?;
        if let Some(name) = branch.name()? {
            locals.insert(name.to_string());
            entries.push(BranchEntry {
                name: name.to_string(),
                remote_only: false,
            });
        }
    }

    for branch in repo.branches(Some(git2::BranchType::Remote))? {
        let (branch, _) = branch?;
        if let Some(name) = branch.name()? {
            // Strip the leading "<remote>/" segment. `HEAD` symbolic
            // refs ("origin/HEAD") are skipped; they're not a real
            // branch the user can base off.
            let short = name.split_once('/').map(|(_, rest)| rest).unwrap_or(name);
            if short == "HEAD" || short.is_empty() {
                continue;
            }
            if locals.contains(short) {
                continue;
            }
            entries.push(BranchEntry {
                name: short.to_string(),
                remote_only: true,
            });
        }
    }

    entries.sort_by(|a, b| {
        let a_is_main = a.name == "main" || a.name == "master";
        let b_is_main = b.name == "main" || b.name == "master";
        match (a_is_main, b_is_main) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        }
    });

    Ok(entries)
}

/// Get the default branch name (main or master).
/// Delegates to `GitWorktree::detect_default_branch` which also checks
/// remote tracking refs as a fallback.
pub fn get_default_branch(repo_path: &Path) -> Result<String> {
    let git_wt = super::GitWorktree::new(repo_path.to_path_buf())?;
    git_wt.detect_default_branch()
}

/// Get the default base ref for diffing as a remote-qualified ref name
/// when the freshest copy lives on a non-default remote. Falls back to
/// the short branch name when the picked candidate is local.
///
/// Use this (not `get_default_branch`) for diff base resolution, so
/// fork + `upstream` layouts compare against `upstream/main` instead
/// of a stale local `main`. See issue #1029.
pub fn get_default_base_ref(repo_path: &Path) -> Result<String> {
    let git_wt = super::GitWorktree::new(repo_path.to_path_buf())?;
    Ok(git_wt.detect_default_branch_info()?.qualified_ref())
}

/// Returns `Ok(())` when `reference` resolves to a commit in the repo at
/// `repo_path` using the same resolution chain (`local branch`,
/// `origin/<ref>` tracking branch, `revparse_single`) that
/// `compute_changed_files` consults. Used by the CLI to validate
/// user-provided refs (e.g. `aoe session set-base`) before persisting
/// a per-session diff base override. See #970.
pub fn validate_ref(repo_path: &Path, reference: &str) -> Result<()> {
    let repo = git2::Repository::open(repo_path)?;
    get_commit_from_ref(&repo, reference).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_repo() -> (TempDir, git2::Repository) {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Create initial commit
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        // Create a test file
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

        // Add and commit
        {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("test.txt")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
                .unwrap();
        }

        (dir, repo)
    }

    /// Helper to create a commit on the current branch
    fn commit_file(repo: &git2::Repository, path: &str, content: &str, message: &str) {
        let dir = repo.workdir().unwrap();
        let file_path = dir.join(path);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&file_path, content).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new(path)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
            .unwrap();
    }

    #[test]
    fn unchanged_file_contents_serves_tracked_file() {
        let (dir, _repo) = setup_test_repo();
        let canonical = dir.path().join("test.txt").canonicalize().unwrap();
        let out = compute_unchanged_file_contents(dir.path(), Path::new("test.txt"), &canonical)
            .unwrap()
            .expect("tracked file should be served");
        assert_eq!(out.content, "line 1\nline 2\nline 3\n");
        assert!(!out.is_binary);
    }

    #[test]
    fn unchanged_file_contents_rejects_untracked_file() {
        // A gitignored secret never committed: present on disk, not in HEAD, so
        // the tracked-blob gate refuses it (returns None -> 404). See #1810.
        let (dir, _repo) = setup_test_repo();
        let secret = dir.path().join(".env");
        fs::write(&secret, "API_KEY=supersecret\n").unwrap();
        let canonical = secret.canonicalize().unwrap();
        let out =
            compute_unchanged_file_contents(dir.path(), Path::new(".env"), &canonical).unwrap();
        assert!(out.is_none(), "untracked .env must not be served");
    }

    #[test]
    fn unchanged_file_contents_rejects_git_internals() {
        // `.git/config` lives inside the worktree but is not a tracked blob.
        let (dir, _repo) = setup_test_repo();
        let canonical = dir.path().join(".git/config").canonicalize().unwrap();
        let out = compute_unchanged_file_contents(dir.path(), Path::new(".git/config"), &canonical)
            .unwrap();
        assert!(out.is_none(), ".git internals must not be served");
    }

    /// Ensure a local branch exists at the given commit.
    /// This keeps tests stable when git init defaults to `main` vs `master`.
    fn ensure_local_branch(repo: &git2::Repository, name: &str, commit: &git2::Commit<'_>) {
        if repo.find_branch(name, git2::BranchType::Local).is_err() {
            repo.branch(name, commit, false).unwrap();
        }
    }

    /// Set up a repo with a main branch and a feature branch that diverged.
    /// main has extra commits that the feature branch doesn't have.
    fn setup_branching_repo() -> (TempDir, git2::Repository) {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Initial commit on default branch (master from git init)
        commit_file(&repo, "shared.txt", "shared content\n", "Initial commit");

        // Create "main" and "feature" branches at this point
        {
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            ensure_local_branch(&repo, "main", &head);
            ensure_local_branch(&repo, "feature", &head);
        }

        // Add a commit on main that the feature branch won't have
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(
            &repo,
            "main_only.txt",
            "this file only exists on main\n",
            "Add main-only file",
        );
        commit_file(
            &repo,
            "shared.txt",
            "shared content\nmain added this line\n",
            "Modify shared file on main",
        );

        // Switch to feature branch and make a feature-specific change
        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(
            &repo,
            "feature_only.txt",
            "feature-specific content\n",
            "Add feature-only file",
        );

        (dir, repo)
    }

    #[test]
    fn test_merge_base_excludes_main_only_changes() {
        let (dir, _repo) = setup_branching_repo();

        // We're on the feature branch, comparing against main.
        // Only feature_only.txt should show up -- NOT main_only.txt
        // and NOT the main-side modification to shared.txt.
        let files = compute_changed_files(dir.path(), "main").unwrap();

        let paths: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();
        assert!(
            paths.contains(&Path::new("feature_only.txt")),
            "feature_only.txt should appear in diff, got: {:?}",
            paths
        );
        assert!(
            !paths.contains(&Path::new("main_only.txt")),
            "main_only.txt should NOT appear (it's a main-only change), got: {:?}",
            paths
        );
        // shared.txt was only modified on main, not on the feature branch,
        // so it should not appear in the merge-base diff
        assert!(
            !paths.contains(&Path::new("shared.txt")),
            "shared.txt should NOT appear (only changed on main), got: {:?}",
            paths
        );
    }

    #[test]
    fn test_merge_base_file_diff_uses_correct_base() {
        let (dir, _repo) = setup_branching_repo();

        // The file diff for feature_only.txt should show it as entirely new
        let diff = compute_file_diff(dir.path(), Path::new("feature_only.txt"), "main", 3).unwrap();
        assert_eq!(diff.file.status, FileStatus::Added);
        assert!(diff.file.additions > 0);
        assert_eq!(diff.file.deletions, 0);
    }

    #[test]
    fn test_merge_base_in_worktree() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Initial commit
        commit_file(&repo, "shared.txt", "shared\n", "Initial commit");

        // Create main branch and feature branch
        {
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            ensure_local_branch(&repo, "main", &head);
        }

        // Add a commit to main
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "main_only.txt", "main\n", "Main-only commit");

        // Create a worktree for "feature" branch
        let wt_path = dir.path().parent().unwrap().join("feature_worktree");
        {
            let first_commit = repo
                .revparse_single("main~1")
                .unwrap()
                .peel_to_commit()
                .unwrap();
            ensure_local_branch(&repo, "feature", &first_commit);
        }

        // Add worktree
        repo.worktree(
            "feature",
            &wt_path,
            Some(
                git2::WorktreeAddOptions::new().reference(Some(
                    &repo
                        .find_branch("feature", git2::BranchType::Local)
                        .unwrap()
                        .into_reference(),
                )),
            ),
        )
        .unwrap();

        // Open the worktree repo and add a feature-only file
        let wt_repo = git2::Repository::open(&wt_path).unwrap();
        commit_file(&wt_repo, "feature_only.txt", "feature\n", "Feature commit");

        // Now test: from the worktree, diff against main should only show feature_only.txt
        let files = compute_changed_files(&wt_path, "main").unwrap();
        let paths: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();

        assert!(
            paths.contains(&Path::new("feature_only.txt")),
            "feature_only.txt should appear, got: {:?}",
            paths
        );
        assert!(
            !paths.contains(&Path::new("main_only.txt")),
            "main_only.txt should NOT appear in worktree diff, got: {:?}",
            paths
        );

        // Cleanup worktree
        fs::remove_dir_all(&wt_path).ok();
    }

    #[test]
    fn test_check_merge_base_status_ok_when_common_ancestor_exists() {
        let (dir, _repo) = setup_branching_repo();
        // Feature branch and main share a common ancestor, so no warning
        let status = check_merge_base_status(dir.path(), "main");
        assert!(
            status.is_none(),
            "Expected no warning when merge-base exists, got: {:?}",
            status
        );
    }

    #[test]
    fn test_check_merge_base_status_warns_on_missing_branch() {
        let (dir, _repo) = setup_test_repo();
        let status = check_merge_base_status(dir.path(), "nonexistent-branch");
        assert!(status.is_some(), "Expected warning for missing branch");
        assert!(
            status.unwrap().contains("not found"),
            "Warning should mention branch not found"
        );
    }

    #[test]
    fn test_check_merge_base_status_warns_on_unrelated_histories() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Create first commit on default branch
        commit_file(&repo, "file_a.txt", "a\n", "First commit");

        // Create an orphan branch with no shared history
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        {
            fs::write(dir.path().join("file_b.txt"), "b\n").unwrap();
            let mut index = repo.index().unwrap();
            index.clear().unwrap();
            index.add_path(Path::new("file_b.txt")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            // Commit with no parents (orphan)
            let oid = repo
                .commit(None, &sig, &sig, "Orphan commit", &tree, &[])
                .unwrap();
            let commit = repo.find_commit(oid).unwrap();
            repo.branch("orphan", &commit, false).unwrap();
        }

        // HEAD is on master/default branch, compare against orphan
        let status = check_merge_base_status(dir.path(), "orphan");
        assert!(status.is_some(), "Expected warning for unrelated histories");
        assert!(
            status.unwrap().contains("No common ancestor"),
            "Warning should mention no common ancestor"
        );
    }

    #[test]
    fn test_check_merge_base_status_ok_same_commit() {
        let (dir, _repo) = setup_test_repo();
        // Comparing HEAD against HEAD -- same commit, no warning
        let status = check_merge_base_status(dir.path(), "HEAD");
        assert!(
            status.is_none(),
            "Expected no warning when comparing same commit, got: {:?}",
            status
        );
    }

    #[test]
    fn test_file_status_indicator() {
        assert_eq!(FileStatus::Added.indicator(), 'A');
        assert_eq!(FileStatus::Modified.indicator(), 'M');
        assert_eq!(FileStatus::Deleted.indicator(), 'D');
        assert_eq!(FileStatus::Renamed.indicator(), 'R');
    }

    #[test]
    fn test_compute_changed_files_no_changes() {
        let (dir, _repo) = setup_test_repo();
        let files = compute_changed_files(dir.path(), "HEAD").unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_compute_changed_files_with_modification() {
        let (dir, _repo) = setup_test_repo();

        // Modify the file
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1 modified\nline 2\nline 3\n").unwrap();

        let files = compute_changed_files(dir.path(), "HEAD").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!(files[0].path, Path::new("test.txt"));
    }

    #[test]
    fn test_compute_changed_files_with_addition() {
        let (dir, _repo) = setup_test_repo();

        // Add a new file
        let new_file = dir.path().join("new.txt");
        fs::write(&new_file, "new content\n").unwrap();

        let files = compute_changed_files(dir.path(), "HEAD").unwrap();
        assert!(files.iter().any(|f| f.status == FileStatus::Untracked));
    }

    #[test]
    fn test_compute_file_diff() {
        let (dir, _repo) = setup_test_repo();

        // Modify the file
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1 modified\nline 2\nline 3\nnew line 4\n").unwrap();

        let diff = compute_file_diff(dir.path(), Path::new("test.txt"), "HEAD", 3).unwrap();

        assert!(!diff.is_binary);
        assert!(!diff.hunks.is_empty());
        assert!(diff.file.additions > 0);
    }

    #[test]
    fn test_compute_file_contents_modified() {
        let (dir, _repo) = setup_test_repo();

        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1 modified\nline 2\nline 3\nnew line 4\n").unwrap();

        let c = compute_file_contents(dir.path(), Path::new("test.txt"), "HEAD").unwrap();

        assert!(!c.is_binary);
        assert_eq!(c.status, FileStatus::Modified);
        assert_eq!(c.old_content, "line 1\nline 2\nline 3\n");
        assert_eq!(
            c.new_content,
            "line 1 modified\nline 2\nline 3\nnew line 4\n"
        );
        // Server-computed unified diff with git-style headers.
        assert!(c.patch.contains("--- a/test.txt"));
        assert!(c.patch.contains("+++ b/test.txt"));
        assert!(c.patch.contains("@@"));
        assert!(c.patch.contains("-line 1\n"));
        assert!(c.patch.contains("+line 1 modified\n"));
    }

    #[test]
    fn test_compute_file_contents_added() {
        let (dir, _repo) = setup_test_repo();

        fs::write(dir.path().join("brand_new.txt"), "hello\nworld\n").unwrap();

        let c = compute_file_contents(dir.path(), Path::new("brand_new.txt"), "HEAD").unwrap();

        assert_eq!(c.status, FileStatus::Added);
        assert!(c.old_content.is_empty());
        assert_eq!(c.new_content, "hello\nworld\n");
    }

    #[test]
    fn test_compute_file_contents_deleted() {
        let (dir, _repo) = setup_test_repo();

        fs::remove_file(dir.path().join("test.txt")).unwrap();

        let c = compute_file_contents(dir.path(), Path::new("test.txt"), "HEAD").unwrap();

        assert_eq!(c.status, FileStatus::Deleted);
        assert_eq!(c.old_content, "line 1\nline 2\nline 3\n");
        assert!(c.new_content.is_empty());
    }

    #[test]
    fn test_compute_file_contents_matches_diff_inputs() {
        // The contents path must hand back exactly the old/new text the legacy
        // hunk path diffs, so the client-side parse reproduces the same diff.
        let (dir, _repo) = setup_test_repo();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1\nchanged\nline 3\n").unwrap();

        let c = compute_file_contents(dir.path(), Path::new("test.txt"), "HEAD").unwrap();
        let d = compute_file_diff(dir.path(), Path::new("test.txt"), "HEAD", 3).unwrap();

        // Reconstruct old/new from the hunk lines and compare.
        let mut old_from_hunks = String::new();
        let mut new_from_hunks = String::new();
        for h in &d.hunks {
            for l in &h.lines {
                match l.tag {
                    ChangeTag::Delete => old_from_hunks.push_str(&l.content),
                    ChangeTag::Insert => new_from_hunks.push_str(&l.content),
                    ChangeTag::Equal => {
                        old_from_hunks.push_str(&l.content);
                        new_from_hunks.push_str(&l.content);
                    }
                }
            }
        }
        assert_eq!(c.old_content, old_from_hunks);
        assert_eq!(c.new_content, new_from_hunks);
    }

    #[test]
    fn test_list_branches() {
        let (dir, repo) = setup_test_repo();

        // Create another branch
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("feature", &commit, false).unwrap();

        let branches = list_branches(dir.path()).unwrap();
        assert!(!branches.is_empty());
    }

    #[test]
    fn test_get_default_branch() {
        let (dir, _repo) = setup_test_repo();
        // Should return the current branch (usually "master" for git init)
        let branch = get_default_branch(dir.path());
        assert!(branch.is_ok());
    }

    #[test]
    fn test_validate_ref_accepts_existing_branch() {
        let (dir, repo) = setup_test_repo();
        let head_name = repo.head().unwrap().shorthand().unwrap().to_string();
        validate_ref(dir.path(), &head_name).expect("HEAD branch should resolve");
    }

    #[test]
    fn test_validate_ref_rejects_missing_branch() {
        let (dir, _repo) = setup_test_repo();
        let err = validate_ref(dir.path(), "definitely-does-not-exist");
        assert!(err.is_err(), "missing ref should not resolve");
    }

    #[test]
    fn test_validate_ref_rejects_non_repo() {
        let dir = TempDir::new().unwrap();
        let err = validate_ref(dir.path(), "main");
        assert!(
            err.is_err(),
            "validate_ref against non-repo path should error"
        );
    }

    #[test]
    fn test_is_binary_bytes() {
        assert!(!is_binary_bytes(b"hello world"));
        assert!(!is_binary_bytes(b"line 1\nline 2"));
        assert!(is_binary_bytes(b"hello\0world"));
    }

    /// Set up a repo in a mid-merge state with a conflict on `conflicted.txt`.
    /// Returns the temp dir; HEAD is on `feature` branch.
    fn setup_conflict_repo() -> (TempDir, git2::Repository) {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        commit_file(&repo, "conflicted.txt", "base\n", "Initial commit");

        // Scope borrows of repo so commits are dropped before we return repo.
        let main_commit_id = {
            let ancestor = repo.head().unwrap().peel_to_commit().unwrap();
            ensure_local_branch(&repo, "main", &ancestor);
            repo.set_head("refs/heads/main").unwrap();
            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .unwrap();
            commit_file(&repo, "conflicted.txt", "main version\n", "Main change");
            let main_commit = repo.head().unwrap().peel_to_commit().unwrap();
            let id = main_commit.id();

            ensure_local_branch(&repo, "feature", &ancestor);
            id
        };

        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(
            &repo,
            "conflicted.txt",
            "feature version\n",
            "Feature change",
        );

        let conflict_content =
            "<<<<<<< HEAD\nfeature version\n=======\nmain version\n>>>>>>> main\n";
        fs::write(dir.path().join("conflicted.txt"), conflict_content).unwrap();

        let ancestor_blob = repo.blob(b"base\n").unwrap();
        let our_blob = repo.blob(b"feature version\n").unwrap();
        let their_blob = repo.blob(b"main version\n").unwrap();

        let make_entry = |id: git2::Oid, stage: u16, size: u32| git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: size,
            id,
            flags: stage << 12,
            flags_extended: 0,
            path: b"conflicted.txt".to_vec(),
        };

        let mut index = repo.index().unwrap();
        index.remove(Path::new("conflicted.txt"), 0).ok();
        index.add(&make_entry(ancestor_blob, 1, 5)).unwrap();
        index.add(&make_entry(our_blob, 2, 16)).unwrap();
        index.add(&make_entry(their_blob, 3, 13)).unwrap();
        index.write().unwrap();

        repo.reference("refs/heads/main", main_commit_id, true, "reset main")
            .unwrap();

        (dir, repo)
    }

    #[test]
    fn test_conflicted_file_appears_in_changed_files() {
        let (dir, _repo) = setup_conflict_repo();
        let files = compute_changed_files(dir.path(), "main").unwrap();
        let conflicted: Vec<_> = files
            .iter()
            .filter(|f| f.status == FileStatus::Conflicted)
            .collect();
        assert!(
            !conflicted.is_empty(),
            "Expected at least one Conflicted file, got: {:?}",
            files
                .iter()
                .map(|f| (&f.path, f.status))
                .collect::<Vec<_>>()
        );
        assert!(
            conflicted
                .iter()
                .any(|f| f.path == Path::new("conflicted.txt")),
            "conflicted.txt should be Conflicted"
        );
    }

    #[test]
    fn test_conflicted_file_no_duplicate_in_changed_files() {
        let (dir, _repo) = setup_conflict_repo();
        let files = compute_changed_files(dir.path(), "main").unwrap();
        let count = files
            .iter()
            .filter(|f| f.path == Path::new("conflicted.txt"))
            .count();
        assert_eq!(count, 1, "conflicted.txt should appear exactly once");
    }

    #[test]
    fn test_conflicted_file_diff_shows_markers() {
        let (dir, _repo) = setup_conflict_repo();
        let diff = compute_file_diff(dir.path(), Path::new("conflicted.txt"), "main", 3).unwrap();

        assert_eq!(diff.file.status, FileStatus::Conflicted);
        assert!(
            !diff.hunks.is_empty(),
            "Should produce hunks for conflicted file"
        );

        let all_content: String = diff
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| l.content.as_str())
            .collect();
        assert!(
            all_content.contains("<<<<<<<"),
            "Diff should contain conflict markers"
        );
        assert!(
            all_content.contains(">>>>>>>"),
            "Diff should contain conflict markers"
        );
    }

    #[test]
    fn test_save_and_get_working_file() {
        let (dir, _repo) = setup_test_repo();

        let content = "new content here\n";
        save_working_file_content(dir.path(), Path::new("test.txt"), content).unwrap();

        let loaded = get_working_file_content(dir.path(), Path::new("test.txt")).unwrap();
        assert_eq!(loaded, content);
    }

    /// Build a repo where `origin/main` is ahead of a stale local `main`,
    /// with HEAD detached at the `origin/main` tip and a clean working tree.
    /// Mirrors the #2164 scenario: a worktree cut from `origin/main` whose
    /// local `main` ref drifted behind. Returns (dir, repo, origin_tip).
    fn setup_stale_local_main() -> (TempDir, git2::Repository, git2::Oid) {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Base commit, then a local `main` pinned to it.
        commit_file(&repo, "shared.txt", "base\n", "Initial commit");
        let base = repo.head().unwrap().peel_to_commit().unwrap().id();
        ensure_local_branch(&repo, "main", &repo.find_commit(base).unwrap());

        // Advance `main` with two upstream commits (the dependabot-style churn).
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "upstream_a.txt", "a\n", "Upstream commit A");
        commit_file(&repo, "upstream_b.txt", "b\n", "Upstream commit B");
        let origin_tip = repo.head().unwrap().peel_to_commit().unwrap().id();

        // Record that tip as origin/main, then rewind local `main` to base so
        // it is strictly behind origin/main.
        repo.reference(
            "refs/remotes/origin/main",
            origin_tip,
            true,
            "set origin/main",
        )
        .unwrap();
        repo.reference("refs/heads/main", base, true, "rewind local main")
            .unwrap();

        // Detach HEAD at the origin tip with a matching (clean) working tree.
        repo.set_head_detached(origin_tip).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        (dir, repo, origin_tip)
    }

    #[test]
    fn test_stale_local_main_resolves_to_remote() {
        // Story: worktree HEAD == origin/main, local `main` strictly behind.
        // The diff must show zero changed files, not the upstream commits.
        let (dir, _repo, _tip) = setup_stale_local_main();
        let files = compute_changed_files(dir.path(), "main").unwrap();
        assert!(
            files.is_empty(),
            "stale local main behind origin/main should yield no phantom changes, got: {:?}",
            files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_local_main_equals_origin_diff_unchanged() {
        // Story: local `main` == origin/main, so resolution and the computed
        // diff are identical to the no-remote case (regression guard).
        let (dir, repo) = setup_branching_repo();
        let main_tip = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        repo.reference(
            "refs/remotes/origin/main",
            main_tip,
            true,
            "origin/main == main",
        )
        .unwrap();

        let files = compute_changed_files(dir.path(), "main").unwrap();
        let paths: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();
        assert!(
            paths.contains(&Path::new("feature_only.txt")),
            "feature_only.txt should appear, got: {:?}",
            paths
        );
        assert!(
            !paths.contains(&Path::new("main_only.txt")),
            "main_only.txt should NOT appear, got: {:?}",
            paths
        );
    }

    #[test]
    fn test_diverged_local_main_stays_local() {
        // Story: local base branch genuinely diverged from its remote (not a
        // strict ancestor), so the diff still resolves against the local branch.
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        commit_file(&repo, "shared.txt", "base\n", "Initial commit");
        let base = repo.head().unwrap().peel_to_commit().unwrap().id();
        ensure_local_branch(&repo, "main", &repo.find_commit(base).unwrap());

        // Local `main`: base -> local_main.txt.
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "local_main.txt", "local\n", "Local main commit");

        // origin/main diverges off the same base via a throwaway branch.
        ensure_local_branch(&repo, "origintmp", &repo.find_commit(base).unwrap());
        repo.set_head("refs/heads/origintmp").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "origin_main.txt", "origin\n", "Origin main commit");
        let origin_tip = repo.head().unwrap().peel_to_commit().unwrap().id();
        repo.reference(
            "refs/remotes/origin/main",
            origin_tip,
            true,
            "diverged origin/main",
        )
        .unwrap();

        // Feature branch off local `main`, then a feature change.
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let local_main_tip = repo.head().unwrap().peel_to_commit().unwrap();
        ensure_local_branch(&repo, "feature", &local_main_tip);
        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "feature.txt", "feature\n", "Feature commit");

        let files = compute_changed_files(dir.path(), "main").unwrap();
        let paths: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();
        assert!(
            paths.contains(&Path::new("feature.txt")),
            "feature.txt should appear, got: {:?}",
            paths
        );
        // Resolving against the diverged origin tip would drop the merge-base
        // back to the common ancestor and surface local_main.txt. Its absence
        // proves the local branch was used.
        assert!(
            !paths.contains(&Path::new("local_main.txt")),
            "local_main.txt should NOT appear (diverged local must not switch to origin), got: {:?}",
            paths
        );
    }
}

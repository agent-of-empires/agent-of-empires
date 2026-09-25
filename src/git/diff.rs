//! Diffs between a branch or commit and the working directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use similar::{ChangeTag, TextDiff};

use super::error::{GitError, Result};

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

#[derive(Debug, Clone)]
pub struct DiffFile {
    /// Relative to the repo root.
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub status: FileStatus,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub tag: ChangeTag,
    /// `None` for an insertion.
    pub old_line_num: Option<usize>,
    /// `None` for a deletion.
    pub new_line_num: Option<usize>,
    pub content: String,
}

/// A group of changes in a diff.
#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub file: DiffFile,
    pub hunks: Vec<DiffHunk>,
    pub is_binary: bool,
}

/// Changed files between a base branch and the working directory, from the
/// merge-base of HEAD and the base, so only this branch's changes show.
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

/// Resolve a branch name, tag, or commit hash to a commit.
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

/// The merge-base tree between HEAD and `reference`, so a diff excludes
/// commits added to the base since the branch forked. Falls back to the
/// ref's tip tree when HEAD does not resolve (unborn branch, or HEAD
/// itself).
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

/// `Some(warning)` when no merge-base can be computed, so the diff will
/// compare against the branch tip and include unrelated base changes.
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

/// Full diff for one file, against the merge-base of HEAD and the base.
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

/// Raw old and new contents of one file plus its status, for the endpoint
/// the web client renders itself via `@pierre/diffs`. No addition or
/// deletion counts: the renderer derives them and the file list already
/// carries them.
#[derive(Debug, Clone)]
pub struct FileContents {
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub status: FileStatus,
    pub old_content: String,
    pub new_content: String,
    /// Unified diff of old against new, computed here so the client parses
    /// text rather than re-running the diff on its main thread. Empty for a
    /// binary file.
    pub patch: String,
    pub is_binary: bool,
}

/// Base-tree and working-dir contents of one file, plus a unified diff of
/// the two computed here so the client only parses it.
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

/// Base-tree and working-dir contents, the binary flag, and the status.
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

/// Raw bytes of a blob in a tree.
fn get_blob_bytes(repo: &git2::Repository, tree: &git2::Tree, path: &Path) -> Option<Vec<u8>> {
    let entry = tree.get_path(path).ok()?;
    let obj = entry.to_object(repo).ok()?;
    let blob = obj.as_blob()?;
    Some(blob.content().to_vec())
}

/// Null-byte heuristic for binary content.
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

/// Full contents of an unchanged, agent-cited file, for the structured-view
/// viewer's full-file fallback (#1810).
///
/// Membership is gated on the path being a tracked blob in `HEAD`, which
/// excludes `.git/` internals, gitignored secrets, and submodule gitlinks.
/// The contents come from the working directory via the caller's already
/// containment-checked `canonical_path`, so what renders is what the agent
/// saw. `Ok(None)` for anything else, so the caller answers `404` without
/// disclosing whether an untracked file exists.
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

/// Working-directory contents of a file.
pub fn get_working_file_content(repo_path: &Path, file_path: &Path) -> Result<String> {
    let repo = super::open_repo_at(repo_path)?;
    let workdir = repo.workdir().ok_or(GitError::NotAGitRepo)?;
    let full_path = workdir.join(file_path);

    std::fs::read_to_string(&full_path).map_err(GitError::IoError)
}

/// Write contents to a file in the working directory.
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

/// Local branch names.
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
    /// Short name, with any remote prefix stripped; `create_worktree`
    /// resolves the remote itself.
    pub name: String,
    /// No matching local branch, so a new worktree fetches and branches
    /// from the remote tip. Surfaced in the UI.
    pub remote_only: bool,
}

/// Local branches plus remote-only ones, so the base-branch picker can
/// offer a teammate's unfetched branch (#948).
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

/// The default branch name, via `GitWorktree::detect_default_branch`.
pub fn get_default_branch(repo_path: &Path) -> Result<String> {
    let git_wt = super::GitWorktree::new(repo_path.to_path_buf())?;
    git_wt.detect_default_branch()
}

/// The default diff base, remote-qualified when the freshest copy lives on
/// a non-default remote and short when the pick is local. Diff resolution
/// uses this rather than `get_default_branch`, so a fork with an `upstream`
/// compares against `upstream/main` instead of a stale local `main`
/// (#1029).
pub fn get_default_base_ref(repo_path: &Path) -> Result<String> {
    let git_wt = super::GitWorktree::new(repo_path.to_path_buf())?;
    Ok(git_wt.detect_default_branch_info()?.qualified_ref())
}

/// Whether `reference` resolves through the same chain
/// `compute_changed_files` uses, so the CLI can validate a ref before
/// persisting it as a per-session diff base (#970).
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

    /// Commit on the current branch.
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

    /// Only tracked blobs are served: an untracked secret (#1810) and `.git`
    /// internals both exist on disk but must 404.
    #[test]
    fn unchanged_file_contents_serves_only_tracked_blobs() {
        let (dir, _repo) = setup_test_repo();
        fs::write(dir.path().join(".env"), "API_KEY=supersecret\n").unwrap();
        for (path, expected) in [
            ("test.txt", Some("line 1\nline 2\nline 3\n")),
            (".env", None),
            (".git/config", None),
        ] {
            let canonical = dir.path().join(path).canonicalize().unwrap();
            let out =
                compute_unchanged_file_contents(dir.path(), Path::new(path), &canonical).unwrap();
            assert_eq!(out.as_ref().map(|c| c.content.as_str()), expected, "{path}");
            assert!(out.is_none_or(|c| !c.is_binary));
        }
    }

    /// Pin a branch name, so `git init`'s default does not decide the test.
    fn ensure_local_branch(repo: &git2::Repository, name: &str, commit: &git2::Commit<'_>) {
        if repo.find_branch(name, git2::BranchType::Local).is_err() {
            repo.branch(name, commit, false).unwrap();
        }
    }

    /// A repo whose main branch has commits the feature branch forked before.
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

        // Only feature_only.txt differs from the merge base; main-only additions
        // and the main-side edit to shared.txt must not show up.
        let files = compute_changed_files(dir.path(), "main").unwrap();
        let paths: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();
        assert_eq!(paths, [Path::new("feature_only.txt")]);

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
    fn check_merge_base_status_warns_only_where_no_base_exists() {
        let (branching, _repo) = setup_branching_repo();
        let (plain, _repo) = setup_test_repo();
        let cases = [
            // A shared ancestor, and HEAD against itself, both diff cleanly.
            (branching.path(), "main", None),
            (plain.path(), "HEAD", None),
            (plain.path(), "nonexistent-branch", Some("not found")),
        ];
        for (repo, base, expected) in cases {
            let status = check_merge_base_status(repo, base);
            match expected {
                None => assert!(status.is_none(), "{base}: {status:?}"),
                Some(text) => assert!(
                    status.as_deref().is_some_and(|s| s.contains(text)),
                    "{base}: {status:?}"
                ),
            }
        }
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
        assert!(!d.is_binary);
        assert!(d.file.additions > 0);
    }

    #[test]
    fn branch_listing_sees_local_branches_and_the_default() {
        let (dir, repo) = setup_test_repo();
        let commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &commit, false).unwrap();

        let branches = list_branches(dir.path()).unwrap();
        assert!(branches.iter().any(|b| b == "feature"), "{branches:?}");
        assert!(branches.contains(&get_default_branch(dir.path()).unwrap()));
    }

    #[test]
    fn validate_ref_accepts_only_a_resolvable_ref() {
        let (dir, repo) = setup_test_repo();
        let head = repo.head().unwrap().shorthand().unwrap().to_string();
        validate_ref(dir.path(), &head).expect("the HEAD branch resolves");
        assert!(validate_ref(dir.path(), "definitely-does-not-exist").is_err());

        let bare = TempDir::new().unwrap();
        assert!(
            validate_ref(bare.path(), "main").is_err(),
            "a path that is not a repo resolves nothing"
        );
    }

    #[test]
    fn test_is_binary_bytes() {
        assert!(!is_binary_bytes(b"hello world"));
        assert!(!is_binary_bytes(b"line 1\nline 2"));
        assert!(is_binary_bytes(b"hello\0world"));
    }

    /// A repo mid-merge with a conflict on `conflicted.txt`, HEAD on `feature`.
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
    fn test_conflicted_file_is_listed_once_and_diffs_with_markers() {
        let (dir, _repo) = setup_conflict_repo();
        let files = compute_changed_files(dir.path(), "main").unwrap();
        let entries: Vec<_> = files
            .iter()
            .filter(|f| f.path == Path::new("conflicted.txt"))
            .map(|f| f.status)
            .collect();
        assert_eq!(entries, [FileStatus::Conflicted]);

        let diff = compute_file_diff(dir.path(), Path::new("conflicted.txt"), "main", 3).unwrap();
        assert_eq!(diff.file.status, FileStatus::Conflicted);
        let all_content: String = diff
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| l.content.as_str())
            .collect();
        assert!(all_content.contains("<<<<<<<") && all_content.contains(">>>>>>>"));
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

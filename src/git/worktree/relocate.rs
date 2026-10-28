//! Moving a worktree, including the manual path for worktrees with submodules
//! and the classification of moves that timed out.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{
    command_failed, path_str, run_bounded, GitWorktree, MUTATION_OBSERVATION_TIMEOUT,
    WORKTREE_MUTATION_TIMEOUT,
};
use crate::git::command::run_git_with_timeout;
use crate::git::error::{GitError, Result};

/// Where a move left the worktree. `Err` from a move means the checkout is
/// still at the source.
#[derive(Debug, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved,
    /// The checkout is at the destination but git is broken there; the
    /// message says how to repair it.
    MovedDegraded(String),
}

impl MoveOutcome {
    pub fn into_warning(self) -> Option<String> {
        match self {
            Self::Moved => None,
            Self::MovedDegraded(message) => Some(message),
        }
    }
}

/// What a mutation that timed out actually did, judged by observing both sides.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TimedMutationOutcome {
    Applied,
    Unchanged,
    Indeterminate,
}

fn classify_timed_mutation(old_exists: bool, new_exists: bool) -> TimedMutationOutcome {
    match (old_exists, new_exists) {
        (false, true) => TimedMutationOutcome::Applied,
        (true, false) => TimedMutationOutcome::Unchanged,
        _ => TimedMutationOutcome::Indeterminate,
    }
}

pub(super) fn classify_ref_observations(
    old_exists: Option<bool>,
    new_exists: Option<bool>,
) -> TimedMutationOutcome {
    match (old_exists, new_exists) {
        (Some(old_exists), Some(new_exists)) => classify_timed_mutation(old_exists, new_exists),
        _ => TimedMutationOutcome::Indeterminate,
    }
}

/// Classifies a timed-out move from `git worktree list --porcelain -z`.
/// Paths compare byte-for-byte and any malformed listing is `None`.
fn classify_worktree_move_listing(
    output: &[u8],
    from: &Path,
    to: &Path,
) -> Option<TimedMutationOutcome> {
    let from = from.as_os_str().as_encoded_bytes();
    let to = to.as_os_str().as_encoded_bytes();
    let mut from_registered = false;
    let mut to_registered = false;
    let mut at_record_start = true;
    let mut fields = output.split(|byte| *byte == 0);

    while let Some(field) = fields.next() {
        if field.is_empty() {
            if at_record_start {
                return fields
                    .all(|remaining| remaining.is_empty())
                    .then(|| classify_timed_mutation(from_registered, to_registered));
            }
            at_record_start = true;
            continue;
        }
        if at_record_start {
            let path = field.strip_prefix(b"worktree ")?;
            if path.is_empty() {
                return None;
            }
            from_registered |= path == from;
            to_registered |= path == to;
            at_record_start = false;
        }
    }
    None
}

/// Git refuses to move a worktree whose admin dir holds `modules/`, whether or
/// not the submodules are populated.
fn worktree_has_submodule_admin_state(path: &Path) -> bool {
    crate::git::cleanup::read_linked_worktree_gitdir(path)
        .is_some_and(|gitdir| gitdir.join("modules").is_dir())
}

/// A populated submodule checkout and the git dir its pointer resolves to.
/// That git dir stays under the main repo, so recording it before the rename
/// makes the pointers repairable afterwards.
struct SubmoduleCheckout {
    /// Relative to the worktree root, parents before children.
    relative: PathBuf,
    /// Canonical.
    gitdir: PathBuf,
}

fn path_from_git_bytes(bytes: &[u8], description: &str) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let _ = description;
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        let path = String::from_utf8(bytes.to_vec()).map_err(|error| {
            GitError::WorktreeCommandFailed(format!(
                "could not decode {description} as UTF-8: {error}"
            ))
        })?;
        Ok(PathBuf::from(path))
    }
}

fn parse_nul_delimited_paths(output: &[u8]) -> Result<Vec<PathBuf>> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if !output.ends_with(&[0]) {
        return Err(GitError::WorktreeCommandFailed(
            "git submodule foreach returned an unterminated path list".to_string(),
        ));
    }

    output[..output.len() - 1]
        .split(|byte| *byte == 0)
        .map(|path| {
            if path.is_empty() {
                return Err(GitError::WorktreeCommandFailed(
                    "git submodule foreach returned an empty path".to_string(),
                ));
            }
            path_from_git_bytes(path, "submodule path")
        })
        .collect()
}

fn submodule_gitdir(checkout: &Path) -> Result<PathBuf> {
    let output = run_bounded(
        checkout,
        ["rev-parse", "--absolute-git-dir"],
        "git rev-parse --absolute-git-dir",
    )?;
    let gitdir = output.stdout.strip_suffix(b"\n").ok_or_else(|| {
        GitError::WorktreeCommandFailed(
            "git rev-parse --absolute-git-dir returned an unterminated path".to_string(),
        )
    })?;
    let gitdir = path_from_git_bytes(gitdir, "submodule git directory")?;
    if !gitdir.is_absolute() {
        return Err(GitError::WorktreeCommandFailed(
            "git rev-parse --absolute-git-dir returned a relative path".to_string(),
        ));
    }
    gitdir.canonicalize().map_err(|error| {
        GitError::WorktreeCommandFailed(format!(
            "could not resolve submodule git directory: {error}"
        ))
    })
}

/// Git lists canonical paths; resolve while the source and destination parent exist.
fn canonicalize_move_endpoint(path: &Path) -> PathBuf {
    path.canonicalize()
        .ok()
        .or_else(|| {
            let parent = path.parent()?.canonicalize().ok()?;
            Some(parent.join(path.file_name()?))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

fn observe_worktree_move(repo_path: &Path, from: &Path, to: &Path) -> TimedMutationOutcome {
    match run_git_with_timeout(
        repo_path,
        ["worktree", "list", "--porcelain", "-z"],
        MUTATION_OBSERVATION_TIMEOUT,
    ) {
        Ok(Some(output)) if output.status.success() => {
            classify_worktree_move_listing(&output.stdout, from, to)
                .unwrap_or(TimedMutationOutcome::Indeterminate)
        }
        _ => TimedMutationOutcome::Indeterminate,
    }
}

impl GitWorktree {
    fn relock(&self, to: &Path) {
        if let Err(e) = self.lock_worktree(to) {
            tracing::warn!(target: "git.worktree",
                to = %to.display(),
                error = %e,
                "move_worktree: could not re-lock worktree at new path"
            );
        }
    }

    /// Move a worktree with `git worktree move` so git's bookkeeping follows,
    /// unlocking before and re-locking after. Worktrees with submodules take
    /// `relocate_worktree_with_submodules`.
    pub fn move_worktree(&self, from: &Path, to: &Path) -> Result<MoveOutcome> {
        if !from.exists() {
            return Err(GitError::WorktreeNotFound(from.to_path_buf()));
        }
        if to.exists() {
            return Err(GitError::WorktreeAlreadyExists(to.to_path_buf()));
        }
        if worktree_has_submodule_admin_state(from) {
            return self.relocate_worktree_with_submodules(from, to);
        }
        let observation_from = canonicalize_move_endpoint(from);
        let observation_to = canonicalize_move_endpoint(to);
        let (from_str, to_str) = (path_str(from)?, path_str(to)?);

        tracing::info!(target: "git.worktree",
            from = %from.display(),
            to = %to.display(),
            "move_worktree: invoking `git worktree move`"
        );
        self.unlock_worktree(from);
        // A relative pointer left stale by an earlier move cannot be followed.
        if crate::git::cleanup::read_linked_worktree_gitdir(from).is_some_and(|g| !g.exists()) {
            if let Err(e) = self.repair_worktree(from) {
                tracing::warn!(target: "git.worktree",
                    from = %from.display(),
                    error = %e,
                    "move_worktree: could not repair a stale .git pointer"
                );
            }
        }
        let original_pointer = Self::convert_git_file_to_absolute(from).unwrap_or_else(|e| {
            tracing::warn!(target: "git.worktree",
                from = %from.display(),
                error = %e,
                "move_worktree: could not make the .git pointer absolute"
            );
            None
        });
        let restore_pointer = || {
            if let Some(original) = &original_pointer {
                let _ = std::fs::write(from.join(".git"), original);
            }
        };
        let moved = run_git_with_timeout(
            &self.repo_path,
            ["worktree", "move", from_str, to_str],
            WORKTREE_MUTATION_TIMEOUT,
        );
        let Some(output) = moved.inspect_err(|_| restore_pointer())? else {
            let secs = WORKTREE_MUTATION_TIMEOUT.as_secs();
            match observe_worktree_move(&self.repo_path, &observation_from, &observation_to) {
                TimedMutationOutcome::Applied => {
                    self.relock(to);
                    tracing::warn!(target: "git.worktree", to = %to.display(), "timed-out worktree move completed before termination");
                    return Ok(MoveOutcome::Moved);
                }
                TimedMutationOutcome::Unchanged
                    if matches!(from.try_exists(), Ok(true))
                        && matches!(to.try_exists(), Ok(false)) =>
                {
                    restore_pointer();
                    let _ = self.lock_worktree(from);
                    return Err(GitError::WorktreeCommandFailed(format!(
                        "`git worktree move` timed out after {secs}s without moving"
                    )));
                }
                TimedMutationOutcome::Unchanged | TimedMutationOutcome::Indeterminate => {
                    let _ = self.lock_worktree(from);
                    let _ = self.lock_worktree(to);
                    return Err(GitError::WorktreeCommandFailed(format!(
                        "`git worktree move` timed out after {secs}s and its final location is indeterminate"
                    )));
                }
            }
        };
        if !output.status.success() {
            let error = command_failed(&output);
            // Backstop for a submodule layout the admin-dir probe missed.
            if let GitError::WorktreeCommandFailed(stderr) = &error {
                if crate::git::cleanup::is_submodule_blocker(stderr) {
                    return self.relocate_worktree_with_submodules(from, to);
                }
            }
            restore_pointer();
            let _ = self.lock_worktree(from);
            return Err(error);
        }
        self.relock(to);
        Ok(MoveOutcome::Moved)
    }

    /// Rename a worktree with submodules and repair git's pointers, keeping
    /// uncommitted and untracked submodule work (`submodule deinit` would
    /// refuse or discard it).
    ///
    /// An `Err` leaves the checkout at `from`. Once the checkout is at `to`
    /// the result is `Ok`, `MovedDegraded` when a pointer could not be fixed.
    fn relocate_worktree_with_submodules(&self, from: &Path, to: &Path) -> Result<MoveOutcome> {
        tracing::info!(target: "git.worktree",
            from = %from.display(),
            to = %to.display(),
            "move_worktree: relocating a worktree with submodules by hand"
        );
        // Recorded while the pointers still resolve.
        let checkouts = match Self::populated_submodule_checkouts(from) {
            Ok(checkouts) => checkouts,
            Err(e) => {
                // `move_worktree`'s backstop has already unlocked the source.
                let _ = self.lock_worktree(from);
                return Err(e);
            }
        };
        self.unlock_worktree(from);

        if let Err(e) = std::fs::rename(from, to) {
            let _ = self.lock_worktree(from);
            return Err(e.into());
        }
        let mut problems = Vec::new();
        if let Err(e) = self.repair_worktree(to) {
            if let Err(rollback) = std::fs::rename(to, from) {
                tracing::error!(target: "git.worktree",
                    to = %to.display(),
                    error = %e,
                    "move_worktree: could not repair or move back; the checkout is stranded at the new path"
                );
                problems.push(format!(
                    "`git worktree repair` failed ({e}) and moving back failed ({rollback}); \
                     run `git worktree repair {}` from the main repository",
                    to.display()
                ));
            } else {
                let _ = self.repair_worktree(from);
                let _ = self.lock_worktree(from);
                return Err(e);
            }
        }

        if let Err(e) = Self::convert_git_file_to_relative(to) {
            problems.push(format!(
                "could not restore the relative .git pointer ({e}); \
                 run `git worktree repair {}` from the main repository",
                to.display()
            ));
        }
        let mut submodule_failed = false;
        for checkout in &checkouts {
            if let Err(e) = Self::repoint_submodule(to, checkout) {
                submodule_failed = true;
                problems.push(format!(
                    "submodule {} is not repointed: {e}",
                    checkout.relative.display()
                ));
            }
        }
        self.relock(to);
        if problems.is_empty() {
            return Ok(MoveOutcome::Moved);
        }
        tracing::error!(target: "git.worktree",
            to = %to.display(),
            problems = ?problems,
            "move_worktree: relocated the worktree but left it degraded"
        );
        let mut message = problems.join("; ");
        if submodule_failed {
            message.push_str(&format!(
                "; run `git submodule update --init --recursive` in {} to repair",
                to.display()
            ));
        }
        Ok(MoveOutcome::MovedDegraded(message))
    }

    /// `git worktree repair`, which fixes both the checkout's `.git` pointer and
    /// the admin `gitdir` file, even when a relative pointer broke in the move.
    fn repair_worktree(&self, path: &Path) -> Result<()> {
        run_bounded(
            &self.repo_path,
            ["worktree", "repair", path_str(path)?],
            "git worktree repair",
        )
        .map(drop)
    }

    /// `submodule foreach` visits only populated checkouts and `$displaypath`
    /// is cumulative, so nested submodules come back worktree-relative.
    fn populated_submodule_checkouts(worktree_path: &Path) -> Result<Vec<SubmoduleCheckout>> {
        let output = run_bounded(
            worktree_path,
            [
                "submodule",
                "foreach",
                "--recursive",
                "--quiet",
                r#"printf '%s\0' "$displaypath""#,
            ],
            "git submodule foreach",
        )?;
        let paths = parse_nul_delimited_paths(&output.stdout)?;
        let mut checkouts = Vec::with_capacity(paths.len());
        for relative in paths {
            let checkout = worktree_path.join(&relative);
            let gitfile = checkout.join(".git");
            let metadata = std::fs::metadata(&gitfile).map_err(|error| {
                GitError::WorktreeCommandFailed(format!(
                    "could not inspect submodule git metadata at {}: {error}",
                    gitfile.display()
                ))
            })?;
            // A `.git` directory moves with the checkout; no pointer to fix.
            if metadata.is_dir() {
                continue;
            }
            if !metadata.is_file() {
                return Err(GitError::WorktreeCommandFailed(format!(
                    "submodule git metadata at {} is not a file or directory",
                    gitfile.display()
                )));
            }
            checkouts.push(SubmoduleCheckout {
                relative,
                gitdir: submodule_gitdir(&checkout)?,
            });
        }
        Ok(checkouts)
    }

    /// Point a moved submodule checkout and its git dir back at each other,
    /// both relative. A stale `core.worktree` alone makes every command in the
    /// parent fail with `cannot chdir`, and is written with `--file` because
    /// git cannot enter the checkout until it is fixed.
    fn repoint_submodule(worktree_path: &Path, checkout: &SubmoduleCheckout) -> Result<()> {
        let path = worktree_path.join(&checkout.relative);
        let canonical = path.canonicalize()?;
        let gitdir = &checkout.gitdir;

        let pointer = Self::diff_paths(gitdir, &canonical).unwrap_or_else(|| gitdir.clone());
        let mut gitfile = b"gitdir: ".to_vec();
        gitfile.extend_from_slice(pointer.as_os_str().as_encoded_bytes());
        gitfile.push(b'\n');
        std::fs::write(path.join(".git"), gitfile)?;

        let core_worktree =
            Self::diff_paths(&canonical, gitdir).unwrap_or_else(|| canonical.clone());
        let config = gitdir.join("config");
        run_bounded(
            worktree_path,
            [
                OsStr::new("config"),
                OsStr::new("--file"),
                config.as_os_str(),
                OsStr::new("core.worktree"),
                core_worktree.as_os_str(),
            ],
            "git config core.worktree",
        )
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{init_repo, repo_with_nested_submodules, run_git};
    use tempfile::TempDir;

    #[test]
    fn timed_mutation_classification_fails_closed() {
        use TimedMutationOutcome::*;
        for (old, new, want) in [
            (Some(false), Some(true), Applied),
            (Some(true), Some(false), Unchanged),
            (Some(false), Some(false), Indeterminate),
            (Some(true), Some(true), Indeterminate),
            (None, Some(true), Indeterminate),
            (Some(false), None, Indeterminate),
            (None, None, Indeterminate),
        ] {
            assert_eq!(classify_ref_observations(old, new), want);
        }

        let from = Path::new("/repo/old");
        let to = Path::new("/repo/new");
        let cases: &[(&[u8], Option<TimedMutationOutcome>)] = &[
            (
                b"worktree /repo/old\0HEAD abc\0branch refs/heads/topic\0\0",
                Some(Unchanged),
            ),
            (
                b"worktree /repo/new\0HEAD abc\0branch refs/heads/topic\0\0",
                Some(Applied),
            ),
            (
                b"worktree /repo/old\0HEAD abc\0\0worktree /repo/new\0HEAD abc\0\0",
                Some(Indeterminate),
            ),
            (b"worktree /repo/other\0HEAD abc\0\0", Some(Indeterminate)),
            (
                b"worktree /repo/old-suffix\0HEAD abc\0\0",
                Some(Indeterminate),
            ),
            (b"HEAD abc\0worktree /repo/new\0\0", None),
            (b"worktree /repo/new\0HEAD abc\0", None),
        ];
        for (listing, want) in cases {
            assert_eq!(
                classify_worktree_move_listing(listing, from, to).as_ref(),
                want.as_ref(),
                "{:?}",
                String::from_utf8_lossy(listing)
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn move_observation_canonicalizes_symlinked_parents() {
        let dir = TempDir::new().unwrap();
        let real_parent = dir.path().join("real");
        std::fs::create_dir(&real_parent).unwrap();
        let linked_parent = dir.path().join("linked");
        std::os::unix::fs::symlink(&real_parent, &linked_parent).unwrap();
        // macOS temp dirs sit under the `/var` symlink too.
        let real_parent = real_parent.canonicalize().unwrap();
        std::fs::create_dir(linked_parent.join("old")).unwrap();

        let observed_from = canonicalize_move_endpoint(&linked_parent.join("old"));
        let observed_to = canonicalize_move_endpoint(&linked_parent.join("new"));
        assert_eq!(observed_from, real_parent.join("old"));
        assert_eq!(observed_to, real_parent.join("new"));

        let mut listing = b"worktree ".to_vec();
        listing.extend_from_slice(observed_to.as_os_str().as_encoded_bytes());
        listing.extend_from_slice(b"\0HEAD abc\0branch refs/heads/topic\0\0");
        assert_eq!(
            classify_worktree_move_listing(&listing, &observed_from, &observed_to),
            Some(TimedMutationOutcome::Applied),
        );
    }

    /// #3695: `git worktree move` refuses worktrees with submodules; the move
    /// must keep uncommitted and untracked work at every level.
    #[test]
    #[serial_test::serial]
    fn move_worktree_relocates_nested_submodules_and_keeps_local_changes() {
        // Ambient git config (e.g. a global excludesFile ignoring `.claude`)
        // breaks the submodule fixtures, so anchor HOME.
        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        let dirs = repo_with_nested_submodules("test-move", Path::new(".claude"));
        let repo_path = dirs[0].path().to_path_buf();
        let git_wt = GitWorktree::new(repo_path.clone())
            .unwrap()
            .allow_submodule_file_transport();
        let parent = TempDir::new().unwrap();
        let from = parent.path().join("source");
        git_wt
            .create_worktree("test-move", &from, false, None)
            .unwrap();
        assert!(worktree_has_submodule_admin_state(&from));

        let edits = [
            (".claude/mid.md", "edited outer\n"),
            (".claude/scratch.txt", "outer scratch\n"),
            (".claude/nested/inner.md", "edited nested\n"),
            (".claude/nested/scratch.txt", "nested scratch\n"),
        ];
        assert!(from.join(".claude/nested/inner.md").is_file());
        for (path, content) in edits {
            std::fs::write(from.join(path), content).unwrap();
        }

        // One level deeper, so every relative pointer changes.
        let to = parent.path().join("nested-dest/moved");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        git_wt.move_worktree(&from, &to).unwrap();

        assert!(!from.exists());
        for (path, content) in edits {
            assert_eq!(
                std::fs::read_to_string(to.join(path)).unwrap(),
                content,
                "{path}"
            );
        }
        run_git(&to, &["status", "--short"]);
        run_git(&to, &["submodule", "status", "--recursive"]);
        run_git(&to.join(".claude"), &["log", "--oneline", "-1"]);
        run_git(&to.join(".claude/nested"), &["log", "--oneline", "-1"]);

        let listed = run_git(&repo_path, &["worktree", "list", "--porcelain"]);
        let expected = to.canonicalize().unwrap();
        assert!(
            listed
                .lines()
                .any(|l| l.strip_prefix("worktree ").map(Path::new) == Some(expected.as_path())),
            "{listed}"
        );
        assert!(listed.contains("locked"), "{listed}");

        let (plain, _repo) = init_repo();
        let plain_wt = parent.path().join("plain");
        GitWorktree::new(plain.path().to_path_buf())
            .unwrap()
            .create_worktree("plain-branch", &plain_wt, true, None)
            .unwrap();
        assert!(!worktree_has_submodule_admin_state(&plain_wt));
    }

    #[test]
    fn parse_nul_delimited_submodule_paths_without_normalizing() {
        let paths = parse_nul_delimited_paths(b" leading \ntrailing \0nested\0").unwrap();
        assert_eq!(
            paths,
            [
                PathBuf::from(" leading \ntrailing "),
                PathBuf::from("nested")
            ]
        );
        assert!(parse_nul_delimited_paths(b"unterminated").is_err());
        assert!(parse_nul_delimited_paths(b"path\0\0").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn parse_nul_delimited_submodule_paths_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let paths = parse_nul_delimited_paths(b" path\xff\n\0").unwrap();
        assert_eq!(paths[0].as_os_str().as_bytes(), b" path\xff\n");
    }

    #[test]
    #[cfg(not(unix))]
    fn parse_nul_delimited_submodule_paths_rejects_unrepresentable_bytes() {
        assert!(parse_nul_delimited_paths(b" leading \xff\n\0").is_err());
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn move_worktree_preserves_leading_and_trailing_whitespace_in_submodule_paths() {
        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        let outer_path = " spaced ";
        let dirs = repo_with_nested_submodules("test-move-whitespace", Path::new(outer_path));
        let repo_path = dirs[0].path().to_path_buf();
        let git_wt = GitWorktree::new(repo_path.clone())
            .unwrap()
            .allow_submodule_file_transport();
        let parent = TempDir::new().unwrap();
        let from = parent.path().join("source");
        git_wt
            .create_worktree("test-move-whitespace", &from, false, None)
            .unwrap();

        let submodule_path = Path::new(outer_path);
        assert!(from.join(submodule_path).join("mid.md").is_file());
        assert!(from.join(submodule_path).join("nested/inner.md").is_file());

        let to = parent.path().join("nested-dest/moved");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        git_wt.move_worktree(&from, &to).unwrap();

        assert!(!from.exists());
        assert!(to.join(submodule_path).join("mid.md").is_file());
        assert!(to.join(submodule_path).join("nested/inner.md").is_file());
        assert!(run_git(&to, &["status", "--short"]).is_empty());
        let submodule_status = run_git(&to, &["submodule", "status", "--recursive"]);
        assert_eq!(submodule_status.lines().count(), 2, "{submodule_status:?}");
        assert!(
            submodule_status.lines().all(|line| line.starts_with(' ')),
            "{submodule_status:?}"
        );
        assert!(run_git(&to.join(submodule_path), &["status", "--short"]).is_empty());
        assert!(run_git(
            &to.join(submodule_path).join("nested"),
            &["status", "--short"]
        )
        .is_empty());
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn move_worktree_relocates_non_utf8_submodule_paths() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        // EILSEQ and EINVAL can signal a filesystem rejecting non-UTF-8 names.
        fn refused(result: std::io::Result<()>) -> bool {
            match result {
                Ok(()) => false,
                Err(err)
                    if matches!(err.raw_os_error(), Some(libc::EILSEQ) | Some(libc::EINVAL)) =>
                {
                    true
                }
                Err(err) => panic!("non-UTF-8 name probe failed for another reason: {err}"),
            }
        }

        let probe = TempDir::new().unwrap();
        let named = |bytes: &[u8]| probe.path().join(OsStr::from_bytes(bytes));
        let control = named(b"probe-utf8");
        std::fs::create_dir(&control).expect("probe dir must be writable");
        let created = named(b"probe-\xff");
        let renamed = named(b"probe-renamed-\xff");
        if refused(std::fs::create_dir(&created)) || refused(std::fs::rename(&control, renamed)) {
            eprintln!(
                "skipping move_worktree_relocates_non_utf8_submodule_paths: {} refuses non-UTF-8 names",
                probe.path().display()
            );
            return;
        }

        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        let outer_path = PathBuf::from(std::ffi::OsString::from_vec(b"module-\xff".to_vec()));
        let dirs = repo_with_nested_submodules("test-move-non-utf8", &outer_path);
        let repo_path = dirs[0].path().to_path_buf();
        let git_wt = GitWorktree::new(repo_path.clone())
            .unwrap()
            .allow_submodule_file_transport();
        let parent = TempDir::new().unwrap();
        let from = parent.path().join("source");
        git_wt
            .create_worktree("test-move-non-utf8", &from, false, None)
            .unwrap();

        let source_checkout = from.join(&outer_path);
        run_git(&source_checkout, &["submodule", "deinit", "-f", "nested"]);
        let gitdir_output = std::process::Command::new("git")
            .current_dir(&source_checkout)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .unwrap();
        assert!(gitdir_output.status.success());
        let gitdir_bytes = gitdir_output
            .stdout
            .strip_suffix(b"\n")
            .unwrap_or(&gitdir_output.stdout);
        let gitdir = PathBuf::from(std::ffi::OsString::from_vec(gitdir_bytes.to_vec()));
        let non_utf8_gitdir =
            gitdir.with_file_name(std::ffi::OsString::from_vec(b"fixture-\xff".to_vec()));
        std::fs::rename(&gitdir, &non_utf8_gitdir).unwrap();
        let mut source_gitfile = b"gitdir: ".to_vec();
        source_gitfile.extend_from_slice(non_utf8_gitdir.as_os_str().as_encoded_bytes());
        source_gitfile.push(b'\n');
        std::fs::write(source_checkout.join(".git"), source_gitfile).unwrap();

        let to = parent.path().join("nested-dest/moved");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        git_wt.move_worktree(&from, &to).unwrap();

        assert!(!from.exists());
        let checkout = to.join(&outer_path);
        assert!(run_git(&checkout, &["status", "--short"]).is_empty());
        assert!(run_git(&checkout, &["rev-parse", "--show-toplevel"]).contains("moved"));
        run_git(&to, &["submodule", "status", "--recursive"]);

        let gitfile = std::fs::read(checkout.join(".git")).unwrap();
        let pointer = gitfile
            .strip_prefix(b"gitdir: ")
            .unwrap()
            .strip_suffix(b"\n")
            .unwrap();
        assert!(pointer.contains(&0xff), "gitfile pointer: {gitfile:?}");
        let gitdir = checkout
            .join(PathBuf::from(std::ffi::OsString::from_vec(
                pointer.to_vec(),
            )))
            .canonicalize()
            .unwrap();
        assert!(gitdir.join("config").is_file());

        let config = std::process::Command::new("git")
            .current_dir(&to)
            .args(["config", "--file"])
            .arg(gitdir.join("config"))
            .args(["--get", "core.worktree"])
            .output()
            .unwrap();
        assert!(
            config.status.success(),
            "git config failed: {}",
            String::from_utf8_lossy(&config.stderr)
        );
        let actual = config.stdout.strip_suffix(b"\n").unwrap_or(&config.stdout);
        let expected = GitWorktree::diff_paths(&checkout.canonicalize().unwrap(), &gitdir).unwrap();
        assert!(expected.as_os_str().as_bytes().contains(&0xff));
        assert_eq!(actual, expected.as_os_str().as_bytes());
    }

    #[test]
    #[serial_test::serial]
    fn move_worktree_fails_before_rename_when_submodule_inventory_is_invalid() {
        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        let dirs = repo_with_nested_submodules("test-move-invalid-submodule", Path::new(".claude"));
        let repo_path = dirs[0].path().to_path_buf();
        let git_wt = GitWorktree::new(repo_path)
            .unwrap()
            .allow_submodule_file_transport();
        let parent = TempDir::new().unwrap();
        let from = parent.path().join("source");
        git_wt
            .create_worktree("test-move-invalid-submodule", &from, false, None)
            .unwrap();

        let gitfile = from.join(".claude/.git");
        let original_gitfile = std::fs::read(&gitfile).unwrap();
        std::fs::write(&gitfile, b"gitdir: missing\n").unwrap();
        let to = parent.path().join("moved");

        let result = git_wt.move_worktree(&from, &to);
        std::fs::write(&gitfile, original_gitfile).unwrap();

        assert!(result.is_err());
        assert!(from.is_dir());
        assert!(!to.exists());
    }

    /// A submodule gitdir that cannot be rewritten must surface as a degraded
    /// move at the destination, never as a plain success.
    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn move_worktree_reports_degraded_when_submodule_repoint_fails() {
        use std::os::unix::fs::PermissionsExt;

        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: root ignores directory permissions");
            return;
        }
        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        let dirs = repo_with_nested_submodules("test-degraded-move", Path::new(".claude"));
        let git_wt = GitWorktree::new(dirs[0].path().to_path_buf())
            .unwrap()
            .allow_submodule_file_transport();
        let parent = TempDir::new().unwrap();
        let from = parent.path().join("source");
        git_wt
            .create_worktree("test-degraded-move", &from, false, None)
            .unwrap();

        // Git cannot take `config.lock` in a read-only gitdir.
        let gitdir = submodule_gitdir(&from.join(".claude")).unwrap();
        std::fs::set_permissions(&gitdir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let to = parent.path().join("nested-dest/moved");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        let result = git_wt.move_worktree(&from, &to);
        std::fs::set_permissions(&gitdir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let Ok(MoveOutcome::MovedDegraded(message)) = result else {
            panic!("expected a degraded move, got {result:?}");
        };
        assert!(message.contains(".claude"), "{message}");
        assert!(
            message.contains("git submodule update --init --recursive"),
            "{message}"
        );
        assert!(!from.exists());
        assert!(to.is_dir());
        let listed = run_git(&git_wt.repo_path, &["worktree", "list", "--porcelain"]);
        assert!(listed.contains("moved"), "{listed}");
    }

    /// The backstop reaches the relocation after `move_worktree` has already
    /// unlocked the source; an inventory failure must lock it again.
    #[test]
    #[serial_test::serial]
    fn relocation_relocks_source_when_submodule_inventory_fails_after_unlock() {
        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        let dirs = repo_with_nested_submodules("test-relock-inventory", Path::new(".claude"));
        let git_wt = GitWorktree::new(dirs[0].path().to_path_buf())
            .unwrap()
            .allow_submodule_file_transport();
        let parent = TempDir::new().unwrap();
        let from = parent.path().join("source");
        git_wt
            .create_worktree("test-relock-inventory", &from, false, None)
            .unwrap();

        let gitfile = from.join(".claude/.git");
        let original_gitfile = std::fs::read(&gitfile).unwrap();
        std::fs::write(&gitfile, b"gitdir: missing\n").unwrap();
        let to = parent.path().join("moved");

        git_wt.unlock_worktree(&from);
        let result = git_wt.relocate_worktree_with_submodules(&from, &to);
        std::fs::write(&gitfile, original_gitfile).unwrap();

        assert!(result.is_err());
        assert!(!to.exists());
        let listing = run_git(&git_wt.repo_path, &["worktree", "list", "--porcelain"]);
        let record = listing
            .split("\n\n")
            .find(|record| {
                record
                    .lines()
                    .next()
                    .is_some_and(|l| l.ends_with("/source"))
            })
            .unwrap_or_else(|| panic!("{listing}"));
        assert!(
            record.lines().any(|line| line.starts_with("locked")),
            "{listing}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn move_worktree_round_trips_a_relative_gitdir_pointer_through_a_deeper_path() {
        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        let (dir, _repo) = init_repo();
        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        let parent = TempDir::new().unwrap();
        let original = parent.path().join("wt");
        git_wt
            .create_worktree("relative-pointer", &original, true, None)
            .unwrap();
        let pointer = |path: &Path| std::fs::read_to_string(path.join(".git")).unwrap();
        assert!(pointer(&original).starts_with("gitdir: ../"));

        let relative = pointer(&original);
        GitWorktree::convert_git_file_to_absolute(&original).unwrap();
        assert!(Path::new(pointer(&original)["gitdir: ".len()..].trim()).is_absolute());
        std::fs::write(original.join(".git"), relative).unwrap();

        let trash = parent.path().join(".aoe-trash").join("id");
        std::fs::create_dir_all(trash.parent().unwrap()).unwrap();
        git_wt.move_worktree(&original, &trash).unwrap();
        git_wt.move_worktree(&trash, &original).unwrap();
        run_git(&original, &["status", "--porcelain"]);
    }

    #[test]
    #[serial_test::serial]
    fn failed_move_restores_the_relative_gitdir_pointer() {
        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        let (dir, _repo) = init_repo();
        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        let parent = TempDir::new().unwrap();
        let original = parent.path().join("wt");
        git_wt
            .create_worktree("failed-move", &original, true, None)
            .unwrap();
        let before = std::fs::read(original.join(".git")).unwrap();

        let missing_parent = parent.path().join("missing").join("wt");
        assert!(git_wt.move_worktree(&original, &missing_parent).is_err());
        assert_eq!(std::fs::read(original.join(".git")).unwrap(), before);
    }

    #[test]
    #[serial_test::serial]
    fn move_worktree_repairs_a_relative_pointer_broken_by_an_earlier_move() {
        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        let (dir, _repo) = init_repo();
        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        let parent = TempDir::new().unwrap();
        let original = parent.path().join("wt");
        git_wt
            .create_worktree("broken-pointer", &original, true, None)
            .unwrap();

        let trash = parent.path().join(".aoe-trash").join("id");
        std::fs::create_dir_all(trash.parent().unwrap()).unwrap();
        git_wt.unlock_worktree(&original);
        std::fs::rename(&original, &trash).unwrap();

        git_wt.move_worktree(&trash, &original).unwrap();
        run_git(&original, &["status", "--porcelain"]);
    }

    #[test]
    #[cfg(unix)]
    fn convert_git_file_to_absolute_preserves_non_utf8_admin_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let root = TempDir::new().unwrap();
        let admin = root.path().join(OsStr::from_bytes(b"admin-\xff"));
        // EILSEQ and EINVAL signal a filesystem rejecting non-UTF-8 names.
        if let Err(err) = std::fs::create_dir(&admin) {
            assert!(
                matches!(err.raw_os_error(), Some(libc::EILSEQ) | Some(libc::EINVAL)),
                "non-UTF-8 name probe failed for another reason: {err}"
            );
            eprintln!(
                "skipping convert_git_file_to_absolute_preserves_non_utf8_admin_path_bytes: {} refuses non-UTF-8 names",
                root.path().display()
            );
            return;
        }
        std::os::unix::fs::symlink(&admin, root.path().join("link")).unwrap();
        let worktree = root.path().join("wt");
        std::fs::create_dir(&worktree).unwrap();
        let original = b"gitdir: ../link\n".to_vec();
        std::fs::write(worktree.join(".git"), &original).unwrap();

        let returned = GitWorktree::convert_git_file_to_absolute(&worktree).unwrap();

        let mut expected = b"gitdir: ".to_vec();
        expected.extend_from_slice(admin.canonicalize().unwrap().as_os_str().as_bytes());
        expected.push(b'\n');
        assert_eq!(returned, Some(original));
        assert_eq!(std::fs::read(worktree.join(".git")).unwrap(), expected);
        let written = std::fs::read(worktree.join(".git")).unwrap();
        let target = written
            .strip_prefix(b"gitdir: ")
            .and_then(|rest| rest.strip_suffix(b"\n"))
            .unwrap();
        assert!(Path::new(OsStr::from_bytes(target)).exists());
    }

    #[test]
    #[cfg(unix)]
    fn convert_git_file_to_absolute_never_writes_through_a_name_already_in_the_checkout() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("admin")).unwrap();
        let worktree = root.path().join("wt");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), b"gitdir: ../admin\n").unwrap();
        // The name an earlier version used for its temp file, redirecting elsewhere.
        let bystander = root.path().join("bystander");
        std::fs::write(&bystander, b"keep").unwrap();
        std::os::unix::fs::symlink(&bystander, worktree.join(".git.aoe-pointer.tmp")).unwrap();

        let returned = GitWorktree::convert_git_file_to_absolute(&worktree).unwrap();

        assert!(returned.is_some());
        assert_eq!(std::fs::read(&bystander).unwrap(), b"keep");
        assert!(worktree
            .join(".git.aoe-pointer.tmp")
            .symlink_metadata()
            .unwrap()
            .is_symlink());
        let written = std::fs::read(worktree.join(".git")).unwrap();
        assert!(written.starts_with(b"gitdir: /"), "{written:?}");
    }

    #[test]
    #[cfg(unix)]
    fn convert_git_file_to_absolute_leaves_the_original_when_the_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("admin")).unwrap();
        let worktree = root.path().join("wt");
        std::fs::create_dir(&worktree).unwrap();
        let original = b"gitdir: ../admin\n".to_vec();
        std::fs::write(worktree.join(".git"), &original).unwrap();
        // A read-only directory refuses the temp file. Root ignores the mode.
        std::fs::set_permissions(&worktree, std::fs::Permissions::from_mode(0o555)).unwrap();
        let writable = std::fs::File::create(worktree.join("probe")).is_ok();

        let result = GitWorktree::convert_git_file_to_absolute(&worktree);

        std::fs::set_permissions(&worktree, std::fs::Permissions::from_mode(0o755)).unwrap();
        if writable {
            eprintln!("skipping write-failure check: permissions are not enforced");
            return;
        }
        assert!(result.is_err());
        assert_eq!(std::fs::read(worktree.join(".git")).unwrap(), original);
    }
}

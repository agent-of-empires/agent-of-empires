// Shared between `build.rs` (via `include!`) and the regression test in
// `tests/build_version_rerun.rs` (via `#[path] mod`). Keep it free of
// dependencies on the rest of the crate: build scripts compile in isolation.

/// The git files cargo should watch so `AOE_BUILD_VERSION` is recomputed when
/// the checkout's revision or staged state changes: `HEAD` moves on
/// checkout/commit, `index` moves on stage.
///
/// Paths are resolved for the repository rooted at `dir` via
/// `git rev-parse --git-path`, which is correct for both a normal checkout
/// (`.git/HEAD`) and a git worktree, where `.git` is a file pointing at
/// `<main>/.git/worktrees/<name>/` and the literal `.git/HEAD` path does not
/// exist. Hardcoding `.git/HEAD` made cargo treat the missing input as
/// perpetually stale, rebuilding on every invocation inside a worktree
/// (issue #1962). Returns an empty vec when git is unavailable or `dir` is not
/// a git checkout (e.g. a source tarball), leaving the build version pinned to
/// `CARGO_PKG_VERSION` with no spurious rerun trigger.
pub fn git_watch_paths(dir: &std::path::Path) -> Vec<String> {
    ["HEAD", "index"]
        .iter()
        .filter_map(|file| git_path(dir, file))
        .collect()
}

/// Resolve a single per-worktree git file path via `git rev-parse --git-path`,
/// run inside `dir`. `None` when git fails or the output is empty.
pub fn git_path(dir: &std::path::Path, file: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--git-path", file])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

// Shared by `build.rs` (via `include!`) and `tests/build_version_rerun.rs`; keep it dependency-free.

/// Git files to watch so `AOE_BUILD_VERSION` is recomputed on a revision change: `HEAD` and
/// the per-worktree `logs/HEAD`. `index` is not watched because a plain `git status` rewrites it.
/// Only existing paths are returned: cargo treats a missing input as perpetually stale.
pub fn git_watch_paths(dir: &std::path::Path) -> Vec<String> {
    ["HEAD", "logs/HEAD"]
        .iter()
        .filter_map(|file| git_path(dir, file))
        .filter(|path| watched_path_exists(dir, path))
        .collect()
}

fn git_path(dir: &std::path::Path, file: &str) -> Option<String> {
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

/// Relative paths resolve against `dir`; worktrees yield absolute paths.
fn watched_path_exists(dir: &std::path::Path, path: &str) -> bool {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.exists()
    } else {
        dir.join(p).exists()
    }
}

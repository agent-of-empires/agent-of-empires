// Shared by build.rs and tests/integration/build_version_rerun.rs; dependency-free.

/// Watch existing HEAD and reflog paths, never the index or missing inputs.
pub fn git_watch_paths(dir: &std::path::Path) -> Vec<String> {
    ["HEAD", "logs/HEAD"]
        .iter()
        .filter_map(|file| git_path(dir, file))
        .filter(|path| watched_path_exists(dir, path))
        .collect()
}

fn git_path(dir: &std::path::Path, file: &str) -> Option<String> {
    let out = build_git_command(dir)
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

fn build_git_command(dir: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    command
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

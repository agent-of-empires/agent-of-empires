// Git path-resolution shared with the regression test in
// `tests/build_version_rerun.rs` so the test exercises the real watch-path
// logic against a temporary worktree.
include!("build_git_watch.rs");

fn main() {
    check_stale_build_cache();
    emit_build_version();

    #[cfg(feature = "serve")]
    build_frontend();
}

/// Emit `AOE_BUILD_VERSION`, the build identity stamped on each structured view
/// worker record so the daemon can tell whether a surviving worker is
/// running the current binary or an older one (see issue #1754).
///
/// `CARGO_PKG_VERSION` alone is insufficient: it stays constant across
/// many local rebuilds, so a dev who rebuilds and restarts the daemon
/// would silently re-adopt a worker on stale code. We append a git
/// commit identity so dev rebuilds across commits are distinguishable.
///
/// Source order (first hit wins):
///   1. `AOE_BUILD_VERSION` env override (release packaging, reproducible
///      builds, downstream packagers).
///   2. `GITHUB_SHA` (CI builds where `.git` may be a shallow checkout).
///   3. Local `git rev-parse` + a coarse dirty flag.
///   4. `CARGO_PKG_VERSION` alone (source tarball without `.git`).
///
/// The dirty flag is intentionally coarse (a boolean suffix, not a
/// content hash): two different uncommitted edits at the same commit read
/// as equal. This is a respawn gate, not a cryptographic binary hash, and
/// a content hash would force a recompile on every source save.
fn emit_build_version() {
    use std::process::Command;

    // Re-run when the committed revision changes or an override toggles.
    // HEAD moves on checkout/commit; index moves on stage. Resolve the real
    // paths via `git rev-parse --git-path` rather than hardcoding `.git/HEAD`:
    // in a git worktree `.git` is a file pointing at
    // `<main>/.git/worktrees/<name>/`, so the literal `.git/HEAD` path does not
    // exist. Cargo treats a missing `rerun-if-changed` input as perpetually
    // stale, which reran this script (and recompiled the lib + binary that read
    // AOE_BUILD_VERSION) on every build inside a worktree (issue #1962).
    for path in git_watch_paths(std::path::Path::new(".")) {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=AOE_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");

    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();

    let build_version = if let Ok(explicit) = std::env::var("AOE_BUILD_VERSION") {
        explicit
    } else if let Ok(sha) = std::env::var("GITHUB_SHA") {
        let short: String = sha.chars().take(12).collect();
        format!("{pkg_version}+g{short}")
    } else if let Some(short) = git_short_sha(&mut Command::new("git")) {
        let dirty = git_is_dirty(&mut Command::new("git"));
        format!(
            "{pkg_version}+g{short}{}",
            if dirty { "-dirty" } else { "" }
        )
    } else {
        pkg_version
    };

    println!("cargo:rustc-env=AOE_BUILD_VERSION={build_version}");
}

/// Short (12-char) HEAD commit hash, or `None` when git is unavailable or
/// this is not a git checkout (e.g. a source tarball).
fn git_short_sha(cmd: &mut std::process::Command) -> Option<String> {
    let out = cmd
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Coarse working-tree dirty check: any tracked or untracked change makes
/// `git status --porcelain` print at least one line.
fn git_is_dirty(cmd: &mut std::process::Command) -> bool {
    match cmd.args(["status", "--porcelain"]).output() {
        Ok(out) => out.status.success() && !out.stdout.is_empty(),
        Err(_) => false,
    }
}

/// Detect stale build caches by tracking Cargo.lock content hash.
///
/// When Cargo.lock changes (dependency updates, feature additions, branch
/// switches in worktrees), the target/ directory can contain incompatible
/// artifacts that cause cryptic compilation errors like "can't find crate"
/// or "found possibly newer version of crate." This check catches that
/// early with a clear message instead of letting the build fail inscrutably.
fn check_stale_build_cache() {
    use std::path::Path;

    // Re-run this check whenever Cargo.lock changes.
    println!("cargo:rerun-if-changed=Cargo.lock");

    let lockfile = Path::new("Cargo.lock");
    let target_dir = std::env::var("OUT_DIR")
        .ok()
        .and_then(|out| {
            // OUT_DIR is something like target/debug/build/agent-of-empires-xxx/out
            // Walk up to find the target/ root.
            let mut p = Path::new(&out).to_path_buf();
            while p.pop() {
                if p.file_name().is_some_and(|n| n == "target") {
                    return Some(p);
                }
            }
            None
        })
        .unwrap_or_else(|| Path::new("target").to_path_buf());

    let hash_file = target_dir.join(".cargo-lock-hash");

    let Ok(lock_content) = std::fs::read(lockfile) else {
        return; // No Cargo.lock, nothing to check.
    };

    // Simple, fast hash: use the file length + first/last 1KB as a fingerprint.
    // This avoids pulling in a hash crate in build.rs.
    let len = lock_content.len();
    let head: u64 = lock_content[..len.min(1024)]
        .iter()
        .fold(0u64, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let tail: u64 = lock_content[len.saturating_sub(1024)..]
        .iter()
        .fold(0u64, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let current_hash = format!("{:x}{:x}{:x}", len, head, tail);

    if let Ok(stored_hash) = std::fs::read_to_string(&hash_file) {
        if stored_hash.trim() != current_hash {
            println!(
                "cargo:warning=Cargo.lock changed since last build. \
                 If you see strange compilation errors, run `cargo clean`."
            );
        }
    }

    // Always update the stored hash.
    let _ = std::fs::write(&hash_file, &current_hash);
}

#[cfg(feature = "serve")]
fn build_frontend() {
    use std::path::Path;
    use std::process::Command;

    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/index.html");
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/package-lock.json");
    println!("cargo:rerun-if-changed=web/vite.config.ts");
    println!("cargo:rerun-if-changed=web/tsconfig.json");

    // AOE_WEB_DIST allows Nix (and other reproducible build systems) to supply
    // a pre-built frontend directory, bypassing the npm build entirely. When
    // set, the directory is copied to web/dist/ and npm is not invoked.
    //
    // Registered unconditionally so Cargo re-runs build.rs when the var is
    // added or removed, not only when it is already set.
    println!("cargo:rerun-if-env-changed=AOE_WEB_DIST");

    // AOE_COVERAGE=1 instructs Vite to build the web bundle with inline
    // sourcemaps (see web/vite.config.ts) so Playwright can collect raw V8
    // coverage against the embedded frontend and remap it to web/src. The env
    // var is read by the npm child process below; we only need to tell Cargo
    // to invalidate the build script's cache when it toggles.
    println!("cargo:rerun-if-env-changed=AOE_COVERAGE");
    if let Ok(dist_src) = std::env::var("AOE_WEB_DIST") {
        eprintln!("Using pre-built web frontend from AOE_WEB_DIST={dist_src}");
        let src = Path::new(&dist_src);
        let dst = Path::new("web/dist");
        if dst.exists() {
            std::fs::remove_dir_all(dst).expect("Failed to remove existing web/dist");
        }
        // Recursively copy src -> web/dist
        copy_dir(src, dst);
        return;
    }

    // Always rebuild: the rerun-if-changed directives above ensure this
    // function only runs when web source files actually changed.
    // Previously this short-circuited when dist/ existed, which meant
    // source changes were silently ignored.

    eprintln!("Building web frontend...");

    assert!(
        Command::new("npm")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success()),
        "npm is required because the web dashboard is built by default.\n\
         - Install Node.js (https://nodejs.org/)\n\
         - OR set AOE_WEB_DIST to a prebuilt frontend directory\n\
         - OR build a TUI-only binary with `cargo build --no-default-features`"
    );

    maybe_install_web_deps();

    let status = Command::new("npm")
        .args(["run", "build"])
        .current_dir("web")
        .status()
        .expect("Failed to run npm run build");

    if !status.success() {
        panic!("npm run build failed in web/. Run `cd web && npm run build` to debug.");
    }
}

/// Install web dependencies when node_modules is missing OR stale relative to
/// package.json / package-lock.json.
///
/// The previous check only looked for `web/node_modules/.package-lock.json` and
/// skipped install when it existed. That broke a real workflow: after pulling
/// new commits that add a dependency (e.g. `cmdk`), contributors hit cryptic
/// TypeScript errors like "Cannot find module 'cmdk'" because the old
/// node_modules was considered "good enough." This now compares mtimes so any
/// lockfile change triggers a reinstall.
#[cfg(feature = "serve")]
fn maybe_install_web_deps() {
    use std::path::Path;
    use std::process::Command;

    let node_modules_marker = Path::new("web/node_modules/.package-lock.json");
    let package_json = Path::new("web/package.json");
    let package_lock = Path::new("web/package-lock.json");

    let marker_mtime = node_modules_marker
        .metadata()
        .and_then(|m| m.modified())
        .ok();
    let stale = match marker_mtime {
        None => true, // fresh clone, no node_modules yet
        Some(marker) => is_newer_than(package_json, marker) || is_newer_than(package_lock, marker),
    };

    if !stale {
        return;
    }

    // Prefer `npm ci` when a lockfile exists: it is deterministic and cleans
    // up drift from manual edits. Fall back to `npm install` for projects
    // without a lockfile (unusual, but keeps first-time setup working).
    let install_cmd = if package_lock.exists() {
        "ci"
    } else {
        "install"
    };

    // Use `cargo:warning=` so the notice shows in a default `cargo build`
    // (plain eprintln! is suppressed unless the user passes -vv).
    println!(
        "cargo:warning=Installing web dependencies via `npm {install_cmd}` (node_modules is stale or missing)..."
    );

    let status = Command::new("npm")
        .args([install_cmd])
        .current_dir("web")
        .status()
        .unwrap_or_else(|e| panic!("Failed to spawn `npm {install_cmd}` in web/: {e}"));

    if !status.success() {
        panic!(
            "`npm {install_cmd}` failed in web/. \
             Run `cd web && npm {install_cmd}` to see the full error."
        );
    }
}

#[cfg(feature = "serve")]
fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).expect("Failed to create directory");
    for entry in std::fs::read_dir(src).expect("Failed to read directory") {
        let entry = entry.expect("Failed to read entry");
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().expect("Failed to get file type").is_dir() {
            copy_dir(&entry.path(), &dst_path);
        } else {
            std::fs::copy(entry.path(), dst_path).expect("Failed to copy file");
        }
    }
}

#[cfg(feature = "serve")]
fn is_newer_than(path: &std::path::Path, reference: std::time::SystemTime) -> bool {
    match path.metadata().and_then(|m| m.modified()) {
        Ok(mtime) => mtime > reference,
        Err(_) => false, // if the file doesn't exist, it can't be newer
    }
}

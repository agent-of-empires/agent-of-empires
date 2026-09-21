// Shared with `tests/build_version_rerun.rs`.
include!("build_git_watch.rs");

fn main() {
    check_stale_build_cache();
    emit_build_version();
    warn_on_deprecated_serve_feature();

    #[cfg(feature = "web")]
    build_frontend();
}

/// Cargo sets `CARGO_FEATURE_SERVE` only when the `serve` alias itself was selected.
fn warn_on_deprecated_serve_feature() {
    if std::env::var_os("CARGO_FEATURE_SERVE").is_some() {
        println!(
            "cargo:warning=the `serve` feature is deprecated and will be removed after the next \
             release; use `--features web` instead"
        );
    }
}

/// `AOE_BUILD_VERSION` lets the daemon detect a worker running an older binary. First hit
/// wins: env override, `GITHUB_SHA`, local git sha plus a coarse dirty flag, then
/// `CARGO_PKG_VERSION`.
fn emit_build_version() {
    use std::process::Command;

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

fn git_is_dirty(cmd: &mut std::process::Command) -> bool {
    match cmd.args(["status", "--porcelain"]).output() {
        Ok(out) => out.status.success() && !out.stdout.is_empty(),
        Err(_) => false,
    }
}

/// A changed Cargo.lock can leave incompatible artifacts in target/; warn clearly.
fn check_stale_build_cache() {
    use std::path::Path;

    println!("cargo:rerun-if-changed=Cargo.lock");

    let lockfile = Path::new("Cargo.lock");
    let target_dir = std::env::var("OUT_DIR")
        .ok()
        .and_then(|out| {
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

    // Length plus first/last 1KB, avoiding a hash crate in build.rs.
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

    let _ = std::fs::write(&hash_file, &current_hash);
}

#[cfg(feature = "web")]
fn build_frontend() {
    use std::path::Path;
    use std::process::Command;

    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/index.html");
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/package-lock.json");
    println!("cargo:rerun-if-changed=web/vite.config.ts");
    println!("cargo:rerun-if-changed=web/tsconfig.json");

    // Lets Nix supply a pre-built frontend. Registered unconditionally so toggling it reruns.
    println!("cargo:rerun-if-env-changed=AOE_WEB_DIST");

    // Builds the bundle with inline sourcemaps for Playwright coverage.
    println!("cargo:rerun-if-env-changed=AOE_COVERAGE");
    if let Ok(dist_src) = std::env::var("AOE_WEB_DIST") {
        eprintln!("Using pre-built web frontend from AOE_WEB_DIST={dist_src}");
        let src = Path::new(&dist_src);
        let dst = Path::new("web/dist");
        if dst.exists() {
            std::fs::remove_dir_all(dst).expect("Failed to remove existing web/dist");
        }
        copy_dir(src, dst);
        return;
    }

    eprintln!("Building web frontend...");

    assert!(
        Command::new("npm").arg("--version").output().is_ok(),
        "npm is required to build with --features web. Install Node.js: https://nodejs.org/"
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

#[cfg(feature = "web")]
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

    // `npm ci` when a lockfile exists; `npm install` otherwise.
    let install_cmd = if package_lock.exists() {
        "ci"
    } else {
        "install"
    };

    // `cargo:warning=` shows in a default build; eprintln! needs -vv.
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

#[cfg(feature = "web")]
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

#[cfg(feature = "web")]
fn is_newer_than(path: &std::path::Path, reference: std::time::SystemTime) -> bool {
    match path.metadata().and_then(|m| m.modified()) {
        Ok(mtime) => mtime > reference,
        Err(_) => false, // if the file doesn't exist, it can't be newer
    }
}

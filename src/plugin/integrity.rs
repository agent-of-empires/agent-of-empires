//! Deterministic content hash over a plugin's source tree.
//!
//! This is the hash a maintainer pins in `plugins/featured.toml` and an author
//! reproduces with `aoe plugin hash`. It covers the source files only: a
//! downloaded release-binary worker is excluded (it is injected after this is
//! computed, and is pinned separately by the lockfile's `asset_sha256`), so an
//! author's repo checkout and the installed tree hash to the same value.
//!
//! The format is versioned (`HASH_PREFIX`) so the hashed fields can change
//! later (for example folding in the executable bit once #2095 launches
//! workers) without a new value silently colliding with an old pin.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

/// Domain-separation header. Bump the version when the hashed fields change.
const HASH_PREFIX: &[u8] = b"aoe-plugin-tree-hash-v1\0";

/// Reserved directory for a plugin's build output. A `command` runtime's build
/// steps (a Python `.venv`, `node_modules`, compiled artifacts) must write here,
/// never into the source tree. It is excluded from the hash at every level, like
/// `.git`, so a build that mutates the install tree does not change the source
/// hash: an author's `aoe plugin hash` of a clean checkout and the live load-path
/// re-derivation over the built tree produce the same value, keeping a featured
/// pin verifiable after the build runs. Build output is therefore not attested by
/// the pin (build steps already run unsandboxed at the user's trust); a fixed
/// reserved name keeps the exclusion out of attacker control, unlike a
/// manifest-declared list which a tampered manifest could widen to hide source.
pub const BUILD_OUTPUT_DIR: &str = ".aoe-build";

/// Deterministic `sha256:<hex>` over the files in `dir`.
///
/// Files are sorted by their forward-slash relative path; each contributes
/// `file\0<path>\0<len><content>` to the digest, where `<len>` is the content
/// length as 8 little-endian bytes so a path/content boundary is unambiguous.
/// `.git` and the reserved [`BUILD_OUTPUT_DIR`] are skipped at every level (both
/// are stripped from, or generated into, an installed tree). A symlink or a
/// non-UTF-8 path is an error, not a silent skip, so nothing that would be
/// installed escapes the hash. File mode is deliberately excluded for
/// cross-platform determinism (Windows has no executable bit).
pub fn tree_hash(dir: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    hasher.update(HASH_PREFIX);
    for (rel, contents) in &files {
        hasher.update(b"file\0");
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update((contents.len() as u64).to_le_bytes());
        hasher.update(contents);
    }
    Ok(format_digest(&hasher.finalize()))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        // Skip git history at every level. Skip the reserved build-output dir
        // before inspecting the entry's type (a build output like a `.venv`
        // holds symlinks the check below would reject, and is not hashed
        // source), but ONLY at the root: it is a single top-level dir, so a
        // nested `<sub>/.aoe-build` is ordinary source, hashed and
        // symlink-checked like anything else rather than silently dropped.
        if entry.file_name() == ".git" {
            continue;
        }
        if dir == root && entry.file_name() == BUILD_OUTPUT_DIR {
            continue;
        }
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_symlink() {
            bail!(
                "plugin tree contains a symlink ({}); symlinks are not allowed in a hashed tree",
                path.display()
            );
        }
        if file_type.is_dir() {
            collect(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .expect("entry path is under root")
                .to_str()
                .ok_or_else(|| anyhow!("non-UTF-8 path in plugin tree: {}", path.display()))?
                .replace('\\', "/");
            let contents =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            out.push((rel, contents));
        }
    }
    Ok(())
}

fn format_digest(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, contents: &[u8]) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn stable_and_prefixed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "aoe-plugin.toml", b"id = \"a.b\"\n");
        write(dir.path(), "src/main.rs", b"fn main() {}\n");

        let first = tree_hash(dir.path()).unwrap();
        let second = tree_hash(dir.path()).unwrap();
        assert_eq!(first, second, "hash is stable across runs");
        assert!(first.starts_with("sha256:"));
    }

    #[test]
    fn content_change_flips_hash() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", b"one");
        let before = tree_hash(dir.path()).unwrap();
        write(dir.path(), "f.txt", b"two");
        assert_ne!(before, tree_hash(dir.path()).unwrap());
    }

    #[test]
    fn order_independent_but_path_sensitive() {
        // Two files swapping their contents must not hash the same: the path is
        // bound to its content, not just concatenated alongside it.
        let a = tempfile::tempdir().unwrap();
        write(a.path(), "x", b"1");
        write(a.path(), "y", b"2");
        let b = tempfile::tempdir().unwrap();
        write(b.path(), "x", b"2");
        write(b.path(), "y", b"1");
        assert_ne!(tree_hash(a.path()).unwrap(), tree_hash(b.path()).unwrap());
    }

    #[test]
    fn git_dir_is_skipped() {
        let with_git = tempfile::tempdir().unwrap();
        write(with_git.path(), "aoe-plugin.toml", b"x");
        write(with_git.path(), ".git/config", b"junk");
        let without_git = tempfile::tempdir().unwrap();
        write(without_git.path(), "aoe-plugin.toml", b"x");
        assert_eq!(
            tree_hash(with_git.path()).unwrap(),
            tree_hash(without_git.path()).unwrap()
        );
    }

    #[test]
    fn build_output_dir_is_skipped() {
        let with_build = tempfile::tempdir().unwrap();
        write(with_build.path(), "aoe-plugin.toml", b"x");
        write(
            with_build.path(),
            &format!("{BUILD_OUTPUT_DIR}/venv/pyvenv.cfg"),
            b"junk",
        );
        let without_build = tempfile::tempdir().unwrap();
        write(without_build.path(), "aoe-plugin.toml", b"x");
        assert_eq!(
            tree_hash(with_build.path()).unwrap(),
            tree_hash(without_build.path()).unwrap()
        );
    }

    #[test]
    fn nested_build_output_dir_is_hashed_not_skipped() {
        // The reserved dir is excluded only at the root. A nested
        // `sub/.aoe-build` is ordinary source: it must change the hash, so it
        // cannot be used to hide files from the pin.
        let without = tempfile::tempdir().unwrap();
        write(without.path(), "aoe-plugin.toml", b"x");
        let with_nested = tempfile::tempdir().unwrap();
        write(with_nested.path(), "aoe-plugin.toml", b"x");
        write(with_nested.path(), "sub/.aoe-build/hidden", b"payload");
        assert_ne!(
            tree_hash(without.path()).unwrap(),
            tree_hash(with_nested.path()).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn nested_build_output_symlink_is_rejected() {
        // A symlink under a nested (non-root) `.aoe-build` is still rejected:
        // only the root build-output dir escapes the symlink check.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "aoe-plugin.toml", b"x");
        let nested = dir.path().join("sub").join(".aoe-build");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink("real", nested.join("link")).unwrap();
        let err = tree_hash(dir.path()).unwrap_err().to_string();
        assert!(err.contains("symlink"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_build_output_is_not_rejected() {
        // A build like a Python venv places a symlink under the build-output
        // dir; the hash must skip it rather than hard-error, so a built tree
        // still re-derives the source hash.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "aoe-plugin.toml", b"x");
        let build = dir.path().join(BUILD_OUTPUT_DIR).join("bin");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink("real", build.join("python3")).unwrap();
        assert!(tree_hash(dir.path()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real", b"x");
        std::os::unix::fs::symlink("real", dir.path().join("link")).unwrap();
        let err = tree_hash(dir.path()).unwrap_err().to_string();
        assert!(err.contains("symlink"), "got: {err}");
    }
}

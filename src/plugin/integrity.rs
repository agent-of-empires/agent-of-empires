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

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

/// Domain-separation header. Bump the version when the hashed fields change.
const HASH_PREFIX: &[u8] = b"aoe-plugin-tree-hash-v1\0";

/// Deterministic `sha256:<hex>` over the files in `dir`.
///
/// Files are sorted by their forward-slash relative path; each contributes
/// `file\0<path>\0<len><content>` to the digest, where `<len>` is the content
/// length as 8 little-endian bytes so a path/content boundary is unambiguous.
/// `.git` is skipped at every level (it is stripped from an installed tree). A
/// symlink or a non-UTF-8 path is an error, not a silent skip, so nothing that
/// would be installed escapes the hash. File mode is deliberately excluded for
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

/// [`tree_hash`] memoized for the registry load path, where validation re-hashes
/// every featured-and-installed plugin on each rebuild. The result is keyed by
/// plugin dir and guarded by a cheap `(path, len, mtime)` signature: a hit
/// returns the previously computed hash only when the on-disk tree is unchanged,
/// otherwise it falls back to a full re-hash.
///
/// This is an in-memory optimization, not a root of trust. The cache lives only
/// for the process, is never persisted, and is never read by anything but this
/// function; the value it returns is always either a fresh `tree_hash` or the
/// exact hash a fresh `tree_hash` produced for the same tree. A signature is
/// content-independent and therefore forgeable (mtime can be reset), which is
/// why a persisted cache could not gate the verified decision; keeping it
/// in-process sidesteps that, since an attacker who can write the tree and forge
/// mtimes mid-process could equally just install a vetted tree.
//
// ponytail: in-process memo only. A persistent cross-launch cache would need a
// tamper-evident key the content-integrity decision cannot safely trust, so it
// is deliberately not built; revisit only if a benchmark on a large featured set
// shows the per-load re-hash actually hurts.
pub fn cached_tree_hash(dir: &Path) -> Result<String> {
    let Some(sig) = tree_signature(dir) else {
        return tree_hash(dir);
    };
    let key = dir.to_path_buf();
    if let Some((cached_sig, hash)) = cache().read().ok().and_then(|c| c.get(&key).cloned()) {
        if cached_sig == sig {
            return Ok(hash);
        }
    }
    let hash = tree_hash(dir)?;
    if let Ok(mut c) = cache().write() {
        c.insert(key, (sig, hash.clone()));
    }
    Ok(hash)
}

fn cache() -> &'static RwLock<HashMap<PathBuf, (u64, String)>> {
    static CACHE: OnceLock<RwLock<HashMap<PathBuf, (u64, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// A cheap, content-independent signature of `dir`: every file's relative path,
/// length, and mtime folded together. Returns `None` on any IO error, a
/// symlink, or a non-UTF-8 path so the caller re-hashes directly (those are the
/// cases [`tree_hash`] treats as hard errors anyway).
fn tree_signature(dir: &Path) -> Option<u64> {
    let mut files: Vec<(String, u64, u128)> = Vec::new();
    sig_collect(dir, dir, &mut files).ok()?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (rel, len, mtime) in &files {
        rel.hash(&mut hasher);
        len.hash(&mut hasher);
        mtime.hash(&mut hasher);
    }
    Some(hasher.finish())
}

fn sig_collect(root: &Path, dir: &Path, out: &mut Vec<(String, u64, u128)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_symlink() {
            bail!("symlink in plugin tree; cannot signature, will re-hash");
        }
        if file_type.is_dir() {
            sig_collect(root, &path, out)?;
        } else {
            let metadata = entry.metadata()?;
            let mtime = metadata
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map_err(|e| anyhow!("mtime before unix epoch: {e}"))?
                .as_nanos();
            let rel = path
                .strip_prefix(root)
                .expect("entry path is under root")
                .to_str()
                .ok_or_else(|| anyhow!("non-UTF-8 path in plugin tree: {}", path.display()))?
                .replace('\\', "/");
            out.push((rel, metadata.len(), mtime));
        }
    }
    Ok(())
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_name() == ".git" {
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
    fn cached_matches_tree_hash_and_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", b"one");
        let cached = cached_tree_hash(dir.path()).unwrap();
        assert_eq!(cached, tree_hash(dir.path()).unwrap());

        // A content change of a different length flips the signature, so the
        // memo recomputes rather than returning the stale hash.
        write(dir.path(), "f.txt", b"a much longer body");
        let after = cached_tree_hash(dir.path()).unwrap();
        assert_ne!(cached, after);
        assert_eq!(after, tree_hash(dir.path()).unwrap());
    }

    #[test]
    fn signature_tracks_content_length() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", b"one");
        let before = tree_signature(dir.path()).unwrap();
        write(dir.path(), "f.txt", b"two longer");
        assert_ne!(before, tree_signature(dir.path()).unwrap());
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

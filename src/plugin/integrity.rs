//! Deterministic content hash over a plugin's source tree.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

const HASH_PREFIX: &[u8] = b"aoe-plugin-tree-hash-v1\0";

pub const BUILD_OUTPUT_DIR: &str = ".aoe-build";

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
    fn hash_is_stable_prefixed_and_sensitive_to_content_and_path() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "aoe-plugin.toml", b"id = \"a.b\"\n");
        write(dir.path(), "src/main.rs", b"fn main() {}\n");
        let first = tree_hash(dir.path()).unwrap();
        assert_eq!(first, tree_hash(dir.path()).unwrap(), "stable across runs");
        assert!(first.starts_with("sha256:"));

        write(dir.path(), "src/main.rs", b"fn main() { 1; }\n");
        assert_ne!(first, tree_hash(dir.path()).unwrap(), "content is hashed");

        let a = tempfile::tempdir().unwrap();
        write(a.path(), "x", b"1");
        write(a.path(), "y", b"2");
        let b = tempfile::tempdir().unwrap();
        write(b.path(), "x", b"2");
        write(b.path(), "y", b"1");
        assert_ne!(
            tree_hash(a.path()).unwrap(),
            tree_hash(b.path()).unwrap(),
            "which path holds which bytes matters"
        );
    }

    #[test]
    fn top_level_git_and_build_output_dirs_are_skipped() {
        let bare = tempfile::tempdir().unwrap();
        write(bare.path(), "aoe-plugin.toml", b"x");
        let expected = tree_hash(bare.path()).unwrap();

        for junk in [
            ".git/config".to_string(),
            format!("{BUILD_OUTPUT_DIR}/venv/pyvenv.cfg"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), "aoe-plugin.toml", b"x");
            write(dir.path(), &junk, b"junk");
            assert_eq!(tree_hash(dir.path()).unwrap(), expected, "{junk}");
        }

        let nested = tempfile::tempdir().unwrap();
        write(nested.path(), "aoe-plugin.toml", b"x");
        write(nested.path(), "sub/.aoe-build/hidden", b"payload");
        assert_ne!(
            tree_hash(nested.path()).unwrap(),
            expected,
            "only the top-level build output dir is skipped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_rejected_outside_the_build_output_dir() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real", b"x");
        std::os::unix::fs::symlink("real", dir.path().join("link")).unwrap();
        let err = tree_hash(dir.path()).unwrap_err().to_string();
        assert!(err.contains("symlink"), "got: {err}");

        let nested = tempfile::tempdir().unwrap();
        write(nested.path(), "aoe-plugin.toml", b"x");
        let sub = nested.path().join("sub").join(".aoe-build");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink("real", sub.join("link")).unwrap();
        let err = tree_hash(nested.path()).unwrap_err().to_string();
        assert!(
            err.contains("symlink"),
            "a nested build dir is still hashed"
        );

        let skipped = tempfile::tempdir().unwrap();
        write(skipped.path(), "aoe-plugin.toml", b"x");
        let build = skipped.path().join(BUILD_OUTPUT_DIR).join("bin");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink("real", build.join("python3")).unwrap();
        assert!(tree_hash(skipped.path()).is_ok());
    }
}

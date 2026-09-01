//! Node.js runtime resolution for acp-worker subprocesses.
//!
//! Resolve order (matches the v4 design doc):
//! 1. `AOE_ACP_NODE` env var.
//! 2. `acp.node_path` from settings.
//! 3. `node` on `PATH` (must satisfy minimum version).
//! 4. Previously-downloaded Node at
//!    `$AOE_DATA_DIR/acp/node-v22.21.0/bin/node`.
//! 5. (Future) download from nodejs.org/dist on first use.
//!
//! For 5 we have a real `download` function, but it is opt-in: the
//! caller must explicitly invoke it. Resolving at session-spawn time
//! returns a typed error if no Node is present, and the CLI surfaces
//! the doctor's `[!! ] Node runtime missing` message.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{debug, info, warn};

/// The minimum Node major version aoe-agent supports. Pinned to the
/// `engines.node` field in `acp-worker/aoe-agent/package.json` by
/// `package_engines_matches_min_node_major`.
pub const MIN_NODE_MAJOR: u32 = 22;

/// The pinned Node version aoe downloads when no host Node is found.
/// Bumping this requires bumping the SHA-256 below at the same time.
pub const PINNED_NODE_VERSION: &str = "22.21.0";

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("no Node.js >= {0} found and AOE_ACP_NODE is unset")]
    NoNode(u32),
    #[error("Node at {path} is too old (version {found}; need >= {min})")]
    TooOld {
        path: PathBuf,
        found: String,
        min: u32,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of a successful resolve.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub path: PathBuf,
    pub version: String,
    pub source: NodeSource,
}

#[derive(Debug, Clone, Copy)]
pub enum NodeSource {
    Env,
    Settings,
    Path,
    Bundled,
}

/// Resolve Node.js for structured view use. `settings_node_path` is the value
/// configured in `acp.node_path` (empty when unset). `app_dir` is
/// where the bundled tarball would be extracted.
pub fn resolve(settings_node_path: &str, app_dir: &Path) -> Result<ResolvedNode, NodeError> {
    if let Ok(env_path) = std::env::var("AOE_ACP_NODE") {
        if !env_path.is_empty() {
            let path = PathBuf::from(env_path);
            return verify_path(&path, NodeSource::Env);
        }
    }

    if !settings_node_path.is_empty() {
        let path = PathBuf::from(settings_node_path);
        return verify_path(&path, NodeSource::Settings);
    }

    if let Some(path) = which("node") {
        if let Ok(node) = verify_path(&path, NodeSource::Path) {
            return Ok(node);
        }
    }

    let bundled = bundled_node_path(app_dir);
    if bundled.exists() {
        return verify_path(&bundled, NodeSource::Bundled);
    }

    Err(NodeError::NoNode(MIN_NODE_MAJOR))
}

fn verify_path(path: &Path, source: NodeSource) -> Result<ResolvedNode, NodeError> {
    let output = std::process::Command::new(path).arg("--version").output()?;
    if !output.status.success() {
        return Err(NodeError::TooOld {
            path: path.to_path_buf(),
            found: "<no version output>".into(),
            min: MIN_NODE_MAJOR,
        });
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if meets_minimum(&raw) != Some(true) {
        return Err(NodeError::TooOld {
            path: path.to_path_buf(),
            found: raw,
            min: MIN_NODE_MAJOR,
        });
    }
    debug!(target: "acp.node", source = ?source, path = %path.display(), version = %raw, "node resolved");
    Ok(ResolvedNode {
        path: path.to_path_buf(),
        version: raw,
        source,
    })
}

fn parse_major(raw: &str) -> Option<u32> {
    let trimmed = raw.trim_start_matches('v');
    let major_str = trimmed.split('.').next()?;
    major_str.parse::<u32>().ok()
}

/// Whether a raw `node --version` string satisfies [`MIN_NODE_MAJOR`].
/// `None` for unrecognisable output, which callers must treat as "not
/// proven compatible" rather than as a pass. The spawn path and
/// `aoe acp doctor` share this so their verdicts cannot diverge.
pub fn meets_minimum(raw: &str) -> Option<bool> {
    parse_major(raw).map(|major| major >= MIN_NODE_MAJOR)
}

fn which(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn bundled_node_path(app_dir: &Path) -> PathBuf {
    app_dir
        .join("acp")
        .join(format!("node-v{PINNED_NODE_VERSION}"))
        .join("bin")
        .join("node")
}

/// Pinned platform-specific tarball SHA-256 values for
/// `PINNED_NODE_VERSION`. Fetched once from nodejs.org's SHASUMS256.txt
/// and committed here. Bumping `PINNED_NODE_VERSION` requires
/// refreshing every entry in this table.
struct PlatformTarball {
    /// e.g., "linux-x64". Forms the filename: node-vX.Y.Z-{slug}.tar.xz
    slug: &'static str,
    /// Hex-encoded SHA-256 of the tarball.
    sha256: &'static str,
}

const PINNED_TARBALLS: &[(NodePlatform, PlatformTarball)] = &[
    (
        NodePlatform::LinuxX64,
        PlatformTarball {
            slug: "linux-x64",
            sha256: "71a04f4b9144870c9407b8019fe912514229e50246bc706862eded3ac8e9025d",
        },
    ),
    (
        NodePlatform::LinuxArm64,
        PlatformTarball {
            slug: "linux-arm64",
            sha256: "fe3e371f6f72d07a3f75a94a54c97d652ace6bfcc48f82cc0867f0c0722b84bd",
        },
    ),
    (
        NodePlatform::DarwinX64,
        PlatformTarball {
            slug: "darwin-x64",
            sha256: "8c61b1ab7b3a398717b3503fbd205d239079cac22402ee9327f4d3a240622d86",
        },
    ),
    (
        NodePlatform::DarwinArm64,
        PlatformTarball {
            slug: "darwin-arm64",
            sha256: "54b884588727c9833cad6e4b902f922128b8da136ba845e76e878b0d2d08c8f4",
        },
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodePlatform {
    LinuxX64,
    LinuxArm64,
    DarwinX64,
    DarwinArm64,
    /// Windows uses a .zip; we don't support it via auto-download
    /// today (would need a zip extractor). Users on Windows must
    /// install Node themselves.
    WindowsUnsupported,
}

pub fn detect_platform() -> NodePlatform {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("linux", "x86_64") => NodePlatform::LinuxX64,
        ("linux", "aarch64") => NodePlatform::LinuxArm64,
        ("macos", "x86_64") => NodePlatform::DarwinX64,
        ("macos", "aarch64") => NodePlatform::DarwinArm64,
        ("windows", _) => NodePlatform::WindowsUnsupported,
        _ => NodePlatform::WindowsUnsupported,
    }
}

fn pinned_for(platform: NodePlatform) -> Option<&'static PlatformTarball> {
    PINNED_TARBALLS
        .iter()
        .find(|(p, _)| *p == platform)
        .map(|(_, t)| t)
}

/// Download the pinned Node tarball from nodejs.org/dist and extract
/// to the bundled location. Verifies SHA-256 against the embedded
/// value before extracting.
///
/// On Windows, returns NoNode because tarball auto-download is not
/// implemented for .zip; users must install Node themselves.
pub async fn download(app_dir: &Path) -> Result<ResolvedNode, NodeError> {
    let platform = detect_platform();
    let tarball = pinned_for(platform).ok_or_else(|| {
        warn!(
            target: "acp.node",
            "automated Node download not supported on this platform; install Node {} on PATH or set AOE_ACP_NODE",
            MIN_NODE_MAJOR
        );
        NodeError::NoNode(MIN_NODE_MAJOR)
    })?;

    let url = format!(
        "https://nodejs.org/dist/v{version}/node-v{version}-{slug}.tar.xz",
        version = PINNED_NODE_VERSION,
        slug = tarball.slug,
    );
    info!(target: "acp.node", url = %url, "downloading Node runtime");

    let bytes = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .map_err(|e| NodeError::Io(std::io::Error::other(format!("fetch: {e}"))))?
        .error_for_status()
        .map_err(|e| NodeError::Io(std::io::Error::other(format!("status: {e}"))))?
        .bytes()
        .await
        .map_err(|e| NodeError::Io(std::io::Error::other(format!("body: {e}"))))?;

    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(tarball.sha256) {
        return Err(NodeError::Io(std::io::Error::other(format!(
            "Node tarball SHA-256 mismatch: expected {} got {}",
            tarball.sha256, actual
        ))));
    }
    info!(target: "acp.node", "downloaded {} bytes; SHA-256 verified", bytes.len());

    // Extract under app_dir/acp/. The tarball's top-level dir is
    // `node-vX.Y.Z-{slug}` so we extract into the parent and then
    // rename/symlink to `node-vX.Y.Z` for a stable bundled-path lookup.
    let acp_dir = app_dir.join("acp");
    std::fs::create_dir_all(&acp_dir)?;

    let cursor = std::io::Cursor::new(bytes);
    let xz_decoder = xz2::read::XzDecoder::new(cursor);
    let mut archive = tar::Archive::new(xz_decoder);
    archive.unpack(&acp_dir)?;

    // Move/rename the extracted dir to the stable name.
    let extracted = acp_dir.join(format!("node-v{}-{}", PINNED_NODE_VERSION, tarball.slug));
    let stable = acp_dir.join(format!("node-v{}", PINNED_NODE_VERSION));
    if stable.exists() {
        std::fs::remove_dir_all(&stable)?;
    }
    std::fs::rename(&extracted, &stable)?;

    let bundled = bundled_node_path(app_dir);
    verify_path(&bundled, NodeSource::Bundled)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xF) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_major_handles_v_prefix_and_unprefixed() {
        assert_eq!(parse_major("v22.21.0"), Some(22));
        assert_eq!(parse_major("v20.0.0"), Some(20));
        assert_eq!(parse_major("18.17.1"), Some(18));
        assert_eq!(parse_major("not a version"), None);
    }

    #[test]
    fn meets_minimum_is_inclusive_at_the_boundary() {
        for (raw, expected) in [
            (format!("v{}.9.9", MIN_NODE_MAJOR - 1), Some(false)),
            (format!("v{MIN_NODE_MAJOR}.0.0"), Some(true)),
            (format!("{MIN_NODE_MAJOR}.0.0"), Some(true)),
            (format!("v{}.0.0", MIN_NODE_MAJOR + 1), Some(true)),
            ("not a version".to_string(), None),
            (String::new(), None),
        ] {
            assert_eq!(meets_minimum(&raw), expected, "for {raw:?}");
        }
    }

    #[test]
    fn package_engines_matches_min_node_major() {
        // package.json cannot read a Rust const, so `engines.node` is the
        // one restatement of the floor outside this module. Assert it
        // tracks the gate so a bump that forgets one side fails CI instead
        // of letting a host pass `aoe acp doctor` and fail at spawn.
        let manifest = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/acp-worker/aoe-agent/package.json"
        ));
        let json: serde_json::Value = serde_json::from_str(manifest).expect("valid package.json");
        let engines = json["engines"]["node"]
            .as_str()
            .expect("package.json declares engines.node");
        let declared = parse_major(engines.trim_start_matches(">="))
            .unwrap_or_else(|| panic!("unparseable engines.node range {engines:?}"));
        assert_eq!(declared, MIN_NODE_MAJOR, "engines.node is {engines:?}");

        // The runtime we download when the host has none must clear the
        // same bar, or `download` returns a Node `verify_path` rejects.
        assert_eq!(meets_minimum(PINNED_NODE_VERSION), Some(true));
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // SHA-256 of the empty string per RFC 6234 / Wikipedia.
        let hex = sha256_hex(b"");
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn pinned_tarballs_cover_all_supported_platforms() {
        for platform in [
            NodePlatform::LinuxX64,
            NodePlatform::LinuxArm64,
            NodePlatform::DarwinX64,
            NodePlatform::DarwinArm64,
        ] {
            let tarball = pinned_for(platform);
            assert!(tarball.is_some(), "missing pinned SHA for {platform:?}");
            let sha = tarball.unwrap().sha256;
            assert_eq!(sha.len(), 64, "SHA must be 64 hex chars");
            assert!(
                sha.chars().all(|c| c.is_ascii_hexdigit()),
                "SHA must be hex"
            );
        }
        assert!(pinned_for(NodePlatform::WindowsUnsupported).is_none());
    }

    #[test]
    fn bundled_path_uses_pinned_version() {
        let p = bundled_node_path(Path::new("/tmp/aoe"));
        let s = p.to_string_lossy();
        assert!(s.contains(&format!("node-v{PINNED_NODE_VERSION}")));
        assert!(s.ends_with("/bin/node") || s.ends_with("\\bin\\node"));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_uses_env_var_when_set() {
        let Some(p) = which("node") else {
            eprintln!("skipping: node not on PATH");
            return;
        };
        std::env::set_var("AOE_ACP_NODE", &p);
        let temp = tempfile::tempdir().unwrap();
        let resolved = resolve("", temp.path()).expect("env var resolves");
        std::env::remove_var("AOE_ACP_NODE");
        assert!(matches!(resolved.source, NodeSource::Env));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_returns_no_node_with_unmatchable_settings() {
        // No PATH-side node, no env, no settings → NoNode.
        let temp = tempfile::tempdir().unwrap();
        let saved_path = std::env::var_os("PATH");
        let saved_env = std::env::var_os("AOE_ACP_NODE");
        std::env::remove_var("PATH");
        std::env::remove_var("AOE_ACP_NODE");
        let result = resolve("", temp.path());
        if let Some(p) = saved_path {
            std::env::set_var("PATH", p);
        }
        if let Some(v) = saved_env {
            std::env::set_var("AOE_ACP_NODE", v);
        }
        assert!(matches!(result, Err(NodeError::NoNode(_))));
    }
}

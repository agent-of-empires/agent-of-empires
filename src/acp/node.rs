//! Node.js runtime resolution for acp-worker subprocesses.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{debug, info, warn};

/// The Node major floor for every adapter.
pub const MIN_NODE_MAJOR: u32 = 22;
/// Minor floor, within `MIN_NODE_MAJOR`, for adapters that ship sources:
/// `--experimental-strip-types`, which runs the bundled `aoe-agent`, arrived
/// in 22.6.
pub const MIN_NODE_MINOR: u32 = 6;

/// The pinned Node version aoe downloads when no host Node is found.
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

/// Resolve Node.js for structured view use.
pub fn resolve(settings_node_path: &str, app_dir: &Path) -> Result<ResolvedNode, NodeError> {
    resolve_for(settings_node_path, app_dir, false)
}

/// Like [`resolve`]; with `sources` the PATH copy is passed over for the
/// bundled runtime when it cannot run an in-tree adapter's TypeScript.
pub fn resolve_for(
    settings_node_path: &str,
    app_dir: &Path,
    sources: bool,
) -> Result<ResolvedNode, NodeError> {
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
            if !sources || supports_strip_types(&node.version) {
                return Ok(node);
            }
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

fn parse_major_minor(raw: &str) -> Option<(u32, u32)> {
    let trimmed = raw.trim().trim_start_matches('v');
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    Some((major, minor))
}

/// Whether a raw `node --version` string satisfies [`MIN_NODE_MAJOR`].
pub fn meets_minimum(raw: &str) -> Option<bool> {
    parse_major_minor(raw).map(|(major, _)| major >= MIN_NODE_MAJOR)
}

/// Whether `raw` can run an in-tree adapter's TypeScript sources
/// (`MIN_NODE_MAJOR.MIN_NODE_MINOR` or newer).
pub fn supports_strip_types(raw: &str) -> bool {
    parse_major_minor(raw).is_some_and(|(major, minor)| {
        major > MIN_NODE_MAJOR || (major == MIN_NODE_MAJOR && minor >= MIN_NODE_MINOR)
    })
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
/// `PINNED_NODE_VERSION`.
struct PlatformTarball {
    /// e.g., "linux-x64".
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
    /// today (would need a zip extractor).
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
/// to the bundled location.
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

    // Extract under app_dir/acp/.
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

    /// `meets_minimum` gates on the major; strip-types also on the minor.
    #[test]
    fn version_floors_gate_on_major_then_minor() {
        let below_major = format!("v{}.9.9", MIN_NODE_MAJOR - 1);
        let below_minor = format!("v{MIN_NODE_MAJOR}.{}.9", MIN_NODE_MINOR - 1);
        let at_floor = format!("{MIN_NODE_MAJOR}.{MIN_NODE_MINOR}.0");
        let above = format!("v{}.0.0", MIN_NODE_MAJOR + 1);
        // (raw, meets_minimum, supports_strip_types)
        let cases = [
            (below_major.as_str(), Some(false), false),
            (below_minor.as_str(), Some(true), false),
            (at_floor.as_str(), Some(true), true),
            (above.as_str(), Some(true), true),
            ("not a version", None, false),
            ("", None, false),
        ];
        for (raw, minimum, strip_types) in cases {
            assert_eq!(meets_minimum(raw), minimum, "{raw:?}");
            assert_eq!(supports_strip_types(raw), strip_types, "{raw:?}");
        }
        // The parser tolerates a `v` prefix and short forms.
        assert_eq!(parse_major_minor("v22.21.0"), Some((22, 21)));
        assert_eq!(parse_major_minor("20"), Some((20, 0)));
        assert_eq!(parse_major_minor("18.17.1"), Some((18, 17)));
        assert_eq!(parse_major_minor("not a version"), None);
    }

    #[test]
    fn package_engines_matches_min_node_major() {
        let manifest = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/acp-worker/aoe-agent/package.json"
        ));
        let json: serde_json::Value = serde_json::from_str(manifest).expect("valid package.json");
        let engines = json["engines"]["node"]
            .as_str()
            .expect("package.json declares engines.node");
        let declared = parse_major_minor(engines.trim_start_matches(">="))
            .unwrap_or_else(|| panic!("unparseable engines.node range {engines:?}"));
        assert_eq!(
            declared,
            (MIN_NODE_MAJOR, MIN_NODE_MINOR),
            "engines.node is {engines:?}"
        );

        assert_eq!(meets_minimum(PINNED_NODE_VERSION), Some(true));
        assert!(supports_strip_types(PINNED_NODE_VERSION));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_prefers_env_var_and_reports_no_node() {
        let temp = tempfile::tempdir().unwrap();
        {
            let _env = crate::session::test_support::EnvGuard::unset(&["PATH", "AOE_ACP_NODE"]);
            assert!(matches!(
                resolve("", temp.path()),
                Err(NodeError::NoNode(_))
            ));
        }
        let Some(p) = which("node") else {
            eprintln!("skipping: node not on PATH");
            return;
        };
        let _env = crate::session::test_support::EnvGuard::set(&[("AOE_ACP_NODE", &p)]);
        let resolved = resolve("", temp.path()).expect("env var resolves");
        assert!(matches!(resolved.source, NodeSource::Env));
    }
}

//! Persisted registry of remote daemon endpoints.
//!
//! Lives at `<app_dir>/remotes.toml`, owner-only, alongside the other
//! credential stores rather than in `config.toml`: entries carry a bearer
//! token and, for a daemon behind a passphrase wall, a login session and its
//! device-binding secret. The passphrase itself is never stored; an expired
//! session is re-established by prompting for it again.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const REMOTES_FILE: &str = "remotes.toml";

/// Bumped on a breaking layout change. An unparseable or newer file is an
/// error rather than a silent reset: unlike a login session, losing an entry
/// costs the user a re-add they did not ask for.
const REMOTES_SCHEMA_VERSION: u32 = 1;

/// One configured endpoint. `token`, `session` and `binding` are credentials;
/// never render them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub url: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// `aoe_session` id from a passphrase login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// base64url of the 32-byte device-binding secret paired with `session`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// Added with `--insecure`: credentials may travel over non-loopback HTTP.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
}

fn default_enabled() -> bool {
    true
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("name", &self.name)
            .field("enabled", &self.enabled)
            .field("authenticated", &(self.token.is_some() || self.has_login()))
            .field("insecure", &self.insecure)
            .finish_non_exhaustive()
    }
}

impl Remote {
    /// Whether this entry carries a passphrase-login credential pair. Both
    /// halves are required: a session without its binding is unusable.
    pub fn has_login(&self) -> bool {
        self.session.is_some() && self.binding.is_some()
    }

    /// The endpoint every client of this entry connects through.
    pub(crate) fn endpoint(&self) -> crate::acp::client::discovery::DaemonEndpoint {
        use crate::acp::client::discovery::{DaemonEndpoint, Source};
        let login = self
            .session
            .clone()
            .zip(self.binding.clone())
            .map(|(session, binding)| crate::daemon::SessionCredential { session, binding });
        DaemonEndpoint::new(self.url.clone(), self.token.clone(), Source::Remote)
            .with_login(login)
            .with_plaintext_allowed(self.insecure)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    version: u32,
    #[serde(default, rename = "remote")]
    remotes: Vec<Remote>,
}

impl Registry {
    pub fn remotes(&self) -> &[Remote] {
        &self.remotes
    }

    pub fn enabled(&self) -> impl Iterator<Item = &Remote> {
        self.remotes.iter().filter(|r| r.enabled)
    }

    pub fn get(&self, name: &str) -> Option<&Remote> {
        self.remotes.iter().find(|r| r.name == name)
    }

    /// Insert or replace by name. Returns whether an existing entry was
    /// replaced, so the caller can word its output accurately.
    pub fn upsert(&mut self, remote: Remote) -> bool {
        match self.remotes.iter_mut().find(|r| r.name == remote.name) {
            Some(existing) => {
                *existing = remote;
                true
            }
            None => {
                self.remotes.push(remote);
                false
            }
        }
    }

    /// Drop entries for `url` under any name but `name`, returning theirs. A
    /// re-pair under a new name would otherwise leave the old credentials
    /// polling, and failing, against the same daemon.
    pub fn remove_other_names_for(&mut self, name: &str, url: &str) -> Vec<String> {
        let (dropped, kept) = std::mem::take(&mut self.remotes)
            .into_iter()
            .partition(|r| r.url == url && r.name != name);
        self.remotes = kept;
        dropped.into_iter().map(|r: Remote| r.name).collect()
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.remotes.len();
        self.remotes.retain(|r| r.name != name);
        self.remotes.len() != before
    }
    /// A name for adding `url`: the entry already there keeps its name,
    /// otherwise `wanted` sanitized and suffixed until no entry has it.
    pub fn name_for(&self, wanted: &str, url: &str) -> String {
        if let Some(existing) = self.remotes.iter().find(|r| r.url == url) {
            return existing.name.clone();
        }
        let base = remote_name_for_host(wanted);
        std::iter::once(base.clone())
            .chain((2..).map(|n| format!("{base}-{n}")))
            .find(|name| self.get(name).is_none())
            .unwrap_or(base)
    }
}

/// A hostname as a remote name, lowercase `[a-z0-9-]`: its first label, or
/// the whole address for an IP.
pub fn remote_name_for_host(hostname: &str) -> String {
    let bare = hostname.trim_start_matches('[').trim_end_matches(']');
    let short = if bare.parse::<std::net::IpAddr>().is_ok() {
        bare
    } else {
        hostname.split('.').next().unwrap_or_default()
    };
    let name: String = short
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-');
    if name.is_empty() {
        "aoe".to_string()
    } else {
        name.to_string()
    }
}

/// Hosts a daemon serves over plain HTTP: loopback, LAN, link-local, a
/// tailnet's CGNAT range, and mDNS names. Anything else is assumed to sit
/// behind TLS.
fn is_private_host(host: &str) -> bool {
    use std::net::IpAddr;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "localhost" || host.ends_with(".local") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            let [a, b, ..] = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || (a == 100 && (64..128).contains(&b))
        }
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// The base URL for what someone typed after `aoe remote add`: a full URL as
/// given, or `host[:port]` with the scheme a daemon there would use (HTTP for
/// a private host, HTTPS otherwise). A daemon picks its HTTP port, so a
/// private host needs one.
pub fn parse_remote_address(raw: &str) -> Result<String> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.contains("://") {
        return Ok(raw.to_string());
    }
    let probe = reqwest::Url::parse(&format!("http://{raw}"))
        .map_err(|_| anyhow::anyhow!("{raw:?} is not a host, host:port or URL"))?;
    let host = probe
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("{raw:?} has no host"))?;
    if !is_private_host(host) {
        return Ok(format!("https://{raw}"));
    }
    if probe.port().is_none() {
        bail!("{raw:?} needs the port its daemon listens on, e.g. {raw}:8080");
    }
    Ok(format!("http://{raw}"))
}

/// The shortest text [`parse_remote_address`] turns back into `base`.
pub fn short_remote_address(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let Ok(url) = reqwest::Url::parse(base) else {
        return base.to_string();
    };
    let short = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => return base.to_string(),
    };
    match parse_remote_address(&short) {
        Ok(round) if round == base => short,
        _ => base.to_string(),
    }
}

pub fn registry_path() -> Result<PathBuf> {
    Ok(crate::session::get_app_dir()?.join(REMOTES_FILE))
}

pub fn load() -> Result<Registry> {
    load_from(&registry_path()?)
}

pub fn save(registry: &Registry) -> Result<()> {
    save_to(&registry_path()?, registry)
}

pub fn load_from(path: &Path) -> Result<Registry> {
    if !path.exists() {
        return Ok(Registry {
            version: REMOTES_SCHEMA_VERSION,
            remotes: Vec::new(),
        });
    }
    crate::util::check_owner_only_file(path, "remotes registry")?;
    let raw = std::fs::read_to_string(path).context("read remotes registry")?;
    let registry: Registry = toml::from_str(&raw).context("parse remotes registry")?;
    if registry.version > REMOTES_SCHEMA_VERSION {
        bail!(
            "remotes.toml was written by a newer aoe (schema {} > {}); upgrade or move the file aside",
            registry.version,
            REMOTES_SCHEMA_VERSION
        );
    }
    Ok(registry)
}

pub fn save_to(path: &Path, registry: &Registry) -> Result<()> {
    crate::util::check_owner_only_file(path, "remotes registry")?;
    let mut out = registry.clone();
    out.version = REMOTES_SCHEMA_VERSION;
    let body = toml::to_string(&out).context("serialize remotes registry")?;
    crate::session::atomic_write(path, body.as_bytes()).context("write remotes registry")?;
    // `atomic_write` lands a 0600 temp file; re-assert so an entry written
    // over a previously loose file cannot stay readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_address_gets_the_scheme_its_daemon_serves() {
        for (raw, expected) in [
            ("192.168.1.5:8081", Ok("http://192.168.1.5:8081")),
            ("100.101.102.103:8080", Ok("http://100.101.102.103:8080")),
            ("[fd7a:115c::1]:8080", Ok("http://[fd7a:115c::1]:8080")),
            ("mini.local:8080", Ok("http://mini.local:8080")),
            ("192.168.1.5", Err("needs the port")),
            (
                "aoe-mini.tailnet.ts.net",
                Ok("https://aoe-mini.tailnet.ts.net"),
            ),
            ("box.example.com:8443", Ok("https://box.example.com:8443")),
            ("http://10.0.0.2:8081/", Ok("http://10.0.0.2:8081")),
            (
                "https://box.ts.net/?token=abc",
                Ok("https://box.ts.net/?token=abc"),
            ),
            ("not a host", Err("not a host")),
        ] {
            match (parse_remote_address(raw), expected) {
                (Ok(url), Ok(want)) => assert_eq!(url, want, "{raw}"),
                (Err(error), Err(want)) => {
                    assert!(error.to_string().contains(want), "{raw}: {error}")
                }
                (got, want) => panic!("{raw}: got {got:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn the_short_address_round_trips_or_stays_a_url() {
        for (base, short) in [
            ("http://192.168.1.5:8081/", "192.168.1.5:8081"),
            ("https://aoe-mini.tailnet.ts.net", "aoe-mini.tailnet.ts.net"),
            ("http://box.example.com:8080", "http://box.example.com:8080"),
            ("https://10.0.0.2:8443", "https://10.0.0.2:8443"),
        ] {
            assert_eq!(short_remote_address(base), short, "{base}");
        }
    }

    #[test]
    fn a_derived_name_is_sanitized_reused_for_its_url_and_unique_otherwise() {
        let mut registry = Registry::default();
        registry.upsert(Remote {
            url: "http://192.168.1.5:8081".into(),
            ..registered(None, None)
        });
        for (wanted, url, expected) in [
            ("MacBook-Pro.local", "http://a:1", "macbook-pro"),
            ("dev box_1", "http://a:1", "dev-box-1"),
            ("", "http://a:1", "aoe"),
            ("192.168.1.9", "http://a:1", "192-168-1-9"),
            ("mini", "http://192.168.1.5:8081", "mini"),
            ("mini.example.com", "http://other:1", "mini-2"),
        ] {
            assert_eq!(registry.name_for(wanted, url), expected, "{wanted} {url}");
        }
    }

    fn registered(session: Option<&str>, binding: Option<&str>) -> Remote {
        Remote {
            name: "mini".into(),
            url: "https://mini.example.ts.net".into(),
            enabled: true,
            token: Some("tok-secret".into()),
            session: session.map(str::to_string),
            binding: binding.map(str::to_string),
            insecure: false,
        }
    }

    #[test]
    fn an_endpoint_carries_a_login_only_when_both_halves_are_present() {
        assert!(registered(None, None).endpoint().login().is_none());
        assert!(registered(Some("s"), None).endpoint().login().is_none());
        let endpoint = registered(Some("s"), Some("b")).endpoint();
        let login = endpoint.login().expect("login present");
        assert_eq!((login.session.as_str(), login.binding.as_str()), ("s", "b"));
    }

    #[test]
    fn only_an_insecure_entry_allows_plaintext() {
        let remote = registered(None, None);
        assert!(!remote.endpoint().allows_plaintext());
        let insecure = Remote {
            insecure: true,
            ..remote
        };
        let endpoint = insecure.endpoint();
        assert!(endpoint.allows_plaintext());
        assert!(format!("{endpoint:?}").contains("allow_plaintext: true"));
    }

    #[test]
    fn debug_output_never_carries_credentials() {
        let remote = registered(Some("sess-secret"), Some("bind-secret"));
        let endpoint = remote.endpoint();
        for rendered in [
            format!("{remote:?}"),
            format!("{endpoint:?}"),
            format!("{:?}", endpoint.login()),
        ] {
            for secret in ["tok-secret", "sess-secret", "bind-secret"] {
                assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
            }
        }
    }

    fn remote(name: &str) -> Remote {
        Remote {
            name: name.to_string(),
            url: format!("https://{name}.example.ts.net"),
            enabled: true,
            token: Some("tok".to_string()),
            session: None,
            binding: None,
            insecure: false,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        let mut registry = Registry::default();
        registry.upsert(remote("mini"));
        registry.upsert(Remote {
            insecure: true,
            ..remote("lan")
        });
        save_to(&path, &registry).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.remotes(), registry.remotes());
        assert!(loaded.get("lan").unwrap().insecure);
        assert_eq!(loaded.version, REMOTES_SCHEMA_VERSION);
    }

    #[test]
    fn a_missing_file_is_an_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_from(&dir.path().join("absent.toml")).unwrap();
        assert!(loaded.remotes().is_empty());
    }

    #[test]
    fn upsert_replaces_by_name_and_reports_it() {
        let mut registry = Registry::default();
        assert!(!registry.upsert(remote("mini")));
        let mut changed = remote("mini");
        changed.url = "https://other.example.ts.net".to_string();
        assert!(registry.upsert(changed));
        assert_eq!(registry.remotes().len(), 1);
        assert_eq!(
            registry.get("mini").unwrap().url,
            "https://other.example.ts.net"
        );
    }

    #[test]
    fn re_adding_a_url_under_a_new_name_drops_the_old_entry() {
        let mut registry = Registry::default();
        let mut old = remote("old");
        old.url = remote("home").url;
        registry.upsert(old);
        registry.upsert(remote("home"));
        registry.upsert(remote("elsewhere"));
        assert_eq!(
            registry.remove_other_names_for("home", &remote("home").url),
            ["old"]
        );
        let names: Vec<_> = registry.remotes().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["home", "elsewhere"]);
    }

    #[test]
    fn remove_reports_whether_anything_went() {
        let mut registry = Registry::default();
        registry.upsert(remote("mini"));
        assert!(registry.remove("mini"));
        assert!(!registry.remove("mini"));
    }

    #[test]
    fn enabled_filters_disabled_entries() {
        let mut registry = Registry::default();
        registry.upsert(remote("on"));
        let mut off = remote("off");
        off.enabled = false;
        registry.upsert(off);
        let names: Vec<_> = registry.enabled().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["on"]);
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_silently_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        std::fs::write(&path, "version = 99\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(load_from(&path).is_err());
    }

    #[test]
    fn login_requires_both_halves() {
        let mut r = remote("mini");
        assert!(!r.has_login());
        r.session = Some("s".to_string());
        assert!(!r.has_login());
        r.binding = Some("b".to_string());
        assert!(r.has_login());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_registry_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("planted.toml");
        std::fs::write(&target, "version = 1\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("remotes.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(load_from(&link).is_err());
        assert!(save_to(&link, &Registry::default()).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "version = 1\n");
    }
}

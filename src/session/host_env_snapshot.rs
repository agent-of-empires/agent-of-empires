//! Persisted snapshot of the operator's interactive environment.
//!
//! Session environment forwarding ([`super::environment::inherited_host_env`])
//! sources its values from aoe's own process environment. That works when the
//! process creating the session is the one the operator launched from a shell,
//! and fails completely when it is not: an `aoe serve` daemon started by a
//! systemd unit without `import-environment`, at boot, over a bare SSH
//! `command`, or respawned by `aoe update` has no `DISPLAY`, no
//! `XDG_RUNTIME_DIR`, and none of the operator's own vars to forward. It cannot
//! hand a session what it never had (#3262, the limitation #3079 deferred).
//!
//! So capture the environment where it is rich, at every interactive aoe
//! invocation, and persist it. A later impoverished daemon reads the snapshot
//! and uses it to fill the gaps in its own environment. The snapshot is a
//! fallback layer only: a value aoe actually has always wins, so a fresh
//! desktop session is never overridden by a stale capture.
//!
//! The file holds the operator's whole shell environment, which routinely
//! includes API tokens, so it is written owner-only (0600) like the sibling
//! `serve.passphrase`.

use std::collections::BTreeMap;
use std::io::IsTerminal;

/// Snapshot filename inside the app data dir.
const SNAPSHOT_FILE: &str = "host_env.json";

/// Keys the snapshot never replays, even when a stored value exists.
///
/// These pin process identity and where aoe reads and writes its own state. A
/// stale `HOME` or `XDG_CONFIG_HOME` replayed into an ACP runner moves the
/// worker-registry path it writes, which the daemon then observes as missing
/// and respawns, the #1383 respawn loop. `PATH` decides which binary an agent
/// loads. The shell bookkeeping vars (`PWD`, `SHLVL`, `_`) describe the
/// captured process, not the session, so replaying them is meaningless. All of
/// them are also vars the live environment always has, so denying them here
/// costs nothing.
///
/// Enforced on read rather than only on write so a snapshot from an older aoe,
/// or a hand-edited one, still cannot move these.
const NEVER_REPLAY: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "XDG_CONFIG_HOME",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "TERM",
];

/// Why a key is excluded from the snapshot, or `None` when it may be stored
/// and replayed.
///
/// `AOE_`-prefixed keys are aoe's own per-process wiring and credentials
/// (`AOE_TOKEN`, `AOE_DAEMON_TOKEN`, `AOE_ACP_SOCKET`, the runner env
/// carrier); a captured value would be meaningless or actively wrong in a
/// later process, so the whole prefix is refused.
fn snapshot_denyreason(key: &str) -> Option<&'static str> {
    if !super::environment::is_valid_env_key(key) {
        return Some("not a valid environment variable name");
    }
    if key.starts_with("AOE_") {
        return Some("aoe-internal wiring or credential");
    }
    if NEVER_REPLAY.contains(&key) {
        return Some("pins process identity or aoe's own state paths");
    }
    None
}

/// Path to the snapshot file, without creating the app dir.
fn snapshot_path() -> Option<std::path::PathBuf> {
    super::get_app_dir_path()
        .ok()
        .map(|dir| dir.join(SNAPSHOT_FILE))
}

/// Capture aoe's current environment to the snapshot when this invocation looks
/// interactive, meaning stdin is a terminal.
///
/// The tty check is the whole heuristic, and it is the right one: a process the
/// operator started from a shell has their environment, and a daemon started by
/// systemd, cron, a boot unit, or an `aoe update` respawn does not have a tty
/// and must not overwrite a good snapshot with its own impoverished one. Called
/// on every interactive invocation so the stored values track the operator's
/// current login rather than drifting from one stale capture.
///
/// Best-effort: a failure to write is logged at debug and otherwise ignored,
/// since forwarding is an enhancement and no command should fail over it.
pub fn capture_if_interactive() {
    if !std::io::stdin().is_terminal() {
        return;
    }
    let vars: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(key, value)| !value.is_empty() && snapshot_denyreason(key).is_none())
        .collect();
    if vars.is_empty() {
        return;
    }
    if let Err(e) = write_snapshot(&vars) {
        tracing::debug!(
            target: "session.create",
            error = %e,
            "could not persist the host environment snapshot"
        );
    }
}

/// Serialize `vars` to the snapshot file at 0600.
///
/// Writes through a temp file in the same dir and renames, so a concurrent
/// reader sees either the old snapshot or the new one and never a half-written
/// file. The 0600 mode is applied to the temp file before any content is
/// written, so the values are never briefly world-readable.
fn write_snapshot(vars: &BTreeMap<String, String>) -> anyhow::Result<()> {
    let dir = super::get_app_dir()?;
    let final_path = dir.join(SNAPSHOT_FILE);
    let tmp_path = dir.join(format!("{SNAPSHOT_FILE}.tmp{}", std::process::id()));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&tmp_path)?;
    serde_json::to_writer(&file, vars)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// The stored environment pairs available to fill gaps in aoe's own
/// environment, or an empty vec when there is no readable snapshot.
///
/// Read fresh on every call rather than cached: a long-lived daemon must pick
/// up the snapshot a later interactive launch wrote, and the file is a few KiB
/// read once per session spawn.
pub(crate) fn snapshot_pairs() -> Vec<(String, String)> {
    let Some(path) = snapshot_path() else {
        return Vec::new();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // A missing snapshot is the normal case on a fresh install.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::debug!(
                target: "session.create",
                error = %e,
                "could not read the host environment snapshot"
            );
            return Vec::new();
        }
    };
    let stored: BTreeMap<String, String> = match serde_json::from_str(&raw) {
        Ok(stored) => stored,
        Err(e) => {
            tracing::warn!(
                target: "session.create",
                error = %e,
                "ignoring a malformed host environment snapshot"
            );
            return Vec::new();
        }
    };
    stored
        .into_iter()
        .filter(|(key, value)| !value.is_empty() && snapshot_denyreason(key).is_none())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_denyreason() {
        // Keys that carry the operator's desktop and toolchain context are
        // exactly what the snapshot exists to replay.
        let allowed = ["DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "GOPATH"];
        for key in allowed {
            assert!(snapshot_denyreason(key).is_none(), "{key} should be stored");
        }
        let refused = [
            // aoe's own auth token and per-process wiring.
            "AOE_TOKEN",
            "AOE_DAEMON_TOKEN",
            "AOE_ACP_SOCKET",
            // Replaying a stale one of these moves aoe's state paths or picks
            // a different agent binary; see NEVER_REPLAY.
            "HOME",
            "PATH",
            "XDG_CONFIG_HOME",
            "SHELL",
            // Shell bookkeeping describes the captured process, not a session.
            "PWD",
            "SHLVL",
            "_",
            // Malformed keys never reach Command::env.
            "",
            "1BAD",
            "HAS-DASH",
        ];
        for key in refused {
            assert!(
                snapshot_denyreason(key).is_some(),
                "{key:?} should be refused"
            );
        }
    }

    /// A snapshot written by an older aoe (or hand-edited) can hold a key the
    /// current deny list refuses. The read path must drop it rather than trust
    /// the file, or a stale `HOME` reaches a runner and reproduces #1383.
    #[test]
    #[serial_test::serial]
    fn test_snapshot_pairs_filters_stored_denied_keys() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = crate::session::test_support::isolate_app_dir_at(tmp.path());

        let stored = BTreeMap::from([
            ("DISPLAY".to_string(), ":0".to_string()),
            ("GOPATH".to_string(), "/home/me/go".to_string()),
            // Must be dropped on read even though it is in the file.
            ("HOME".to_string(), "/stale/home".to_string()),
            ("AOE_TOKEN".to_string(), "secret".to_string()),
            // Empty values are useless to forward and would clobber a good
            // inherited value with a blank one.
            ("XDG_RUNTIME_DIR".to_string(), String::new()),
        ]);
        write_snapshot(&stored).expect("snapshot written");

        let pairs = snapshot_pairs();
        assert_eq!(
            pairs,
            vec![
                ("DISPLAY".to_string(), ":0".to_string()),
                ("GOPATH".to_string(), "/home/me/go".to_string()),
            ]
        );
    }

    /// The file holds the operator's whole shell environment, tokens included,
    /// so it must not be group- or world-readable.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn test_snapshot_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = crate::session::test_support::isolate_app_dir_at(tmp.path());

        write_snapshot(&BTreeMap::from([("DISPLAY".to_string(), ":0".to_string())]))
            .expect("snapshot written");

        let path = snapshot_path().expect("snapshot path");
        let mode = std::fs::metadata(&path)
            .expect("snapshot metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "snapshot must be 0600, got {mode:o}");
    }

    /// A truncated or non-JSON snapshot must degrade to "no snapshot" rather
    /// than propagate an error into a session spawn.
    #[test]
    #[serial_test::serial]
    fn test_snapshot_pairs_ignores_malformed_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = crate::session::test_support::isolate_app_dir_at(tmp.path());

        let path = snapshot_path().expect("snapshot path");
        std::fs::create_dir_all(path.parent().expect("app dir")).expect("app dir");
        std::fs::write(&path, "{not json").expect("write");
        assert!(snapshot_pairs().is_empty());
    }
}

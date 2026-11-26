//! Build identity.
//!
//! Release builds use the `AOE_BUILD_VERSION` that `build.rs` stamps: the
//! package version plus a git commit (e.g. `1.9.5+g7f31a9c42e01`, or `…-dirty`
//! for an uncommitted working tree), or the bare `CARGO_PKG_VERSION` when no
//! VCS identity is available. Unstamped debug builds use the running
//! executable's mtime and size.
//!
//! Acp worker records embed this value so the daemon can respawn
//! workers left running on an older binary after `aoe update`. See #1754.

use std::sync::LazyLock;

/// Build identity of this binary. `main` calls it at startup so a debug
/// identity describes the executable that started, not a later rebuild.
pub fn build_version() -> &'static str {
    if let Some(stamped) = option_env!("AOE_BUILD_VERSION") {
        return stamped;
    }
    static EXE: LazyLock<String> = LazyLock::new(exe_identity);
    &EXE
}

fn exe_identity() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let Ok(meta) = std::env::current_exe().and_then(std::fs::metadata) else {
        return version.to_string();
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    format!("{version}+exe.{mtime}.{}", meta.len())
}

//! Compile-time build identity.
//!
//! [`BUILD_VERSION`] is the string `build.rs` stamps via the
//! `AOE_BUILD_VERSION` rustc-env. It pairs the package version with a git
//! commit identity (e.g. `1.9.5+g7f31a9c42e01`, or `…-dirty` for an
//! uncommitted working tree), falling back to the bare `CARGO_PKG_VERSION`
//! when no VCS identity is available.
//!
//! Acp worker records embed this value so the daemon can respawn
//! workers left running on an older binary after `aoe update`. See #1754.

/// Build identity of this binary. Stamped by `build.rs`; falls back to the
/// package version when `AOE_BUILD_VERSION` was not emitted.
pub const BUILD_VERSION: &str = match option_env!("AOE_BUILD_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

//! Guards the opt-in contract for the kache build cache (issue #1663).
//!
//! kache is an optional rustc wrapper that each developer enables through the
//! `RUSTC_WRAPPER` environment variable in their own shell. It must never be
//! committed as `[build] rustc-wrapper` (or `rustc-workspace-wrapper`) in
//! `.cargo/config.toml`: a committed wrapper forces kache onto every
//! contributor and every CI, Nix, and release runner, hard-failing any
//! environment that does not have kache installed. See docs/development.md,
//! "Faster rebuilds across worktrees (kache)".

use std::path::PathBuf;

#[test]
fn cargo_config_does_not_commit_a_rustc_wrapper() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".cargo")
        .join("config.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let config: toml::Table = text
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));

    let build = match config.get("build") {
        Some(toml::Value::Table(build)) => build,
        // No [build] section at all is trivially fine.
        _ => return,
    };

    for key in ["rustc-wrapper", "rustc-workspace-wrapper"] {
        assert!(
            !build.contains_key(key),
            "[build].{key} is set in .cargo/config.toml; kache must stay opt-in \
             via the RUSTC_WRAPPER env var, not committed. A committed wrapper \
             hard-fails every contributor and every CI/Nix/release runner \
             without kache installed. See docs/development.md."
        );
    }
}

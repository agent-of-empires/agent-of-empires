//! Registry of opt-in features whose adoption telemetry reports.
//!
//! This is the single, auditable place that decides which feature flags are
//! tracked. The result is a map keyed by a **fixed set of short feature
//! names** (the allowlist), so it can never carry a path, name, or other
//! free-form value, and the gateway forwards it as allowlisted short-id ->
//! bool while dropping anything else.
//!
//! Tracking a newly gated feature is one entry here: add its name and how to
//! detect it from [`Config`]. For example, an `openshell` feature behind a
//! config flag would be `m.insert("openshell".into(), config.openshell.enabled)`.

use std::collections::BTreeMap;

use crate::session::config::UpdateCheckMode;
use crate::session::Config;

/// Install-level feature adoption: allowlisted feature name -> whether it is
/// turned on in the **global** config for this install.
///
/// This is deliberately the global, pre-profile-merge config, not a profile's
/// effective config: it answers "what does this install default to", which is a
/// stable install-level adoption signal. Because sessions can run under arbitrary
/// profiles whose overrides are not reflected here, this is not per-session usage;
/// per-session adoption is reported separately by the snapshot's session counts
/// (`session_sandboxed`, `session_yolo`, ...). Documented in `docs/telemetry.md`.
pub fn active_features(config: &Config) -> BTreeMap<String, bool> {
    let mut features = BTreeMap::new();
    features.insert("worktree".to_string(), config.worktree.enabled);
    features.insert("sandbox".to_string(), config.sandbox.enabled_by_default);
    features.insert(
        "auto_update".to_string(),
        matches!(config.updates.update_check_mode, UpdateCheckMode::Auto),
    );
    features
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_allowlisted_flags_from_config() {
        let mut config = Config::default();
        config.worktree.enabled = true;
        config.updates.update_check_mode = UpdateCheckMode::Auto;

        let features = active_features(&config);
        assert_eq!(features.get("worktree"), Some(&true));
        assert_eq!(features.get("auto_update"), Some(&true));
        // Defaults stay false, but the keys are always present so the gateway
        // sees a stable, fixed key set.
        assert_eq!(features.get("sandbox"), Some(&false));
    }
}

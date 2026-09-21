//! Allowlisted registry of opt-in features whose adoption telemetry reports.

use std::collections::BTreeMap;

use crate::session::config::UpdateCheckMode;
use crate::session::Config;

/// Reads the global, pre-profile-merge config: an install-level signal, not per-session usage.
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
        assert_eq!(features.get("sandbox"), Some(&false));
    }
}

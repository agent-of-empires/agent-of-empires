//! Ensure global-only settings migrate after the v030 lineages converge.

pub fn run() -> anyhow::Result<()> {
    super::v030_global_only_profile_settings::run()
}

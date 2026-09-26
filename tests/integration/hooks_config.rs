//! Hook resolution across the saved global and profile files and a repo layer.
//! Merge semantics are unit tested in `session::config`; this pins the on-disk
//! pipeline.

use agent_of_empires::session::{
    merge_repo_config, resolve_config, save_profile_config, update_config, ProfileConfig,
    RepoConfig,
};
use anyhow::Result;
use serde_json::json;
use serial_test::serial;

use crate::common::setup_temp_home;

fn hooks(config: &agent_of_empires::session::Config) -> [Vec<String>; 3] {
    [
        config.hooks.on_create.clone(),
        config.hooks.on_launch.clone(),
        config.hooks.on_destroy.clone(),
    ]
}

/// Each hook type resolves per field: repo over profile over global, and
/// clearing the profile override restores the global value.
#[test]
#[serial]
fn hooks_resolve_per_field_across_global_profile_and_repo() -> Result<()> {
    let _temp = setup_temp_home();
    let s = |v: &str| vec![v.to_string()];

    update_config(|global| {
        global.hooks.on_create = s("global_create");
        global.hooks.on_launch = s("global_launch");
        global.hooks.on_destroy = s("global_destroy");
    })?;
    assert_eq!(
        hooks(&resolve_config("default")?),
        [s("global_create"), s("global_launch"), s("global_destroy")]
    );

    let profile: ProfileConfig = serde_json::from_value(json!({
        "hooks": {"on_create": ["profile_create"], "on_destroy": ["profile_destroy"]}
    }))?;
    save_profile_config("default", &profile)?;
    let resolved = resolve_config("default")?;
    assert_eq!(
        hooks(&resolved),
        [
            s("profile_create"),
            s("global_launch"),
            s("profile_destroy")
        ]
    );

    let repo: RepoConfig = serde_json::from_value(json!({
        "hooks": {"on_launch": ["repo_launch"], "on_destroy": ["repo_destroy"]}
    }))?;
    assert_eq!(
        hooks(&merge_repo_config(resolved, &repo)),
        [s("profile_create"), s("repo_launch"), s("repo_destroy")]
    );

    save_profile_config("default", &ProfileConfig::default())?;
    assert_eq!(
        hooks(&resolve_config("default")?),
        [s("global_create"), s("global_launch"), s("global_destroy")]
    );
    Ok(())
}

//! Profile rename against real profile directories. Create, delete and name
//! validation are covered by the unit tests in `session::mod`.

use agent_of_empires::session::{
    create_profile, list_profiles, rename_profile, set_default_profile, Config, GroupTree,
    Instance, Storage,
};
use anyhow::Result;
use serial_test::serial;

use crate::common::setup_temp_home;

/// Sessions move with the directory, and the default follows only the
/// profile it names.
#[test]
#[serial]
fn rename_profile_moves_sessions_and_tracks_the_default() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("primary")?;
    create_profile("other")?;
    set_default_profile("primary")?;
    let seeded = vec![Instance::new("Test Session", "/path/test")];
    Storage::new_unwatched("other")?.update(|i, g| {
        *i = seeded.to_vec();
        *g = GroupTree::new_with_groups(&seeded, &[]).get_all_groups();
        Ok(())
    })?;

    rename_profile("other", "renamed_other")?;
    let profiles = list_profiles()?;
    assert!(!profiles.contains(&"other".to_string()));
    assert!(profiles.contains(&"renamed_other".to_string()));
    let sessions = Storage::new_unwatched("renamed_other")?.load()?;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title, "Test Session");
    assert_eq!(Config::load()?.default_profile, "primary");

    rename_profile("primary", "renamed")?;
    assert_eq!(Config::load()?.default_profile, "renamed");
    Ok(())
}

#[test]
#[serial]
fn rename_profile_rejects_invalid_requests() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("first")?;
    create_profile("second")?;
    for (from, to, expected) in [
        ("first", "second", "already exists"),
        ("nonexistent", "new_name", "does not exist"),
        ("first", "", "cannot be empty"),
        ("first", "bad/name", "path separators"),
    ] {
        let err = rename_profile(from, to).expect_err(to);
        assert!(err.to_string().contains(expected), "{from} -> {to}: {err}");
    }
    assert!(list_profiles()?.contains(&"first".to_string()));
    Ok(())
}

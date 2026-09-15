//! Regression coverage for process environment restoration on fixture unwind.

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn temporary_home_restores_non_unicode_and_missing_variables_on_unwind() {
    use std::os::unix::ffi::OsStringExt;
    let original = std::ffi::OsString::from_vec(b"caller-home-\xff".to_vec());
    let _caller = crate::common::EnvGuard::new(&["HOME", "XDG_CONFIG_HOME", "AOE_TMUX_SOCKET"])
        .and_set("HOME", &original);
    std::env::remove_var("XDG_CONFIG_HOME");
    let result = std::panic::catch_unwind(|| {
        let home = crate::common::setup_temp_home();
        let app_dir = agent_of_empires::session::get_app_dir().unwrap();
        assert!(app_dir.starts_with(home.path()));
        panic!("fixture body failed");
    });
    assert!(result.is_err());
    assert_eq!(std::env::var_os("HOME"), Some(original));
    assert_eq!(std::env::var_os("XDG_CONFIG_HOME"), None);
}

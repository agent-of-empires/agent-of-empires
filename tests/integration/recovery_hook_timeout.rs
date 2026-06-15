//! Regression coverage for #1265 (hung on_launch hook held the recovery lock).

use std::time::{Duration, Instant};

use agent_of_empires::session::{execute_hooks, HookTimeout, HookTimeoutScope};
use serial_test::serial;
use tempfile::TempDir;

#[test]
#[serial]
fn hung_on_launch_hook_times_out_and_releases_lock_within_grace() {
    let timeout = Duration::from_millis(300);
    let _scope = HookTimeoutScope::new(timeout);

    let project = TempDir::new().expect("tempdir");
    let started = Instant::now();
    let result = execute_hooks(&["sleep 60".to_string()], project.path(), &[]);
    let elapsed = started.elapsed();

    assert!(result.is_err(), "expected timeout Err, got {:?}", result);
    let err_msg = format!("{:#}", result.unwrap_err());
    assert!(
        err_msg.contains("timed out"),
        "expected timeout-shaped error, got: {}",
        err_msg
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "expected timeout to fire within 3s (300ms deadline + kill grace + slow CI cushion), took {:?}",
        elapsed
    );
}

#[test]
#[serial]
fn fast_on_launch_hook_succeeds_inside_timeout_scope() {
    let _scope = HookTimeoutScope::new(Duration::from_secs(5));

    let project = TempDir::new().expect("tempdir");
    let started = Instant::now();
    let result = execute_hooks(&["true".to_string()], project.path(), &[]);
    let elapsed = started.elapsed();

    assert!(result.is_ok(), "fast hook should succeed, got {:?}", result);
    assert!(
        elapsed < Duration::from_secs(1),
        "fast hook should complete promptly, took {:?}",
        elapsed
    );
}

#[test]
#[serial]
fn no_scope_means_no_timeout_for_non_recovery_callers() {
    let project = TempDir::new().expect("tempdir");
    let result = execute_hooks(&["true".to_string()], project.path(), &[]);
    assert!(
        result.is_ok(),
        "no-scope path must remain unchanged, got {:?}",
        result
    );
}

#[test]
#[serial]
fn nested_scopes_restore_outer_timeout_on_drop() {
    let outer = Duration::from_millis(500);
    let inner = Duration::from_millis(100);
    let outer_scope = HookTimeoutScope::new(outer);
    {
        let _inner_scope = HookTimeoutScope::new(inner);
        let project = TempDir::new().expect("tempdir");
        let started = Instant::now();
        let result = execute_hooks(&["sleep 60".to_string()], project.path(), &[]);
        let elapsed = started.elapsed();
        assert!(
            result.is_err(),
            "inner scope must enforce its tighter deadline"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "inner deadline (100ms) must fire fast even on slow CI, took {:?}",
            elapsed
        );
    }
    let project = TempDir::new().expect("tempdir");
    let started = Instant::now();
    let result = execute_hooks(&["sleep 60".to_string()], project.path(), &[]);
    let elapsed = started.elapsed();
    assert!(
        result.is_err(),
        "outer scope must still enforce its deadline"
    );
    assert!(
        elapsed >= Duration::from_millis(400),
        "outer deadline (500ms) must outlive the inner scope, took {:?}",
        elapsed
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "outer deadline must still bound the wait (with slow CI cushion), took {:?}",
        elapsed
    );
    drop(outer_scope);
}

#[test]
#[serial]
fn hook_reading_stdin_does_not_block_under_timeout_scope() {
    let _scope = HookTimeoutScope::new(Duration::from_secs(2));

    let project = TempDir::new().expect("tempdir");
    let started = Instant::now();
    let result = execute_hooks(&["cat".to_string()], project.path(), &[]);
    let elapsed = started.elapsed();
    assert!(
        result.is_ok(),
        "cat must EOF on null stdin and succeed, got {:?}",
        result
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "cat must not block on stdin, took {:?}",
        elapsed
    );
}

#[test]
#[serial]
fn hung_on_launch_hook_emits_typed_hook_timeout_in_error_chain() {
    let timeout = Duration::from_secs(1);
    let _scope = HookTimeoutScope::new(timeout);

    let project = TempDir::new().expect("tempdir");
    let result = execute_hooks(&["sleep 60".to_string()], project.path(), &[]);

    let err = result.expect_err("sleep 60 must time out under a 1s scope");
    let typed = err
        .chain()
        .find_map(|c| c.downcast_ref::<HookTimeout>())
        .unwrap_or_else(|| panic!("expected HookTimeout in chain, got: {err:#}"));
    assert_eq!(
        typed.cmd, "sleep 60",
        "typed error must carry the offending command verbatim",
    );
    assert_eq!(
        typed.timeout_secs, 1,
        "typed error must carry the deadline in seconds",
    );
}

#[test]
#[serial]
fn nonzero_exit_under_scope_is_not_classified_as_hook_timeout() {
    let _scope = HookTimeoutScope::new(Duration::from_secs(5));

    let project = TempDir::new().expect("tempdir");
    let result = execute_hooks(&["false".to_string()], project.path(), &[]);

    let err = result.expect_err("`false` exits non-zero, run_hooks_captured must Err");
    assert!(
        err.chain()
            .all(|c| c.downcast_ref::<HookTimeout>().is_none()),
        "exit-non-zero under a recovery scope must NOT surface as HookTimeout; got: {err:#}",
    );
}

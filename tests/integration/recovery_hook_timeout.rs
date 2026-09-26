//! Regression coverage for #1265 (hung on_launch hook held the recovery lock).

use std::time::{Duration, Instant};

use agent_of_empires::session::{execute_hooks, HookTimeout, HookTimeoutScope};
use serial_test::serial;
use tempfile::TempDir;

/// A hung hook under a recovery scope is killed at the deadline and reported
/// as a typed `HookTimeout` carrying the command and deadline.
#[test]
#[serial]
fn hung_on_launch_hook_times_out_with_typed_error() {
    let _scope = HookTimeoutScope::new(Duration::from_secs(1));

    let project = TempDir::new().expect("tempdir");
    let started = Instant::now();
    let result = execute_hooks(&["sleep 60".to_string()], project.path(), &[]);
    let elapsed = started.elapsed();

    let err = result.expect_err("sleep 60 must time out under a 1s scope");
    assert!(format!("{err:#}").contains("timed out"), "got: {err:#}");
    let typed = err
        .chain()
        .find_map(|c| c.downcast_ref::<HookTimeout>())
        .unwrap_or_else(|| panic!("expected HookTimeout in chain, got: {err:#}"));
    assert_eq!(typed.cmd, "sleep 60");
    assert_eq!(typed.timeout_secs, 1);
    assert!(
        elapsed < Duration::from_secs(4),
        "timeout must fire near the 1s deadline (plus kill grace and CI cushion), took {elapsed:?}"
    );
}

/// Hooks that finish on their own behave the same with or without a scope:
/// success stays success, stdin is closed so `cat` cannot block, and a
/// non-zero exit is an ordinary failure rather than a `HookTimeout`.
#[test]
#[serial]
fn completing_hooks_are_unaffected_by_the_timeout_scope() {
    // (scope, command, succeeds, bound on elapsed)
    let cases = [
        (None, "true", true, Duration::from_secs(1)),
        (Some(5), "true", true, Duration::from_secs(1)),
        (Some(2), "cat", true, Duration::from_millis(500)),
        (Some(5), "false", false, Duration::from_secs(1)),
    ];
    for (scope, cmd, succeeds, bound) in cases {
        let _scope = scope.map(|secs| HookTimeoutScope::new(Duration::from_secs(secs)));
        let project = TempDir::new().expect("tempdir");
        let started = Instant::now();
        let result = execute_hooks(&[cmd.to_string()], project.path(), &[]);
        let elapsed = started.elapsed();

        assert!(elapsed < bound, "{cmd} under {scope:?} took {elapsed:?}");
        match result {
            Ok(()) => assert!(succeeds, "{cmd} under {scope:?} must fail"),
            Err(err) => {
                assert!(!succeeds, "{cmd} under {scope:?}: {err:#}");
                assert!(
                    err.chain()
                        .all(|c| c.downcast_ref::<HookTimeout>().is_none()),
                    "a non-zero exit must not surface as HookTimeout: {err:#}"
                );
            }
        }
    }
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

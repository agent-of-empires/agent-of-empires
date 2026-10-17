//! `aoe serve` must take `serve.lock` before its startup migrations, so it cannot
//! write migrations while `aoe migrate` holds the lock.

use fs2::FileExt;
use serial_test::parallel;

use crate::harness::{app_dir_in, TuiTestHarness};

const VERSION_FILE: &str = ".schema_version";

#[test]
#[parallel]
fn serve_is_refused_before_it_migrates_while_the_lock_is_held() {
    let control = TuiTestHarness::new("serve_lock_control");
    control.run_cli_ok(&["list"]);
    assert!(
        app_dir_in(control.home_path()).join(VERSION_FILE).exists(),
        "an ordinary command must migrate on a fresh home"
    );

    let h = TuiTestHarness::new("serve_lock_held");
    let app_dir = app_dir_in(h.home_path());
    std::fs::create_dir_all(&app_dir).unwrap();
    let lock = std::fs::File::create(app_dir.join("serve.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();

    let stderr = h.run_cli_err(&["serve", "--port", "0"]);
    assert!(stderr.contains("starting or running"), "{stderr}");
    assert!(
        !app_dir.join(VERSION_FILE).exists(),
        "serve ran migrations while the lock was held"
    );
}

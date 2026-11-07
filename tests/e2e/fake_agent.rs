//! The neutral `fake-agent` built-in is detected from a PATH stub and can back a created session.

use serial_test::parallel;

use crate::harness::{init_git_repo, TuiTestHarness};

#[test]
#[parallel]
fn fake_agent_is_detected_and_creatable() {
    let h = TuiTestHarness::new("fake_agent");

    let agents = h.run_cli_ok(&["agents"]);
    let line = agents
        .lines()
        .find(|l| l.contains("fake-agent"))
        .expect("fake-agent listed");
    assert!(!line.contains("not installed"), "{line}");

    let project = h.project_path();
    init_git_repo(&project);
    let id = h.add_session(&[project.to_str().unwrap(), "-t", "fake", "-c", "fake-agent"]);
    assert!(!id.is_empty());
}

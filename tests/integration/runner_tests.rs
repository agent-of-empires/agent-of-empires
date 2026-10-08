use std::path::PathBuf;

#[path = "common/environment.rs"]
mod environment;
mod runner_fixture;
#[cfg(debug_assertions)]
#[path = "common/shim.rs"]
mod shim;

#[cfg(debug_assertions)]
#[path = "acp_runner_control.rs"]
mod control;
#[cfg(debug_assertions)]
#[path = "acp_midturn_resume.rs"]
mod midturn;
#[path = "acp_runner_orphan.rs"]
mod orphan;
#[cfg(debug_assertions)]
#[path = "acp_silent_orphan.rs"]
mod silent;

fn aoe_binary() -> PathBuf {
    let test = std::env::current_exe().expect("current test executable");
    let directory = test.parent().expect("test executable directory");
    let profile = if directory.file_name().is_some_and(|name| name == "deps") {
        directory.parent().expect("Cargo profile directory")
    } else {
        directory
    };
    let binary = profile.join(format!("aoe{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.is_file(),
        "build the aoe binary with this test feature set before running native runner tests: {}",
        binary.display()
    );
    binary
}

#[cfg(debug_assertions)]
impl environment::EnvGuard {
    pub(crate) fn from_pairs(pairs: &[(&'static str, &'static str)]) -> Self {
        let mut guard = Self::new(&[]);
        for (key, value) in pairs {
            guard.set(key, value);
        }
        guard
    }
}

fn isolated_case(module: &str, name: &str) -> bool {
    let module = module.split_once("::").expect("test module path").1;
    let case = format!("{module}::{name}");
    if std::env::var("AOE_ISOLATED_RUNNER_TEST").ok().as_deref() == Some(&case) {
        let path =
            std::env::var_os("AOE_ISOLATED_CASE_ACK").expect("scenario entry acknowledgement");
        std::fs::write(path, &case).expect("acknowledge actual scenario entry");
        return true;
    }
    let root = tempfile::tempdir().expect("isolated scenario home");
    let acknowledgement = root.path().join("entered");
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", &case, "--nocapture"])
        .env("AOE_ISOLATED_RUNNER_TEST", &case)
        .env("AOE_ISOLATED_CASE_ACK", &acknowledgement)
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join(".config"));
    #[cfg(debug_assertions)]
    if let Ok(node) = shim::shim_node() {
        command.env("AOE_ISOLATED_NODE_BIN", node);
    }
    let status = command.status().expect("run isolated native scenario");
    assert!(
        status.success(),
        "isolated native scenario {case} failed: {status}"
    );
    assert_eq!(
        std::fs::read_to_string(acknowledgement).expect("actual scenario entry"),
        case
    );
    false
}

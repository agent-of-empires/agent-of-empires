//! Generic container probes: how a container exec is built, and the environment
//! a running container reports for its agent.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};

/// Upper bound on a container exec that only reports state.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONTAINER_ENV_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn container_exec_command(
    container_name: &str,
    runtime_name: Option<crate::session::config::ContainerRuntimeName>,
    snapshot: Option<&crate::containers::RuntimeExecutionSnapshot>,
    argv: &[&str],
) -> std::process::Command {
    use crate::session::config::ContainerRuntimeName;
    let command_argv = argv
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    if let Some(snapshot) = snapshot {
        return snapshot.exec(container_name, "", &command_argv);
    }
    let runtime = match runtime_name {
        Some(ContainerRuntimeName::AppleContainer) => {
            crate::containers::ContainerRuntime::apple_container()
        }
        Some(ContainerRuntimeName::Docker) => crate::containers::ContainerRuntime::docker(),
        Some(ContainerRuntimeName::Podman) => crate::containers::ContainerRuntime::podman(),
        None => crate::containers::get_container_runtime(),
    };
    let exec_argv = runtime.build_exec_argv(container_name, "", &command_argv);
    let mut command = std::process::Command::new(&exec_argv[0]);
    command.args(&exec_argv[1..]);
    command
}

pub(crate) fn read_container_environment(
    runtime: &crate::containers::RuntimeExecutionSnapshot,
    container_name: &str,
) -> Result<HashMap<String, String>> {
    let command = container_exec_command(container_name, None, Some(runtime), &["env", "-0"]);
    let output = super::run_with_timeout_limit(
        command,
        COMMAND_TIMEOUT,
        "container exec (native environment probe)",
        MAX_CONTAINER_ENV_BYTES,
    )?;
    let text = String::from_utf8(output).context("Container environment is not UTF-8")?;
    let mut values = HashMap::new();
    for line in text.split(char::from(0)) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if crate::session::environment::is_valid_env_key(key) {
            values.insert(key.to_string(), value.to_string());
        }
    }
    Ok(values)
}

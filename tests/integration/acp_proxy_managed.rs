//! Real proxy handshakes, adapter-frame ordering across retained attachment,
//! and loss of a proxy after its Native prompt was written.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::managed::{shim_node, shim_path, shim_ready, with_managed, ManagedClient};
use super::runner_config;
use crate::acp::acp_client::{ProxyFailurePolicy, ProxySpec, SpawnConfig};
use crate::acp::agent_compat::ProxyCompat;
use crate::acp::state::{Event, IntegrationLostReason, StartupErrorDetail};
use crate::process::runner::test_support::managed_runner::RunnerInspection;

fn proxy_config(cwd: &Path) -> SpawnConfig {
    let node = shim_node()
        .expect("shim prerequisite")
        .to_string_lossy()
        .into_owned();
    let shim = shim_path();
    let mut config = runner_config(PathBuf::new());
    config.cwd = cwd.to_owned();
    config.config_path = cwd.to_owned();
    config.spec.command = node.clone();
    config.spec.args = vec![shim.to_string_lossy().into_owned()];
    config.proxy = Some(ProxySpec {
        plugin_id: "fake-proxy".into(),
        command: node,
        args: vec![
            shim.with_file_name("fake-proxy.mjs")
                .to_string_lossy()
                .into_owned(),
            "--".into(),
        ],
        failure_policy: ProxyFailurePolicy::Refuse,
        display_name: "Fake".into(),
        compat: ProxyCompat::default(),
    });
    config
}

/// Write `(text, after_adapter_sequence)` frames; `None` stands for adapter output.
fn emit(file: &Path, sid: &str, frames: &[(&str, Option<u64>)]) {
    let lines: String = frames
        .iter()
        .map(|(text, after)| {
            let mut update = serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            });
            if let Some(after) = after {
                update["_meta"] = serde_json::json!({
                    "aoe": {"v": 1, "key": text, "after_adapter_sequence": after},
                });
            }
            let frame = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": sid, "update": update},
            });
            format!("{frame}\n")
        })
        .collect();
    let staged = file.with_extension("staged");
    std::fs::write(&staged, lines).unwrap();
    std::fs::rename(staged, file).unwrap();
}

async fn wait_for_detached_queue(inspection: &RunnerInspection, count: u64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = inspection.query("snapshot").await;
            if snapshot["attached"] == false && snapshot["queued_frames"] == count {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runner detached with the expected replay backlog");
}

async fn next_chunk(client: &mut ManagedClient) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Event::AgentMessageChunk { text, .. } =
                client.next_event().await.expect("event stream open")
            {
                return text;
            }
        }
    })
    .await
    .expect("expected proxy or adapter frame")
}

/// A v9 SessionReady starts at the undeliverable adapter-frame baseline.
/// Detached replay is counted in order, not again in that baseline.
#[tokio::test]
#[serial_test::serial]
async fn a_reattach_keeps_counting_the_adapters_frames() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    with_managed(|scope| {
        Box::pin(async move {
            let dir = scope.tempdir();
            let file = dir.path().join("emit");
            let name = std::thread::current().name().unwrap().to_owned();
            let inspection = scope.inspect_next_runner(&name);
            let mut config = proxy_config(dir.path());
            config.host_environment.push((
                "FAKE_PROXY_EMIT_FILE".into(),
                file.to_string_lossy().into_owned(),
            ));
            let mut client = scope.spawn(config, "proxy-reattach-count").await.unwrap();
            client.ready().await;
            let sid = client.selected_native_session();
            let owner = client
                .connected_runner()
                .expect("actual original proxy runner");
            emit(&file, &sid, &[("a1", None), ("a2", None)]);
            assert_eq!(next_chunk(&mut client).await, "a1");
            assert_eq!(next_chunk(&mut client).await, "a2");

            // Quiet adapter: an anchor behind the count is delivered at once.
            let mut client = scope
                .adopt_with(client, false, || wait_for_detached_queue(&inspection, 0))
                .await;
            client.ready().await;
            assert_eq!(client.connected_runner(), Some(owner));
            assert_eq!(client.selected_native_session(), sid);
            emit(&file, &sid, &[("t2", Some(2))]);
            assert_eq!(next_chunk(&mut client).await, "t2");

            // An anchor ahead of the count waits for exactly the next adapter frame.
            emit(&file, &sid, &[("t3", Some(3)), ("a3", None)]);
            assert_eq!(next_chunk(&mut client).await, "a3");
            assert_eq!(next_chunk(&mut client).await, "t3");

            let mut client = scope
                .adopt_with(client, false, || async {
                    wait_for_detached_queue(&inspection, 0).await;
                    emit(&file, &sid, &[("a4", None), ("t5", Some(5))]);
                    wait_for_detached_queue(&inspection, 2).await;
                })
                .await;
            client.ready().await;
            assert_eq!(client.connected_runner(), Some(owner));
            assert_eq!(client.selected_native_session(), sid);
            assert_eq!(next_chunk(&mut client).await, "a4");
            emit(&file, &sid, &[("a5", None)]);
            assert_eq!(next_chunk(&mut client).await, "a5");
            assert_eq!(next_chunk(&mut client).await, "t5");
            client.shutdown().await.unwrap();
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn a_bad_or_missing_hello_is_refused() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    with_managed(|scope| {
        Box::pin(async move {
            for mode in ["bad-hello", "no-hello"] {
                let dir = scope.tempdir();
                let mut config = proxy_config(dir.path());
                config
                    .host_environment
                    .push(("FAKE_PROXY_MODE".into(), mode.into()));
                let (id, mut events) = scope.create_observed(config, mode).await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let frame = events.recv().await.expect("observer open");
                        if frame.session_id != id {
                            continue;
                        }
                        match frame.event.as_ref() {
                            Event::IncompatibleAgent { detail } => {
                                assert!(
                                    matches!(detail, StartupErrorDetail::InvalidProxyHello { .. }),
                                    "{mode}: {detail:?}",
                                );
                                break;
                            }
                            Event::AcpSessionAssigned { .. } => {
                                panic!("{mode}: refused proxy became ready");
                            }
                            _ => {}
                        }
                    }
                })
                .await
                .expect("invalid proxy hello diagnosed");
            }
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn killing_the_proxy_mid_turn_loses_the_integration() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    with_managed(|scope| {
        Box::pin(async move {
            let dir = scope.tempdir();
            let mut config = proxy_config(dir.path());
            // The shim streams, then waits on a file that never appears.
            config.host_environment.push((
                "SHIM_PROMPT_COMPLETION_RELEASE_FILE".into(),
                dir.path().join("never").to_string_lossy().into_owned(),
            ));
            let mut client = scope.spawn(config, "proxy-mid-turn").await.unwrap();
            client.ready().await;
            assert!(
                client.verified_proxy(),
                "initialize authenticated the test proxy"
            );
            let proof = scope.original_native_proof(&client.lease);
            let proxy = crate::process::CapturedProcess::capture(proof.pid, &proof.birth)
                .expect("capture the actual runner child")
                .expect("the original proxy is still alive");
            client
                .send_prompt("hello", &[])
                .await
                .expect("write native prompt");
            tokio::time::timeout(Duration::from_secs(20), async {
                let mut killed = false;
                while let Some(event) = client.next_event().await {
                    match event {
                        Event::AgentMessageChunk { ref text, .. } if text == "done" && !killed => {
                            proxy
                                .signal(libc::SIGKILL)
                                .expect("kill the original proxy");
                            killed = true;
                        }
                        Event::IntegrationLost { reason } => {
                            assert!(killed, "loss preceded the proxy exit stimulus");
                            assert_eq!(reason, IntegrationLostReason::ProxyExited);
                            return;
                        }
                        Event::AgentStartupError { message, .. } => {
                            panic!("a proxy exit is not a startup error: {message}");
                        }
                        _ => {}
                    }
                }
                panic!("event stream closed without proxy loss");
            })
            .await
            .expect("the proxy exit is reported");
        })
    })
    .await;
}

//! Private libtest child hosting the unchanged production runner and its real native child.

use super::{control_protocol, ControlBody, DeliveryScope, QueuedKind, RunnerShared};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PRODUCER_EVENT_LIMIT: usize = 64;
const PRODUCER_BYTE_LIMIT: usize = 256 * 1024;

#[derive(Clone)]
pub(super) struct ObservedOriginalPrompt {
    pub(super) attachment: u64,
    pub(super) params: serde_json::Value,
    pub(super) scope: crate::acp::establishment::NativeRequestScope,
}

#[derive(Default)]
pub(super) struct ProducerProbe {
    events: Vec<serde_json::Value>,
    bytes: usize,
    pub(super) original: Option<ObservedOriginalPrompt>,
}

impl ProducerProbe {
    pub(super) fn push(&mut self, event: serde_json::Value) {
        let bytes = serde_json::to_vec(&event).unwrap().len();
        assert!(
            self.events.len() < PRODUCER_EVENT_LIMIT,
            "bounded producer observation overflow"
        );
        assert!(
            self.bytes + bytes <= PRODUCER_BYTE_LIMIT,
            "bounded producer observation byte overflow"
        );
        self.bytes += bytes;
        self.events.push(event);
    }
}
const CHILD_ROLE: &str = "AOE_MANAGED_RUNNER_CHILD";

tokio::task_local! {
    pub(crate) static OBSERVE_RUNNER: tokio::sync::mpsc::UnboundedSender<(
        Arc<RunnerShared>,
        Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    )>;
    pub(crate) static OBSERVE_CONTROL: tokio::sync::mpsc::UnboundedSender<(u64, std::os::fd::OwnedFd)>;
    static NATIVE_BIRTH_FILE: PathBuf;
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ChildRole {
    test_name: String,
    argv: PathBuf,
    socket: PathBuf,
    native_birth: PathBuf,
}

/// Written only by the actual native parent while it still owns the unreaped Child.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSpawnProof {
    pub(crate) session_id: String,
    pub(crate) producer: uuid::Uuid,
    pub(crate) runner: crate::acp::runner_lifecycle::RunnerIdentity,
    pub(crate) pid: u32,
    pub(crate) birth: crate::process::ProcessBirthIdentity,
}

pub(crate) fn observe_native_spawn(
    args: &crate::process::runner::AcpRunnerArgs,
    record: &crate::process::worker_registry::WorkerRecord,
    child: &tokio::process::Child,
) -> anyhow::Result<()> {
    NATIVE_BIRTH_FILE
        .try_with(|path| -> anyhow::Result<()> {
            use std::io::Write;
            let pid = child
                .id()
                .ok_or_else(|| anyhow::anyhow!("native Child has no original PID"))?;
            let birth = crate::process::process_birth_identity(pid)?
                .ok_or_else(|| anyhow::anyhow!("native Child original birth is unavailable"))?;
            let runner = crate::acp::runner_lifecycle::RunnerIdentity::from_record(record);
            anyhow::ensure!(
                runner.pid == std::process::id() && runner.birth_fingerprint.is_some(),
                "native birth observation requires its actual parent owner"
            );
            let proof = NativeSpawnProof {
                session_id: args.session_id.clone(),
                producer: args.establishment_producer,
                runner,
                pid,
                birth,
            };
            let mut file =
                tempfile::NamedTempFile::new_in(path.parent().expect("private birth directory"))?;
            file.write_all(&serde_json::to_vec(&proof)?)?;
            file.persist_noclobber(path)?;
            Ok(())
        })
        .unwrap_or(Ok(()))
}

#[derive(Clone)]
pub(crate) struct RunnerInspection {
    socket: PathBuf,
    native_birth: PathBuf,
}

impl RunnerInspection {
    pub(crate) fn prepare(directory: &Path, test_name: &str) -> (PathBuf, Self) {
        use std::os::unix::fs::PermissionsExt;
        let role = ChildRole {
            test_name: test_name.into(),
            argv: directory.join("runner-argv"),
            socket: directory.join("inspect.sock"),
            native_birth: directory.join("native-birth.json"),
        };
        let manifest = directory.join("runner-role.json");
        std::fs::write(&manifest, serde_json::to_vec(&role).unwrap()).unwrap();
        let executable = directory.join("runner-libtest");
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        std::fs::write(&executable, format!(
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > {}\nexport {CHILD_ROLE}={}\nexec {} --exact {} --nocapture\n",
            quote(role.argv.to_str().unwrap()), quote(manifest.to_str().unwrap()),
            quote(std::env::current_exe().unwrap().to_str().unwrap()), quote(test_name),
        )).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        (
            executable,
            Self {
                socket: role.socket,
                native_birth: role.native_birth,
            },
        )
    }

    fn read_native_spawn_proof(&self) -> anyhow::Result<Option<NativeSpawnProof>> {
        let bytes = match std::fs::read(&self.native_birth) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    pub(crate) fn native_spawn_proof(
        &self,
        lease: &crate::acp::runner_lifecycle::Lease,
    ) -> anyhow::Result<Option<NativeSpawnProof>> {
        let Some(proof) = self.read_native_spawn_proof()? else {
            return Ok(None);
        };
        anyhow::ensure!(
            proof.session_id == lease.session_id()
                && proof.producer == lease.producer_id()
                && Some(proof.runner) == lease.spawned_runner()
                && proof.runner.birth_fingerprint.is_some(),
            "native birth proof belongs to a different launch/owner"
        );
        Ok(Some(proof))
    }

    /// The caller still owns this unreaped child, including on failed startup.
    pub(crate) fn native_spawn_proof_for_child(
        &self,
        child: &std::process::Child,
        session_id: &str,
        producer: uuid::Uuid,
    ) -> anyhow::Result<Option<NativeSpawnProof>> {
        let Some(proof) = self.read_native_spawn_proof()? else {
            return Ok(None);
        };
        let birth = crate::process::process_birth_identity(child.id())?;
        anyhow::ensure!(
            proof.session_id == session_id
                && proof.producer == producer
                && proof.runner.pid == child.id()
                && proof.runner.birth_fingerprint.is_some()
                && proof.runner.birth_fingerprint
                    == birth.as_ref().map(|birth| birth.fingerprint()),
            "native birth proof belongs to a different original child"
        );
        Ok(Some(proof))
    }

    pub(crate) async fn query(&self, command: &str) -> serde_json::Value {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut stream = loop {
                match tokio::net::UnixStream::connect(&self.socket).await {
                    Ok(stream) => break stream,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    Err(error) => panic!("private runner observation connection: {error}"),
                }
            };
            stream
                .write_all(format!("{command}\n").as_bytes())
                .await
                .unwrap();
            let mut response = String::new();
            assert_ne!(
                BufReader::new(stream)
                    .read_line(&mut response)
                    .await
                    .unwrap(),
                0
            );
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert!(
                response.get("error").is_none(),
                "private observation failed: {response}"
            );
            response
        })
        .await
        .expect("real child runner answers private observation")
    }
}

async fn observe_pending_original(shared: &RunnerShared) -> serde_json::Value {
    use crate::acp::establishment::NativeRequestScope;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = {
                let handshake = shared.handshake.lock().await;
                let cache = handshake
                    .session
                    .as_ref()
                    .expect("actual original native cache");
                assert!(cache.result.is_none() && !cache.collector_finished);
                let native = shared.native_channel.lock().unwrap();
                assert!(native.loss.is_none());
                assert!(native
                    .receipt
                    .as_ref()
                    .is_some_and(|receipt| Arc::ptr_eq(receipt, &cache.receipt)));
                let mut originals = native.calls.iter().filter(|(_, call)| {
                    let super::NativeReplyTarget::Handshake(_) = &call.target else {
                        return false;
                    };
                    let NativeRequestScope::Original {
                        attempt,
                        owner,
                        store,
                        ..
                    } = &call.scope
                    else {
                        return false;
                    };
                    attempt.as_ref() == cache.receipt.attempt.as_ref()
                        && *owner == cache.receipt.owner
                        && cache.receipt.same_store(store.as_deref())
                });
                let (request_id, call) = originals
                    .next()
                    .expect("actual original establishment waiter");
                assert!(
                    originals.next().is_none(),
                    "one original establishment waiter"
                );
                if matches!(call.write, super::NativeWriteProgress::Complete) {
                    let super::NativeReplyTarget::Handshake(sender) = &call.target else {
                        unreachable!()
                    };
                    assert!(
                        !sender.is_closed(),
                        "original collector still owns its receiver"
                    );
                    assert!(call
                        .receipt
                        .as_ref()
                        .is_some_and(|receipt| Arc::ptr_eq(receipt, &cache.receipt)));
                    Some(serde_json::json!({
                        "request_id":request_id, "scope":&call.scope, "receipt":&cache.receipt,
                    }))
                } else {
                    None
                }
            };
            if let Some(snapshot) = snapshot {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual original Native write completes before inspection")
}

async fn outbound_observation(shared: &RunnerShared) -> serde_json::Value {
    use sha2::{Digest, Sha256};
    let channel = shared.control.lock().await;
    let entries: Vec<_> = channel
        .queue
        .iter()
        .map(|entry| {
            let digest: [u8; 32] = Sha256::digest(entry.wire.as_ref()).into();
            serde_json::json!({
                "id":entry.id, "kind":format!("{:?}", entry.kind),
                "wire_bytes":entry.wire.len(), "wire_sha256":digest,
            })
        })
        .collect();
    let native = shared.native_channel.lock().unwrap();
    serde_json::json!({
        "attached":channel.active_attachment.is_some(),
        "attachment":channel.active_attachment,
        "callbacks_announced":channel.callbacks_announced,
        "in_flight":channel.in_flight, "entries":entries,
        "native_dispatching":native.dispatching, "native_loss":native.loss,
    })
}

/// Called only at the beginning of the same actual test selected by `--exact`.
/// The production detached-spawn path owns/reaps this child through its real lease.
pub(crate) async fn child_if_requested(test_name: &str) -> bool {
    use clap::Parser;
    use std::os::unix::ffi::OsStringExt;
    let Some(manifest) = std::env::var_os(CHILD_ROLE) else {
        return false;
    };
    let role: ChildRole = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    assert_eq!(
        role.test_name, test_name,
        "child role cannot enter another test"
    );
    #[derive(clap::Parser)]
    struct RunnerCli {
        #[command(flatten)]
        args: crate::process::runner::AcpRunnerArgs,
    }
    let argv = std::fs::read(&role.argv).unwrap();
    let argv = argv
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| std::ffi::OsString::from_vec(arg.to_vec()));
    let args = RunnerCli::parse_from(argv).args;
    let listener = tokio::net::UnixListener::bind(&role.socket).unwrap();
    let (observer, mut observed) = tokio::sync::mpsc::unbounded_channel::<(
        Arc<RunnerShared>,
        Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    )>();
    let (control_observer, mut controls) =
        tokio::sync::mpsc::unbounded_channel::<(u64, std::os::fd::OwnedFd)>();
    let mut inspection = tokio::spawn(async move {
        let (shared, agent_stdin) = observed
            .recv()
            .await
            .expect("production actor publishes actual shared state and stdin");
        let mut stdin_hold: Option<tokio::sync::OwnedMutexGuard<tokio::process::ChildStdin>> = None;
        let mut control_socket = None;
        loop {
            let (stream, _) = loop {
                tokio::select! {
                    accepted = listener.accept() => break accepted.unwrap(),
                    observed = controls.recv() => control_socket = Some(observed.expect("actual actor control observation")),
                }
            };
            let (read, mut write) = stream.into_split();
            let mut command = String::new();
            BufReader::new(read).read_line(&mut command).await.unwrap();
            while let Ok(observed) = controls.try_recv() {
                control_socket = Some(observed);
            }
            let response = match command.trim() {
                "snapshot" => {
                    let channel = shared.control.lock().await;
                    let mut notifications = 0;
                    let mut completions = Vec::new();
                    for (index, frame) in channel.queue.iter().enumerate() {
                        if frame.kind == QueuedKind::Notify {
                            notifications += 1;
                        } else if matches!(frame.kind, QueuedKind::PromptCompleted(_)) {
                            let ControlBody::PromptCompleted {
                                prompt_req_id,
                                outcome,
                                ..
                            } = serde_json::from_slice(&frame.wire[4..])
                                .expect("actual queued completion frame")
                            else {
                                panic!("completion queue kind must match its actual wire frame");
                            };
                            completions.push((
                                prompt_req_id,
                                index,
                                notifications,
                                matches!(
                                    outcome,
                                    control_protocol::PromptOutcome::Error { code: -32603, .. }
                                ),
                            ));
                        }
                    }
                    serde_json::json!({
                        "queued_frames": channel.queue.len(),
                        "queued_bytes": channel.queued_bytes,
                        "attached": channel.active_attachment.is_some(),
                        "retained_bytes": channel.last_prompt_completion.as_ref().map_or(0, |wire| wire.len()),
                        "queued_prompt_completions": completions,
                    })
                }
                "fill-retained-backlog" => fill_retained_backlog(&shared).await,
                "close-control" => {
                    use std::os::fd::AsRawFd;
                    let active = shared.control.lock().await.active_attachment;
                    let (attachment, socket) = control_socket
                        .as_ref()
                        .expect("the actual current native peer socket");
                    assert_eq!(
                        active,
                        Some(*attachment),
                        "never close another attachment's socket"
                    );
                    // The duplicate owns this exact socket, not a reusable raw descriptor.
                    assert_eq!(
                        unsafe { libc::shutdown(socket.as_raw_fd(), libc::SHUT_RDWR) },
                        0
                    );
                    serde_json::json!({"closed_attachment":attachment})
                }
                "observe-producer" => {
                    let mut probe = shared.test_producer.lock().unwrap();
                    assert!(probe.is_none(), "observation starts once per actual runner");
                    *probe = Some(ProducerProbe::default());
                    serde_json::json!({"observing": true})
                }
                "pending-original" => observe_pending_original(&shared).await,
                "outbound-observation" => outbound_observation(&shared).await,
                "producer-snapshot" => {
                    let events = shared
                        .test_producer
                        .lock()
                        .unwrap()
                        .as_ref()
                        .expect("producer observation enabled")
                        .events
                        .clone();
                    let native = shared.native_channel.lock().unwrap();
                    let pending: Vec<_> = native
                        .calls
                        .iter()
                        .map(|(id, call)| {
                            let progress = match call.write {
                                super::NativeWriteProgress::Pending => "pending",
                                super::NativeWriteProgress::Prefix(_) => "prefix",
                                super::NativeWriteProgress::Complete => "complete",
                            };
                            serde_json::json!({"id": id, "progress": progress})
                        })
                        .collect();
                    serde_json::json!({
                        "events": events,
                        "pending": pending,
                        "loss": native.loss,
                        "attachment": native.active_attachment,
                    })
                }
                "hold-native-stdin" => {
                    assert!(stdin_hold.is_none());
                    stdin_hold = Some(Arc::clone(&agent_stdin).lock_owned().await);
                    serde_json::json!({"held": true})
                }
                "release-native-stdin" => {
                    drop(stdin_hold.take().expect("actual stdin was held"));
                    serde_json::json!({"held": false})
                }
                "retry-original-from-detached-attachment" => {
                    let original = shared
                        .test_producer
                        .lock()
                        .unwrap()
                        .as_ref()
                        .expect("producer observation enabled")
                        .original
                        .clone()
                        .expect("capture comes from an actual original prompt");
                    assert_ne!(
                        shared.control.lock().await.active_attachment,
                        Some(original.attachment),
                        "negative stale request only"
                    );
                    let result = shared
                        .agent_prompt(
                            &agent_stdin,
                            original.attachment,
                            original.params,
                            original.scope,
                        )
                        .await;
                    serde_json::json!({"result": result})
                }
                unknown => {
                    serde_json::json!({"error": format!("unknown private fixture operation {unknown}")})
                }
            };
            write
                .write_all(&serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
            write.write_all(b"\n").await.unwrap();
        }
    });
    let actor = OBSERVE_RUNNER.scope(
        observer,
        OBSERVE_CONTROL.scope(
            control_observer,
            NATIVE_BIRTH_FILE.scope(role.native_birth, crate::process::runner::run(args)),
        ),
    );
    tokio::pin!(actor);
    tokio::select! {
        result = &mut actor => result.expect("actual child runner finishes"),
        result = &mut inspection => {
            // The parent owns the qualified child, but an inspection failure must
            // also let the real actor perform native cleanup before this test exits.
            let pid = nix::unistd::Pid::from_raw(std::process::id() as i32);
            nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
            let _ = actor.await;
            panic!("private runner inspection stopped: {result:?}");
        }
    }
    inspection.abort();
    let _ = inspection.await;
    true
}

async fn fill_retained_backlog(shared: &RunnerShared) -> serde_json::Value {
    let session_id = shared
        .handshake
        .lock()
        .await
        .session
        .as_ref()
        .and_then(|cache| cache.receipt.session_id())
        .expect("genuine native establishment result")
        .to_owned();
    let notification = |bytes: usize| {
        let mut body = ControlBody::Notify {
            method: "session/update".into(),
            params: serde_json::json!({"sessionId":session_id,"update":{
                "sessionUpdate":"tool_call","toolCallId":"later","title":"later update"
            },"_meta":{"pad":""}}),
            replay: None,
            evidence: control_protocol::NativeNotifyEvidence::default(),
        };
        let overhead = control_protocol::encode_frame(&body).unwrap().len();
        if let ControlBody::Notify { params, .. } = &mut body {
            params["_meta"]["pad"] = serde_json::Value::String("x".repeat(bytes - overhead));
        }
        Arc::<[u8]>::from(control_protocol::encode_frame(&body).unwrap())
    };
    let wire = notification(1024 * 1024);
    let mut channel = shared.control.lock().await;
    assert!(
        channel.active_attachment.is_none(),
        "budget setup is detached, not racing a consumer"
    );
    assert!(
        channel.queue.is_empty(),
        "the previous real control write committed its completion"
    );
    let retained_bytes = channel
        .last_prompt_completion
        .as_ref()
        .expect("real correlated native completion was retained")
        .len();
    assert!(retained_bytes > 16 * 1024);
    for _ in 0..127 {
        assert!(channel.make_room(QueuedKind::Notify, wire.len()));
        channel.push(
            DeliveryScope::Persistent,
            QueuedKind::Notify,
            Arc::clone(&wire),
        );
    }
    let tail_bytes = control_protocol::MAX_CONTROL_QUEUE_BYTES
        - control_protocol::MAX_NATIVE_LOSS_FRAME_BYTES
        - 16 * 1024
        - channel.queued_bytes;
    let tail = notification(tail_bytes);
    assert!(channel.make_room(QueuedKind::Notify, tail.len()));
    channel.push(DeliveryScope::Persistent, QueuedKind::Notify, tail);
    assert!(
        channel.queued_bytes + retained_bytes + control_protocol::MAX_NATIVE_LOSS_FRAME_BYTES
            <= control_protocol::MAX_CONTROL_QUEUE_BYTES
    );
    assert_eq!(
        channel.queue.len(),
        127,
        "shed a notification, never the genuine completion"
    );
    serde_json::json!({
        "queued_frames": channel.queue.len(), "queued_bytes": channel.queued_bytes,
        "retained_bytes": retained_bytes,
        "loss_reserve": control_protocol::MAX_NATIVE_LOSS_FRAME_BYTES,
        "byte_limit": control_protocol::MAX_CONTROL_QUEUE_BYTES,
    })
}

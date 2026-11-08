//! Raw owned-native fixtures. No SDK projection, historical seal, or Applied permission.

use super::shared::RunnerShared;
use crate::acp::control_protocol::{ControlBody, NativeChannelLossCause};
use crate::acp::establishment::{EstablishmentAttempt, EstablishmentRecovery};
use crate::process::worker_registry::{self, WorkerRecord};
use crate::session::{
    ConversationBinding, ConversationProvenance, ExecutionBinding, Instance, Status, Storage, View,
};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

pub(crate) use super::shared::managed_runner;

#[cfg(unix)]
#[path = "../../../tests/integration/acp_runner_orphan.rs"]
mod acp_runner_orphan;

struct NativeChild(Child);

impl Drop for NativeChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Commit through the real selected-target factory, under its native exclusion.
pub(super) fn arm_selected(
    storage: &Storage,
    id: &str,
    producer: uuid::Uuid,
    recovery: Option<&EstablishmentRecovery>,
) -> Arc<EstablishmentAttempt> {
    let _lifecycle = storage.acquire_instance_lifecycle_lock(id).unwrap();
    let mut prepared = None;
    storage
        .update(|rows, _| {
            let row = rows.iter_mut().find(|row| row.id == id).unwrap();
            let attempt = row.arm_acp_establishment(storage, recovery)?;
            prepared = Some(row.bind_acp_producer(storage, &attempt, producer)?);
            Ok(())
        })
        .unwrap();
    Arc::new(prepared.expect("selected establishment was committed"))
}

/// Callers serialize environment mutation with `serial_test::serial`.
/// The runner owner is this actual process; `cat` is its controlled native pipe.
pub(crate) struct OwnedNativeFixture {
    pub(super) shared: Arc<RunnerShared>,
    pub(super) stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    pub(crate) attempt: Arc<EstablishmentAttempt>,
    pub(crate) store: ExecutionBinding,
    pub(crate) record: WorkerRecord,
    pub(crate) storage: Storage,
    output: BufReader<tokio::process::ChildStdout>,
    attachment: Option<u64>,
    observer: Option<tokio::task::JoinHandle<()>>,
    child: NativeChild,
    _project: tempfile::TempDir,
    _home: crate::session::test_support::AppDirGuard,
}

impl OwnedNativeFixture {
    pub(crate) fn load(sid: &str) -> Self {
        Self::selected(sid, false)
    }

    pub(crate) fn fork(parent: &str) -> Self {
        Self::selected(parent, true)
    }

    fn selected(sid: &str, fork: bool) -> Self {
        assert!(!sid.is_empty());
        let home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let native_store = project.path().join("native");
        std::fs::create_dir(&native_store).unwrap();
        let storage = Storage::new_unwatched("default").unwrap();
        let store = ExecutionBinding {
            agent: "claude".into(),
            stores: vec![native_store.clone()],
            configuration: Vec::new(),
            cwd: project.path().to_owned(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
            exported_default_store: Some(false),
        };
        let mut row = Instance::new("owned raw native", project.path().to_str().unwrap());
        row.source_profile = storage.profile().into();
        row.view = View::Structured;
        row.status = Status::Idle;
        row.lifecycle_generation = 5;
        row.agent_name = Some("claude".into());
        row.resume_binding = Some(ConversationBinding {
            session_id: sid.into(),
            execution: Some(store.clone()),
            provenance: ConversationProvenance::Asserted,
            transcript_path: None,
        });
        if fork {
            row.fork_pending = Some(sid.into());
        } else {
            row.acp_session_id = Some(sid.into());
        }
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let attempt = arm_selected(&storage, &row.id, uuid::Uuid::new_v4(), None);
        let mut record = WorkerRecord::new(
            row.id.clone(),
            std::process::id(),
            worker_registry::socket_path_for(&row.id).unwrap(),
            "cat".into(),
            "claude".into(),
            project.path().to_owned(),
            None,
            Vec::new(),
            Vec::new(),
            row.acp_session_id.clone(),
            Some(row.source_profile.clone()),
        )
        .with_generation(71);
        record.control_protocol_version =
            Some(crate::acp::control_protocol::CONTROL_PROTOCOL_VERSION);
        assert!(
            record.runner_identity.is_some(),
            "actual runner birth is required"
        );
        worker_registry::save(&record).unwrap();
        let mut child = NativeChild(
            Command::new("cat")
                .current_dir(project.path())
                .env("CLAUDE_CONFIG_DIR", &native_store)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let stdin = tokio::process::ChildStdin::from_std(child.0.stdin.take().unwrap()).unwrap();
        let output = tokio::process::ChildStdout::from_std(child.0.stdout.take().unwrap()).unwrap();
        Self {
            shared: Arc::new(RunnerShared::new(Some((row.id, record.pid)))),
            stdin: Arc::new(Mutex::new(stdin)),
            attempt,
            store,
            record,
            storage,
            output: BufReader::new(output),
            attachment: None,
            observer: None,
            child,
            _project: project,
            _home: home,
        }
    }

    pub(crate) fn establish_frame(&self) -> ControlBody {
        let sid = match &self.attempt.target {
            crate::acp::establishment::EstablishmentTarget::Load { session_id, .. }
            | crate::acp::establishment::EstablishmentTarget::Fork { session_id } => session_id,
            crate::acp::establishment::EstablishmentTarget::New => {
                unreachable!("this fixture only selects existing native conversations")
            }
        };
        ControlBody::EstablishSession {
            method: self.attempt.target.method().into(),
            request: serde_json::json!({"sessionId": sid}),
            attempt: Arc::clone(&self.attempt),
            store: Some(self.store.clone()),
        }
    }

    pub(crate) async fn begin(&mut self) {
        assert!(
            self.attachment.is_none(),
            "one original invocation per fixture"
        );
        let attachment = self.shared.begin_attachment().await;
        self.attachment = Some(attachment);
        let ControlBody::EstablishSession {
            method,
            request,
            attempt,
            store,
        } = self.establish_frame()
        else {
            unreachable!()
        };
        self.shared
            .start_or_replay_session(
                Arc::clone(&self.stdin),
                attachment,
                method,
                request,
                attempt,
                store,
            )
            .await;
    }

    pub(crate) async fn read_request(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let bytes = tokio::time::timeout(Duration::from_secs(10), self.output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes > 0, "native pipe ended before the original request");
        serde_json::from_str(&line).unwrap()
    }

    pub(crate) async fn deliver(&self, response: &serde_json::Value) {
        self.shared
            .deliver_line(&serde_json::to_vec(response).unwrap(), &self.stdin)
            .await;
    }

    pub(crate) async fn frames(&self) -> Vec<ControlBody> {
        self.shared
            .control
            .lock()
            .await
            .queue
            .iter()
            .map(|entry| serde_json::from_slice(&entry.wire[4..]).unwrap())
            .collect()
    }

    pub(crate) async fn wait_for_frame(
        &self,
        predicate: impl Fn(&ControlBody) -> bool,
    ) -> ControlBody {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let changed = self.shared.control_wake.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(frame) = self.frames().await.into_iter().find(&predicate) {
                    return frame;
                }
                changed.await;
            }
        })
        .await
        .expect("the original collector must publish its actual frame")
    }

    pub(crate) async fn original_descriptor(&mut self) -> ControlBody {
        assert!(self.observer.is_none());
        let shared = Arc::clone(&self.shared);
        let attachment = self.attachment.expect("original invocation was started");
        self.observer = Some(tokio::spawn(async move {
            assert!(shared.resume_session(attachment).await.is_some());
        }));
        self.wait_for_frame(|frame| matches!(frame, ControlBody::OriginalEstablishment { .. }))
            .await
    }

    pub(crate) async fn join_observer(&mut self) {
        if let Some(observer) = self.observer.take() {
            tokio::time::timeout(Duration::from_secs(10), observer)
                .await
                .unwrap()
                .unwrap();
        }
    }

    pub(super) async fn replay_same_request(&self) {
        let ControlBody::EstablishSession {
            method,
            request,
            attempt,
            store,
        } = self.establish_frame()
        else {
            unreachable!()
        };
        self.shared
            .start_or_replay_session(
                Arc::clone(&self.stdin),
                self.attachment.unwrap(),
                method,
                request,
                attempt,
                store,
            )
            .await;
    }

    pub(super) fn assert_no_more_requests(&mut self) {
        use futures_util::FutureExt as _;
        let mut line = String::new();
        match self.output.read_line(&mut line).now_or_never() {
            None => {}
            Some(Ok(0)) => assert!(self.child.0.try_wait().unwrap().is_some()),
            other => panic!("unexpected second native request: {other:?}, {line:?}"),
        }
    }

    pub(super) fn kill_native(&mut self) {
        let _ = self.child.0.kill();
        self.child.0.wait().unwrap();
    }
}

impl Drop for OwnedNativeFixture {
    fn drop(&mut self) {
        if let Some(observer) = self.observer.take() {
            observer.abort();
        }
        // Reap the exact original native child before restoring HOME/removing its profile.
        let _ = self.child.0.kill();
        let _ = self.child.0.wait();
        self.shared
            .observe_native_loss(NativeChannelLossCause::StdoutEof);
        let _ = worker_registry::retire_selected(&self.record);
    }
}

use super::{bootstrap, current_boot, ExecutionPlan, OwnedStop};
use crate::session::{builder::CreationIntent, LifecycleOperation, Storage};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// A complete inherited environment is captured here, not guessed from Command deltas.
pub struct OwnedCreateCommand {
    owner: Arc<OwnedStop>,
    program: OsString,
    args: Vec<OsString>,
    env: BTreeMap<OsString, OsString>,
    cwd: PathBuf,
    cwd_anchor: Option<OriginalDirAnchor>,
    env_anchors: BTreeMap<OsString, OriginalDirAnchor>,
    kind: CreateCommandKind,
}
struct OriginalDirAnchor {
    file: File,
    path: PathBuf,
    birth: crate::session::DirectoryIdentity,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CreateCommandKind {
    Ordinary,
    Undo,
}
#[derive(Default)]
struct AdmissionGate {
    withdrawing: bool,
}

impl OwnedCreateCommand {
    pub fn arg(&mut self, value: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(value.as_ref().to_owned());
        self
    }
    pub fn args<I, S>(&mut self, values: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(values.into_iter().map(|v| v.as_ref().to_owned()));
        self
    }
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env_anchors.remove(key.as_ref());
        self.env
            .insert(key.as_ref().to_owned(), value.as_ref().to_owned());
        self
    }
    pub fn envs<I, K, V>(&mut self, values: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, value) in values {
            self.env(key, value);
        }
        self
    }
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env_anchors.remove(key.as_ref());
        self.env.remove(key.as_ref());
        self
    }
    pub fn current_dir(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.cwd_anchor = None;
        self.cwd = path.as_ref().to_owned();
        self
    }
    pub(crate) fn anchored_env_path(
        &mut self,
        key: impl AsRef<OsStr>,
        directory: &crate::session::builder::AnchoredDir,
    ) -> Result<&mut Self> {
        let key = key.as_ref();
        anyhow::ensure!(
            [
                OsStr::new("GIT_DIR"),
                OsStr::new("GIT_COMMON_DIR"),
                OsStr::new("GIT_WORK_TREE")
            ]
            .contains(&key),
            "native directory transport is restricted to actual Git directory roles"
        );
        let anchor = self.original_anchor(directory)?;
        self.env.insert(key.to_owned(), OsString::new());
        self.env_anchors.insert(key.to_owned(), anchor);
        Ok(self)
    }
    pub(crate) fn anchored_current_dir(
        &mut self,
        directory: &crate::session::builder::AnchoredDir,
    ) -> Result<&mut Self> {
        let anchor = self.original_anchor(directory)?;
        self.cwd = anchor.path.clone();
        self.cwd_anchor = Some(anchor);
        Ok(self)
    }
    fn original_anchor(
        &self,
        directory: &crate::session::builder::AnchoredDir,
    ) -> Result<OriginalDirAnchor> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.owner, directory.native_owner()),
            "Git anchor changed its original Create owner/g"
        );
        let birth = directory.native_identity();
        let metadata = directory.native_file().metadata()?;
        anyhow::ensure!(
            birth.is_durable()
                && metadata.is_dir()
                && crate::session::DirectoryIdentity::from_metadata(&metadata) == birth,
            "Git anchor is not its actual original role PFD/birth"
        );
        Ok(OriginalDirAnchor {
            file: directory.native_file().try_clone()?,
            path: directory.native_path().to_owned(),
            birth,
        })
    }
    fn freeze(&self) -> Result<(FrozenInvocation, [File; 2], Vec<File>)> {
        anyhow::ensure!(
            self.env.keys().all(|key| !key.is_empty()
                && !key.as_bytes().contains(&b'=')
                && !key.as_bytes().contains(&0)),
            "owned native environment contains an invalid POSIX key"
        );
        let cwd = std::fs::canonicalize(&self.cwd).context("freezing native cwd")?;
        let candidate = if Path::new(&self.program).components().count() > 1
            || Path::new(&self.program).is_absolute()
        {
            let path = PathBuf::from(&self.program);
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        } else {
            let path = self
                .env
                .get(OsStr::new("PATH"))
                .context("owned command has no PATH")?;
            std::env::split_paths(path)
                .map(|dir| {
                    let dir = if dir.is_absolute() {
                        dir
                    } else {
                        cwd.join(dir)
                    };
                    dir.join(&self.program)
                })
                .find(|path| {
                    std::fs::metadata(path)
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                })
                .context("owned native program is not executable in frozen PATH")?
        };
        let program = std::fs::canonicalize(candidate)?;
        let executable = File::open(&program)?;
        let directory = if let Some(anchor) = &self.cwd_anchor {
            anyhow::ensure!(
                crate::session::DirectoryIdentity::from_metadata(&std::fs::metadata(&cwd)?)
                    == anchor.birth,
                "original anchored cwd pathname was replaced before native admission"
            );
            anchor.file.try_clone()?
        } else {
            File::open(&cwd)?
        };
        let mut bindings = Vec::with_capacity(self.env_anchors.len());
        let mut anchors = Vec::with_capacity(self.env_anchors.len());
        for (index, (key, anchor)) in self.env_anchors.iter().enumerate() {
            anyhow::ensure!(
                crate::session::DirectoryIdentity::from_metadata(&anchor.file.metadata()?)
                    == anchor.birth
                    && crate::session::DirectoryIdentity::from_metadata(&std::fs::metadata(
                        &anchor.path
                    )?) == anchor.birth,
                "original Git directory path/birth changed before native admission"
            );
            let target_fd = 64 + index as u32;
            let (path, path_only) =
                crate::process::owned_create_anchor_path(&anchor.file, target_fd)?;
            bindings.push(FrozenDirBinding {
                key: key.as_bytes().to_vec(),
                original_path: anchor.path.as_os_str().as_bytes().to_vec(),
                target_path: path.as_os_str().as_bytes().to_vec(),
                target_fd,
                birth: anchor.birth,
                path_only,
            });
            anchors.push(anchor.file.try_clone()?);
        }
        let program_identity =
            crate::session::DirectoryIdentity::from_metadata(&executable.metadata()?);
        let cwd_identity = crate::session::DirectoryIdentity::from_metadata(&directory.metadata()?);
        Ok((
            FrozenInvocation {
                program: program.as_os_str().as_bytes().to_vec(),
                argv0: self.program.as_bytes().to_vec(),
                args: self.args.iter().map(|v| v.as_bytes().to_vec()).collect(),
                env: self
                    .env
                    .iter()
                    .map(|(key, value)| {
                        let bytes = bindings
                            .iter()
                            .find(|b| b.key == key.as_bytes())
                            .map(|b| b.target_path.clone())
                            .unwrap_or_else(|| value.as_bytes().to_vec());
                        (key.as_bytes().to_vec(), bytes)
                    })
                    .collect(),
                cwd: cwd.as_os_str().as_bytes().to_vec(),
                program_identity,
                cwd_identity,
                bindings,
            },
            [executable, directory],
            anchors,
        ))
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FrozenDirBinding {
    key: Vec<u8>,
    original_path: Vec<u8>,
    target_path: Vec<u8>,
    target_fd: u32,
    birth: crate::session::DirectoryIdentity,
    path_only: bool,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FrozenInvocation {
    program: Vec<u8>,
    argv0: Vec<u8>,
    args: Vec<Vec<u8>>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    cwd: Vec<u8>,
    program_identity: crate::session::DirectoryIdentity,
    cwd_identity: crate::session::DirectoryIdentity,
    bindings: Vec<FrozenDirBinding>,
}
impl FrozenInvocation {
    fn exec(&self, executable: File, directory: File) -> Result<()> {
        let program = Path::new(OsStr::from_bytes(&self.program));
        let cwd = Path::new(OsStr::from_bytes(&self.cwd));
        anyhow::ensure!(
            crate::session::DirectoryIdentity::from_metadata(&std::fs::metadata(program)?)
                == self.program_identity,
            "frozen executable changed before exec"
        );
        anyhow::ensure!(
            crate::session::DirectoryIdentity::from_metadata(&std::fs::metadata(cwd)?)
                == self.cwd_identity,
            "frozen cwd changed before exec"
        );
        anyhow::ensure!(
            crate::session::DirectoryIdentity::from_metadata(&executable.metadata()?)
                == self.program_identity
                && crate::session::DirectoryIdentity::from_metadata(&directory.metadata()?)
                    == self.cwd_identity,
            "target execution replaced its actual frozen program/cwd FDs"
        );
        let argv: Vec<CString> = std::iter::once(self.argv0.as_slice())
            .chain(self.args.iter().map(Vec::as_slice))
            .map(CString::new)
            .collect::<std::result::Result<_, _>>()?;
        let env: Vec<CString> = self
            .env
            .iter()
            .map(|(key, value)| {
                let mut entry = Vec::with_capacity(key.len() + value.len() + 1);
                entry.extend_from_slice(key);
                entry.push(b'=');
                entry.extend_from_slice(value);
                CString::new(entry)
            })
            .collect::<std::result::Result<_, _>>()?;
        crate::process::exec_owned_create(
            &CString::new(self.program.as_slice())?,
            &executable,
            &directory,
            &argv,
            &env,
        )
    }
}

/// Complete invocation secrets stay only in the original live custody and private FD channel.
#[derive(Clone, Serialize, Deserialize)]
struct NativeSpec {
    invocation: FrozenInvocation,
    goal: ExecutionPlan,
    goal_before_start_env: Vec<(String, String)>,
    seal: [u8; 32],
    durable_goal: [u8; 32],
    kind: CreateCommandKind,
}
impl NativeSpec {
    fn commitment(&self) -> Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        struct HashWriter(Sha256);
        impl Write for HashWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = HashWriter(Sha256::new());
        writer.0.update(b"aoe.original-create.native-spec.v1\0");
        // Fixed struct order, byte-valued argv/path/env, sorted explicit environment,
        // and a private random seal make this canonical and resistant to secret guessing.
        serde_json::to_writer(&mut writer, self)?;
        Ok(writer.0.finalize().into())
    }
}

/// Only commitments and genuine native birth/retirement evidence are persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateExecution {
    format: u8,
    nonce: uuid::Uuid,
    session_id: String,
    created_at: chrono::DateTime<chrono::Utc>,
    generation: u64,
    boot: super::BootToken,
    commitment: [u8; 32],
    profile_identity: crate::session::DirectoryIdentity,
    births: Vec<crate::process::ProcessIncarnation>,
    root_status: Option<i32>,
    descendants_retired: bool,
    group_retired: bool,
    external_domain: bool,
    scope_unproven: bool,
    effect_acknowledged: bool,
    #[serde(default)]
    no_target_approved: bool,
}
impl CreateExecution {
    pub(super) fn same_record(&self, other: &Self) -> bool {
        self == other
    }
    pub(super) fn proves_retired(&self) -> bool {
        self.format == 1
            && self.descendants_retired
            && self.group_retired
            && self.root_status.is_some()
            && self.effect_acknowledged
            && !self.external_domain
            && !self.scope_unproven
            && !self.births.is_empty()
    }
}
#[derive(Default)]
pub(super) struct CreateNativeCustody {
    receipts: Mutex<Vec<Arc<CreateReceipt>>>,
    admission: Mutex<AdmissionGate>,
}
impl std::fmt::Debug for CreateNativeCustody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalCreateNativeCustody")
            .finish_non_exhaustive()
    }
}
struct CreateReceipt {
    state: Mutex<CreateExecution>,
    cancel: CancellationToken,
    pins: [File; 2],
    spec: NativeSpec,
    root: Mutex<Option<crate::process::OwnedCreateRoot>>,
    anchors: Vec<File>,
    #[cfg(test)]
    cancel_at_admission: bool,
}
impl std::fmt::Debug for CreateReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalCreateReceipt")
            .finish_non_exhaustive()
    }
}
impl CreateNativeCustody {
    fn retain(&self, receipt: Arc<CreateReceipt>) {
        self.receipts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(receipt);
    }
    fn observed(&self, owner: &OwnedStop) -> Result<()> {
        let receipts = self.receipts.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(receipts.iter().all(|r| {
            let state = r.state.lock().unwrap_or_else(|e| e.into_inner());
            state.root_status.is_some() && state.effect_acknowledged && state.group_retired
                && !state.births.is_empty() && !state.no_target_approved && !r.cancel.is_cancelled()
        }), "native producer has not acknowledged its actual root output and original group retirement");
        owner.with_scope(|row| {
            anyhow::ensure!(
                row.runner_journal.creations.len() == receipts.len()
                    && row
                        .runner_journal
                        .creations
                        .iter()
                        .all(|record| receipts.iter().any(|r| r
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .same_record(record))),
                "observed command completion is not the original producer receipt set"
            );
            Ok(())
        })
    }
    fn warnings(&self) -> Vec<String> {
        let receipts = self.receipts.lock().unwrap_or_else(|e| e.into_inner());
        if receipts.iter().any(|r| {
            !r.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .proves_retired()
        }) {
            vec!["Original native command output is retained, but descendant/container coverage remains uncertain. Publication and an independently authorized runner launch do not authorize destructive withdrawal.".into()]
        } else {
            Vec::new()
        }
    }
    fn retire(&self, owner: &OwnedStop) -> Result<()> {
        let receipts = self.receipts.lock().unwrap_or_else(|e| e.into_inner());
        for receipt in receipts.iter() {
            receipt.cancel.cancel();
        }
        anyhow::ensure!(
            receipts.iter().all(|receipt| receipt
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .proves_retired()),
            "original native births, descendants or external/container domain remain protected"
        );
        let original = owner.current_projection();
        owner.storage().verify_profile_identity()?;
        let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
        let _identity = crate::session::acquire_session_identity_lock()?;
        let _lifecycle = owner
            .storage()
            .acquire_instance_lifecycle_lock(owner.session_id())?;
        let rows = owner
            .storage()
            .load_strict_for_worktree_ownership_locked()?;
        let row = rows
            .iter()
            .find(|row| row.id == owner.session_id())
            .context("original Create disappeared")?;
        original.validate_baseline_at(row, owner.generation())?;
        anyhow::ensure!(
            row.runner_journal.create_coverage == super::CreationCoverage::Owned
                && row.runner_journal.creations.len() == receipts.len()
                && row
                    .runner_journal
                    .creations
                    .iter()
                    .all(|record| record.proves_retired()
                        && receipts.iter().any(|receipt| receipt
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .same_record(record))),
            "canonical native history is not the original retained receipt set"
        );
        Ok(())
    }
}

impl CreationIntent {
    pub fn owned_command(&self, program: impl AsRef<OsStr>) -> Result<OwnedCreateCommand> {
        Ok(OwnedCreateCommand {
            owner: self.borrow_owned_create()?,
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            env: std::env::vars_os().collect(),
            cwd: std::env::current_dir()?,
            cwd_anchor: None,
            env_anchors: BTreeMap::new(),
            kind: CreateCommandKind::Ordinary,
        })
    }
    pub(crate) fn begin_owned_withdrawal(&self) -> Result<()> {
        let owner = self.borrow_owned_create()?;
        let mut gate = owner
            .native_create
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        gate.withdrawing = true;
        // The seal precedes cancellation and remains installed on uncertainty/error.
        // No new ordinary pending CAS can race the filesystem withdrawal.
        owner.native_create.retire(&owner)
    }
    pub(crate) fn owned_undo_command(
        &self,
        program: impl AsRef<OsStr>,
    ) -> Result<OwnedCreateCommand> {
        let owner = self.borrow_owned_create()?;
        let gate = owner
            .native_create
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            gate.withdrawing,
            "original Creating withdrawal was not sealed"
        );
        owner.native_create.retire(&owner)?;
        let mut command = self.owned_command(program)?;
        command.kind = CreateCommandKind::Undo;
        Ok(command)
    }
    pub fn run_owned_output(
        &self,
        command: &mut OwnedCreateCommand,
        cancel: Option<&CancellationToken>,
    ) -> Result<Output> {
        self.run_owned_command_inner(command, cancel, false, |_, _| {})
    }
    pub fn run_owned_command(
        &self,
        command: &mut OwnedCreateCommand,
        cancel: Option<&CancellationToken>,
        progress: impl FnMut(&[u8], &[u8]),
    ) -> Result<Output> {
        self.run_owned_command_inner(command, cancel, true, progress)
    }
    fn run_owned_command_inner(
        &self,
        command: &mut OwnedCreateCommand,
        cancel: Option<&CancellationToken>,
        emit_progress: bool,
        mut progress: impl FnMut(&[u8], &[u8]),
    ) -> Result<Output> {
        let owner = self.borrow_owned_create()?;
        anyhow::ensure!(
            Arc::ptr_eq(&owner, &command.owner),
            "command changed its original Create"
        );
        let (invocation, pins, anchors) = command.freeze()?;
        let gate = owner
            .native_create
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(gate.withdrawing == (command.kind == CreateCommandKind::Undo),
            "ordinary Creating spec was sealed before its actual pending CAS, or Undo was not admitted");
        if command.kind == CreateCommandKind::Undo {
            owner.native_create.retire(&owner)?;
        }
        let original = owner.current_projection();
        anyhow::ensure!(
            original.create_coverage == super::CreationCoverage::Owned,
            "original Create history has no pre-effect native custody"
        );
        let mut seal = [0; 32];
        seal[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        seal[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let spec = NativeSpec {
            invocation,
            durable_goal: canonical_goal(&original.plan.execution)?,
            goal: original.plan.execution.clone(),
            goal_before_start_env: original
                .plan
                .sandbox
                .as_ref()
                .map(|s| s.before_start_env.clone())
                .unwrap_or_default(),
            seal,
            kind: command.kind,
        };
        let record = CreateExecution {
            format: 1,
            nonce: uuid::Uuid::new_v4(),
            session_id: owner.session_id().to_owned(),
            created_at: original.plan.created_at,
            generation: owner.generation(),
            boot: current_boot().context("native boot identity unavailable")?,
            commitment: spec.commitment()?,
            profile_identity: owner.storage().original_profile_identity()?,
            births: Vec::new(),
            root_status: None,
            descendants_retired: false,
            group_retired: false,
            external_domain: false,
            scope_unproven: !crate::process::owned_create_descendant_traceable()
                || spec
                    .invocation
                    .bindings
                    .iter()
                    .any(|binding| binding.path_only),
            effect_acknowledged: false,
            no_target_approved: false,
        };
        let receipt = Arc::new(CreateReceipt {
            state: Mutex::new(record.clone()),
            cancel: cancel
                .map(CancellationToken::child_token)
                .unwrap_or_default(),
            pins,
            spec,
            root: Mutex::new(None),
            anchors,
            #[cfg(test)]
            cancel_at_admission: tests::CANCEL_AT_ADMISSION.with(|flag| flag.replace(false)),
        });
        // Custody is retained before pending CAS, spawn, or any target effect.
        owner.native_create.retain(receipt.clone());
        owner.update_projection(
            |row| {
                row.runner_journal.creations.push(record.clone());
                Ok(())
            },
            |_| Ok(()),
        )?;
        drop(gate);
        let (events, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("owned-create-producer".into())
            .spawn(move || {
                let result = drive(owner, receipt, record, &events, emit_progress);
                let _ = events.send(Event::Finished(result));
            })
            .context("starting original native producer")?;
        while let Ok(event) = receiver.recv() {
            match event {
                Event::Stdout(bytes) => progress(&bytes, &[]),
                Event::Stderr(bytes) => progress(&[], &bytes),
                Event::Finished(result) => return result,
            }
        }
        anyhow::bail!("original native producer ended without its real effect ACK")
    }
    pub(crate) fn ensure_native_commands_observed(&self) -> Result<()> {
        let owner = self.borrow_owned_create()?;
        let gate = owner
            .native_create
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(!gate.withdrawing, "original Creating withdrawal was sealed; a prepared result cannot republish after possible Undo effects");
        owner.native_create.observed(&owner)
    }
    pub(crate) fn native_warnings(&self) -> Result<Vec<String>> {
        let owner = self.borrow_owned_create()?;
        Ok(owner.native_create.warnings())
    }
    pub fn retire_owned_commands(&self) -> Result<()> {
        let owner = self.borrow_owned_create()?;
        owner.native_create.retire(&owner)
    }
}

enum Event {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Finished(Result<Output>),
}
#[derive(Serialize)]
struct OriginalCreateBootstrap<'a> {
    profile: &'a str,
    record: &'a CreateExecution,
    spec: &'a NativeSpec,
    fences: [crate::session::DirectoryIdentity; 3],
}
#[derive(Serialize, Deserialize)]
struct CreateBootstrap {
    profile: String,
    record: CreateExecution,
    spec: NativeSpec,
    fences: [crate::session::DirectoryIdentity; 3],
}

/// Only the hidden native child uses this. It adopts the true profile role FD,
/// publishes its own birth under the original transferred physical fences, and
/// cannot execute the target until its producer has checked the canonical ACK.
pub(crate) fn bootstrap_child() -> Result<()> {
    let input = unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) }.try_clone_to_owned()?;
    let mut channel = UnixStream::from(input);
    let [profile, workspace, identity, lifecycle] =
        crate::process::receive_bootstrap_descriptors(&channel)?;
    let received: CreateBootstrap = bootstrap::read_frame(&mut channel)?;
    let [executable, directory] = crate::process::receive_bootstrap_descriptors::<2>(&channel)?;
    let fences = [
        File::from(workspace),
        File::from(identity),
        File::from(lifecycle),
    ];
    for (fd, expected) in fences.iter().zip(received.fences) {
        anyhow::ensure!(
            crate::session::DirectoryIdentity::from_metadata(&fd.metadata()?) == expected,
            "native Create fence changed its actual original role FD"
        );
    }
    let storage = Arc::new(Storage::adopt_original_profile(
        received.profile,
        File::from(profile),
        received.record.profile_identity,
    )?);
    let incarnation = crate::process::process_incarnation(std::process::id())?
        .context("native Create birth unavailable")?;
    anyhow::ensure!(
        incarnation.group == incarnation.pid
            && received.record.boot == current_boot().context("boot unavailable")?,
        "native Create is not its original new group/root"
    );
    let spec = received.spec;
    let anchors = receive_anchors(&channel, spec.invocation.bindings.len())?;
    for (file, binding) in anchors.iter().zip(&spec.invocation.bindings) {
        anyhow::ensure!(
            file.metadata()?.is_dir()
                && crate::session::DirectoryIdentity::from_metadata(&file.metadata()?)
                    == binding.birth
                && crate::session::DirectoryIdentity::from_metadata(&std::fs::metadata(
                    Path::new(OsStr::from_bytes(&binding.original_path))
                )?) == binding.birth,
            "received native Git role PFD/path is not its original physical birth"
        );
    }
    crate::process::install_owned_create_anchors(
        &anchors,
        &spec
            .invocation
            .bindings
            .iter()
            .map(|b| b.target_fd)
            .collect::<Vec<_>>(),
    )?;
    let mut record = received.record;
    anyhow::ensure!(
        spec.commitment()? == record.commitment,
        "private native spec commitment changed"
    );
    storage.verify_profile_identity()?;
    storage.update_under_workspace_claim_lock(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == record.session_id)
            .context("native Create row disappeared")?;
        validate_goal(&storage, row, &record, &spec)?;
        let pending = row
            .runner_journal
            .creations
            .iter_mut()
            .find(|p| p.nonce == record.nonce)
            .context("native pending producer CAS absent")?;
        anyhow::ensure!(
            pending.births.is_empty() && pending.same_record(&record),
            "native producer CAS was superseded"
        );
        record.births.push(incarnation);
        *pending = record.clone();
        Ok(())
    })?;
    super::sync_parent_directory(storage.sessions_path())?;
    bootstrap::write_frame(&mut channel, &record)?;
    crate::process::hold_owned_create_bootstrap()?;
    let mut approval = [0];
    channel.read_exact(&mut approval)?;
    anyhow::ensure!(approval == [1], "original Create target execution refused");
    storage.verify_profile_identity()?;
    drop(fences);
    drop(channel);
    drop(storage);
    // This preserves the actual admitted root PID/start/group, unlike spawning another CLI.
    spec.invocation
        .exec(File::from(executable), File::from(directory))
}

fn send_anchors(channel: &UnixStream, files: &[File]) -> Result<()> {
    match files {
        [] => Ok(()),
        [a] => bootstrap::send_descriptors(channel, [a.as_fd()]),
        [a, b] => bootstrap::send_descriptors(channel, [a.as_fd(), b.as_fd()]),
        [a, b, c] => bootstrap::send_descriptors(channel, [a.as_fd(), b.as_fd(), c.as_fd()]),
        _ => anyhow::bail!("native Git role packet exceeds its exact admitted scope"),
    }
}
fn receive_anchors(channel: &UnixStream, count: usize) -> Result<Vec<File>> {
    match count {
        0 => Ok(Vec::new()),
        1 => Ok(crate::process::receive_bootstrap_descriptors::<1>(channel)?
            .into_iter()
            .map(File::from)
            .collect()),
        2 => Ok(crate::process::receive_bootstrap_descriptors::<2>(channel)?
            .into_iter()
            .map(File::from)
            .collect()),
        3 => Ok(crate::process::receive_bootstrap_descriptors::<3>(channel)?
            .into_iter()
            .map(File::from)
            .collect()),
        _ => anyhow::bail!("native Git role packet exceeds its exact admitted scope"),
    }
}
fn validate_goal(
    storage: &Storage,
    row: &crate::session::Instance,
    record: &CreateExecution,
    spec: &NativeSpec,
) -> Result<()> {
    anyhow::ensure!(
        row.id == record.session_id
            && row.created_at == record.created_at
            && row.lifecycle_generation == record.generation
            && row.lifecycle_reservation_is_owned(LifecycleOperation::Create, record.generation)
            && record.format == 1,
        "native Create original ID/DOB, private full commitment, or same-g claim changed"
    );
    let current = super::LaunchOrigin::capture_baseline_at(row, Arc::new(storage.clone()))?;
    // Compare the complete canonical durable goal, not a hand-maintained field subset.
    // Live before_start env is already sealed separately with the full effective env.
    anyhow::ensure!(
        canonical_goal(&current.plan.execution)? == spec.durable_goal,
        "original complete prepared goal was superseded before native effect"
    );
    Ok(())
}
fn canonical_goal(goal: &impl Serialize) -> Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    struct GoalHash(Sha256);
    impl Write for GoalHash {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = GoalHash(Sha256::new());
    serde_json::to_writer(&mut writer, goal)?;
    Ok(writer.0.finalize().into())
}

fn publish(owner: &OwnedStop, receipt: &CreateReceipt, record: &CreateExecution) -> Result<()> {
    owner.update_projection(
        |row| {
            validate_goal(owner.storage(), row, record, &receipt.spec)?;
            let current = row
                .runner_journal
                .creations
                .iter_mut()
                .find(|p| p.nonce == record.nonce)
                .context("original native receipt disappeared")?;
            anyhow::ensure!(
                current.same_record(&receipt.state.lock().unwrap_or_else(|e| e.into_inner()))
                    && current.commitment == record.commitment
                    && current
                        .births
                        .iter()
                        .all(|birth| record.births.contains(birth)),
                "original native invocation/birth scope was replaced"
            );
            *current = record.clone();
            Ok(())
        },
        |_| Ok(()),
    )?;
    *receipt.state.lock().unwrap_or_else(|e| e.into_inner()) = record.clone();
    Ok(())
}

fn drive(
    owner: Arc<OwnedStop>,
    receipt: Arc<CreateReceipt>,
    mut record: CreateExecution,
    events: &std::sync::mpsc::Sender<Event>,
    emit_progress: bool,
) -> Result<Output> {
    let workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let identity = crate::session::acquire_session_identity_lock()?;
    owner.storage().verify_profile_identity()?;
    let lifecycle = owner
        .storage()
        .acquire_instance_lifecycle_lock(owner.session_id())?;
    super::ensure_unique_owner(owner.storage(), owner.session_id())?;
    let original = owner.current_projection();
    let row = owner
        .storage()
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|r| r.id == owner.session_id())
        .context("original native Create disappeared")?;
    original.validate_baseline_at(&row, owner.generation())?;
    validate_goal(owner.storage(), &row, &record, &receipt.spec)?;
    let (mut channel, input) = UnixStream::pair()?;
    let input: std::os::fd::OwnedFd = input.into();
    #[cfg(not(test))]
    let executable = std::env::current_exe()?;
    #[cfg(test)]
    let executable = match std::env::var_os("AOE_NATIVE_CREATE_EXECUTABLE") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .and_then(Path::parent)
            .context("unit test executable has no Cargo binary directory")?
            .join("aoe"),
    }
    .canonicalize()
    .context("build the actual aoe binary before native unit tests")?;
    let mut command = Command::new(executable);
    command.env_clear();
    for key in ["HOME", "XDG_CONFIG_HOME"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .arg("__owned-create-native")
        .stdin(Stdio::from(input))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::configure_process_group(&mut command);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("native stdout unavailable")?;
    let stderr = child.stderr.take().context("native stderr unavailable")?;
    let out_events = emit_progress.then(|| events.clone());
    let out = std::thread::spawn(move || collect(stdout, out_events, true));
    let err_events = emit_progress.then(|| events.clone());
    let err = std::thread::spawn(move || collect(stderr, err_events, false));
    let mut native_root = receipt.root.lock().unwrap_or_else(|e| e.into_inner());
    let mut canonical_natal_ack = false;
    let mut approval_attempted = false;
    let mut original_birth = None;
    let issued = (|| -> Result<()> {
        // Capture the actual unreaped Child before transferring any target authority.
        original_birth = Some(
            crate::process::process_incarnation(child.id())?
                .context("original bootstrap kernel birth unavailable")?,
        );
        bootstrap::send_descriptors(
            &channel,
            [
                owner.storage().original_profile_fd()?,
                workspace.as_fd(),
                identity.as_fd(),
                lifecycle.as_fd(),
            ],
        )?;
        bootstrap::write_frame(
            &mut channel,
            &OriginalCreateBootstrap {
                profile: owner.storage().profile(),
                record: &record,
                spec: &receipt.spec,
                fences: [
                    workspace.file_identity()?,
                    identity.file_identity()?,
                    lifecycle.file_identity()?,
                ],
            },
        )?;
        bootstrap::send_descriptors(&channel, [receipt.pins[0].as_fd(), receipt.pins[1].as_fd()])?;
        send_anchors(&channel, &receipt.anchors)?;
        let published: CreateExecution = bootstrap::read_frame(&mut channel)?;
        let birth = original_birth.context("original bootstrap birth not retained")?;
        anyhow::ensure!(
            crate::process::process_incarnation(child.id())? == Some(birth),
            "original bootstrap changed before its natal ACK"
        );
        let mut expected_ack = record.clone();
        expected_ack.births.push(birth);
        anyhow::ensure!(
            published.same_record(&expected_ack) && birth.group == birth.pid,
            "producer natal ACK changed its actual original root"
        );
        let row = owner
            .storage()
            .load_strict_for_worktree_ownership_locked()?
            .into_iter()
            .find(|r| r.id == owner.session_id())
            .context("producer ACK row disappeared")?;
        original.validate_plan_at(&row, owner.generation(), original.plan.trashed)?;
        anyhow::ensure!(
            row.runner_journal
                .launches()
                .iter()
                .all(|launch| original.births.contains(&launch.birth_key()))
                && row.runner_journal.create_coverage == original.create_coverage
                && row.runner_journal.creations.len() == original.creations.len()
                && row.runner_journal.creations.iter().all(|p| {
                    if p.nonce == published.nonce {
                        p.same_record(&published)
                    } else {
                        original
                            .creations
                            .iter()
                            .any(|before| before.same_record(p))
                    }
                }),
            "producer natal ACK changed another original native scope"
        );
        validate_goal(owner.storage(), &row, &published, &receipt.spec)?;
        anyhow::ensure!(
            row.runner_journal
                .creations
                .iter()
                .any(|p| p.same_record(&published)),
            "producer ACK is not its actual durable canonical CAS"
        );
        record = published;
        *owner.acknowledged.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(
            super::LaunchOrigin::capture_baseline_at(&row, original.plan.storage.clone())?,
        ));
        *receipt.state.lock().unwrap_or_else(|e| e.into_inner()) = record.clone();
        canonical_natal_ack = true;
        *native_root = Some(crate::process::OwnedCreateRoot::prepare(birth)?);
        #[cfg(test)]
        if receipt.cancel_at_admission {
            receipt.cancel.cancel();
        }
        if receipt.cancel.is_cancelled() {
            anyhow::bail!("original Create cancelled before target exec");
        }
        native_root
            .as_mut()
            .context("original held root absent")?
            .release()?;
        // A failed write is not evidence that the receiver failed to obtain approval.
        approval_attempted = true;
        channel.write_all(&[1])?;
        Ok(())
    })();
    drop(channel);
    drop(lifecycle);
    drop(identity);
    drop(workspace);
    if let Err(error) = issued {
        let no_target_approved = canonical_natal_ack && !approval_attempted;
        let settled = (|| -> Result<()> {
            if let Some(root) = native_root.as_mut() {
                let stop = CancellationToken::new();
                stop.cancel();
                #[cfg(target_os = "linux")]
                child.kill().context("stopping original traced bootstrap")?;
                let retirement = root.retire(&mut child, &stop, |observation| {
                    match observation {
                        crate::process::CreateObservation::Birth(birth) => {
                            record.births.push(birth)
                        }
                        crate::process::CreateObservation::ExternalDomain => {
                            record.external_domain = true
                        }
                        crate::process::CreateObservation::ScopeUnproven => {
                            record.scope_unproven = true
                        }
                    }
                    publish(&owner, &receipt, &record)
                })?;
                record.root_status = Some(retirement.raw_status);
                record.group_retired = retirement.group_retired;
                record.descendants_retired = retirement.descendants_retired;
                record.external_domain |= retirement.external_domain;
                // Only this original private-channel bootstrap, with the canonical natal
                // ACK and no approval write, proves no target/escaped descendant effect.
                // This is NOT Darwin PathOnly target containment.
                if no_target_approved
                    && original_birth.is_some_and(|birth| record.births.as_slice() == [birth])
                    && record.group_retired
                    && !record.external_domain
                {
                    record.no_target_approved = true;
                    record.descendants_retired = true;
                    record.scope_unproven = false;
                } else {
                    record.scope_unproven |= !retirement.descendants_retired;
                }
                publish(&owner, &receipt, &record)?;
                out.join()
                    .map_err(|_| anyhow::anyhow!("original stdout producer panicked"))??;
                err.join()
                    .map_err(|_| anyhow::anyhow!("original stderr producer panicked"))??;
                record.effect_acknowledged = true;
                publish(&owner, &receipt, &record)?;
            } else {
                // No original held-root capability/canonical ACK: reap only the actual
                // Child; never turn that status into safe native withdrawal evidence.
                child
                    .kill()
                    .context("stopping unacknowledged original bootstrap")?;
                child
                    .wait()
                    .context("reaping unacknowledged original bootstrap")?;
                out.join()
                    .map_err(|_| anyhow::anyhow!("original stdout producer panicked"))??;
                err.join()
                    .map_err(|_| anyhow::anyhow!("original stderr producer panicked"))??;
            }
            Ok(())
        })();
        return match settled {
            Ok(()) => Err(error),
            Err(retirement) => Err(error.context(format!(
                "original native issuance retirement remains unproven: {retirement:#}"
            ))),
        };
    }
    let retirement = native_root
        .as_mut()
        .context("original native root absent")?
        .retire(&mut child, &receipt.cancel, |observation| {
            match observation {
                crate::process::CreateObservation::Birth(birth) => record.births.push(birth),
                crate::process::CreateObservation::ExternalDomain => record.external_domain = true,
                crate::process::CreateObservation::ScopeUnproven => record.scope_unproven = true,
            }
            // Every actual descendant and uncertainty transition is acknowledged before continuation.
            publish(&owner, &receipt, &record)
        })?;
    record.root_status = Some(retirement.raw_status);
    record.descendants_retired = retirement.descendants_retired;
    record.group_retired = retirement.group_retired;
    record.external_domain |= retirement.external_domain;
    record.scope_unproven |= !retirement.descendants_retired;
    publish(&owner, &receipt, &record)?;
    let status = retirement.status;
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("original stdout producer panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("original stderr producer panicked"))??;
    record.effect_acknowledged = true;
    publish(&owner, &receipt, &record)?;
    anyhow::ensure!(
        !receipt.cancel.is_cancelled(),
        "original Create cancelled during target execution"
    );
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
fn collect(
    mut input: impl Read,
    events: Option<std::sync::mpsc::Sender<Event>>,
    stdout: bool,
) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let n = match input.read(&mut buffer) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if n == 0 {
            return Ok(result);
        }
        result.extend_from_slice(&buffer[..n]);
        if let Some(events) = &events {
            let bytes = buffer[..n].to_vec();
            let _ = events.send(if stdout {
                Event::Stdout(bytes)
            } else {
                Event::Stderr(bytes)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{builder::CreationCustody, Instance};
    thread_local! {
        pub(super) static CANCEL_AT_ADMISSION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[test]
    #[ignore = "hosted native only; requires actual aoe and original bootstrap custody"]
    #[serial_test::serial]
    fn hosted_create_pre_target_cancel_original_retirement_and_same_g_undo() {
        let _executable = crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let storage = Arc::new(Storage::new_unwatched("default").unwrap());
        for cancel_at_admission in [false, true] {
            // The first original Create remains retained after its same-g Undo.
            // The second cancellation phase must admit a distinct logical session.
            let title = format!("pre-target cancellation at admission={cancel_at_admission}");
            let mut prepared = Instance::new(&title, home.path().to_str().unwrap());
            let _custody = CreationCustody::register(storage.clone(), &prepared).unwrap();
            let intent = CreationIntent::reserve_metadata(&storage, &mut prepared).unwrap();
            let owner = intent.borrow_owned_create().unwrap();
            let generation = owner.generation();
            let resource = home.path().join(format!("resource-{}", prepared.id));
            std::fs::create_dir(&resource).unwrap();
            let marker = resource.join("untouched-effect");
            std::fs::write(&marker, b"original untouched marker").unwrap();
            let mut command = intent.owned_command("/bin/sh").unwrap();
            command
                .args(["-c", "printf forbidden > untouched-effect"])
                .current_dir(&resource);
            let cancel = CancellationToken::new();
            if cancel_at_admission {
                CANCEL_AT_ADMISSION.with(|flag| flag.set(true));
            } else {
                cancel.cancel();
            }
            let failure = intent
                .run_owned_output(&mut command, Some(&cancel))
                .unwrap_err();
            assert!(format!("{failure:#}").contains("cancelled before target exec"));
            assert_eq!(
                std::fs::read(&marker).unwrap(),
                b"original untouched marker"
            );
            assert!(
                intent.ensure_native_commands_observed().is_err(),
                "no-effect retirement is not permission to publish success"
            );
            let receipts = owner.native_create.receipts.lock().unwrap();
            let receipt = receipts.last().unwrap();
            let record = receipt.state.lock().unwrap().clone();
            assert!(receipt.root.lock().unwrap().is_some());
            assert_eq!(record.births.len(), 1);
            assert_eq!(record.generation, generation);
            assert!(record.no_target_approved && record.proves_retired());
            let canonical = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == prepared.id)
                .unwrap();
            assert_eq!(canonical.lifecycle_generation, generation);
            assert!(canonical
                .runner_journal
                .creations
                .iter()
                .any(|p| p.same_record(&record)));
            drop(receipts);
            intent.begin_owned_withdrawal().unwrap();
            let mut undo = intent.owned_undo_command("/bin/rm").unwrap();
            undo.args([OsStr::new("-rf"), resource.as_os_str()]);
            assert!(intent
                .run_owned_output(&mut undo, None)
                .unwrap()
                .status
                .success());
            assert!(!resource.exists());
            assert_eq!(owner.generation(), generation);
            let canonical = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == prepared.id)
                .unwrap();
            assert_eq!(canonical.lifecycle_generation, generation);
        }
    }

    #[test]
    #[ignore = "hosted native only; requires actual built aoe and original producer custody"]
    #[serial_test::serial]
    fn hosted_create_complete_commitment_and_private_secrets() {
        let _executable = crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let storage = Storage::new_unwatched("default").unwrap();
        let mut prepared =
            Instance::new("private native commitment", home.path().to_str().unwrap());
        let intent = CreationIntent::reserve_metadata(&storage, &mut prepared).unwrap();
        let owner = intent.borrow_owned_create().unwrap();
        let env_secret = format!("private-env-{}", uuid::Uuid::new_v4());
        let arg_secret = format!("private-argv-{}", uuid::Uuid::new_v4());
        let mut command = intent.owned_command("/bin/sh").unwrap();
        command
            .args(["-c", "printf observed", &arg_secret])
            .env("NATIVE_PRIVATE_SECRET", &env_secret)
            .current_dir(home.path());
        let (invocation, _pins, _anchors) = command.freeze().unwrap();
        let goal = owner.current_projection().plan.execution.clone();
        let spec = NativeSpec {
            invocation,
            goal_before_start_env: Vec::new(),
            durable_goal: canonical_goal(&goal).unwrap(),
            goal,
            seal: [17; 32],
            kind: CreateCommandKind::Ordinary,
        };
        let baseline = spec.commitment().unwrap();
        let mut changed = spec.clone();
        changed.invocation.program.push(b'x');
        assert_ne!(baseline, changed.commitment().unwrap());
        changed = spec.clone();
        changed.invocation.args.push(b"different argument".to_vec());
        assert_ne!(baseline, changed.commitment().unwrap());
        changed = spec.clone();
        changed.invocation.env[0].1.push(b'x');
        assert_ne!(baseline, changed.commitment().unwrap());
        changed = spec.clone();
        changed.invocation.cwd.push(b'x');
        assert_ne!(baseline, changed.commitment().unwrap());
        changed = spec.clone();
        changed.goal.command.push('x');
        assert_ne!(baseline, changed.commitment().unwrap());
        changed = spec.clone();
        changed
            .goal_before_start_env
            .push(("private-live-goal".into(), "changed".into()));
        assert_ne!(baseline, changed.commitment().unwrap());
        changed = spec.clone();
        changed.kind = CreateCommandKind::Undo;
        assert_ne!(baseline, changed.commitment().unwrap());
        // The production complete-goal encoder consumes every declared field,
        // including fields whose values are null; no partial field comparator remains.
        let goal_value = serde_json::to_value(&spec.goal).unwrap();
        let complete = canonical_goal(&goal_value).unwrap();
        for key in goal_value.as_object().unwrap().keys() {
            let mut changed = goal_value.clone();
            changed[key] = serde_json::json!({"different_complete_goal_field":key});
            assert_ne!(
                complete,
                canonical_goal(&changed).unwrap(),
                "complete goal omitted {key}"
            );
        }
        let output = intent.run_owned_output(&mut command, None).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"observed");
        let rows = std::fs::read_to_string(storage.sessions_path()).unwrap();
        assert!(!rows.contains(&env_secret) && !rows.contains(&arg_secret));
        let receipts = owner.native_create.receipts.lock().unwrap();
        let debug = format!("{:?}", receipts.last().unwrap());
        assert!(!debug.contains(&env_secret) && !debug.contains(&arg_secret));
        let record = receipts.last().unwrap().state.lock().unwrap();
        assert_eq!(record.commitment.len(), 32);
        assert_eq!(record.generation, prepared.lifecycle_generation);
        assert!(record.root_status.is_some() && record.group_retired);
    }

    #[test]
    #[ignore = "hosted native only; requires AOE_NATIVE_CREATE_EXECUTABLE built from this source"]
    #[serial_test::serial]
    fn hosted_create_original_native_same_g_and_changed_profile_refusal() {
        let _executable = crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let storage = Arc::new(Storage::new_unwatched("default").unwrap());
        let mut prepared = Instance::new("hosted original Create", home.path().to_str().unwrap());
        let custody = CreationCustody::register(storage.clone(), &prepared).unwrap();
        let intent = CreationIntent::reserve_metadata(&storage, &mut prepared).unwrap();
        let generation = prepared.lifecycle_generation;
        let mut command = intent.owned_command("/bin/sh").unwrap();
        command
            .args([
                "-c",
                "(printf descendant > child-effect) & wait; printf '%s' \"$FROZEN_VALUE\"",
            ])
            .current_dir(home.path())
            .env("FROZEN_VALUE", "original environment");
        let output = intent.run_owned_output(&mut command, None).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"original environment");
        assert_eq!(
            std::fs::read(home.path().join("child-effect")).unwrap(),
            b"descendant"
        );
        let owner = intent.borrow_owned_create().unwrap();
        let receipts = owner.native_create.receipts.lock().unwrap();
        let receipt = receipts.last().unwrap().state.lock().unwrap().clone();
        assert_eq!(receipt.generation, generation);
        assert!(!receipt.births.is_empty());
        #[cfg(target_os = "linux")]
        {
            assert!(
                receipt.births.len() >= 2,
                "actual descendant must have its own pre-effect kernel birth ACK"
            );
            assert!(receipt.proves_retired());
        }
        #[cfg(not(target_os = "linux"))]
        assert!(
            !receipt.proves_retired(),
            "host CLI/group exit must not manufacture Darwin descendant coverage"
        );
        drop(receipts);
        #[cfg(target_os = "linux")]
        intent.retire_owned_commands().unwrap();
        let weak = Arc::downgrade(&owner);
        drop(owner);
        drop(custody);
        assert!(
            weak.upgrade().is_some(),
            "the actual original custodian outlives observers"
        );

        let mut forbidden = intent.owned_command("/bin/sh").unwrap();
        forbidden
            .args(["-c", "printf forbidden > must-not-execute"])
            .current_dir(home.path());
        let profile = storage.sessions_path().parent().unwrap().to_owned();
        let moved = profile.with_extension("original-moved");
        std::fs::rename(&profile, &moved).unwrap();
        std::fs::create_dir(&profile).unwrap();
        let marker = home.path().join("must-not-execute");
        let refused = command.owner.clone();
        assert_eq!(refused.generation(), generation);
        assert!(intent.run_owned_output(&mut forbidden, None).is_err());
        assert!(!marker.exists());
        assert_eq!(refused.generation(), generation);
        // Restore the test namespace only; no native/FS withdrawal authority is fabricated.
        std::fs::remove_dir(&profile).unwrap();
        std::fs::rename(moved, profile).unwrap();
    }

    #[test]
    #[ignore = "hosted native only; requires AOE_NATIVE_CREATE_EXECUTABLE built from this source"]
    #[serial_test::serial]
    fn hosted_create_changed_goal_refuses_target_before_effect() {
        let _executable = crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let storage = Storage::new_unwatched("default").unwrap();
        let mut prepared = Instance::new("native goal refusal", home.path().to_str().unwrap());
        let intent = CreationIntent::reserve_metadata(&storage, &mut prepared).unwrap();
        let mut command = intent.owned_command("/bin/sh").unwrap();
        command
            .args(["-c", "printf forbidden > must-not-execute"])
            .current_dir(home.path());
        storage
            .update(|rows, _| {
                rows.iter_mut()
                    .find(|r| r.id == prepared.id)
                    .unwrap()
                    .command = "changed frozen goal".into();
                Ok(())
            })
            .unwrap();
        assert!(intent.run_owned_output(&mut command, None).is_err());
        assert!(!home.path().join("must-not-execute").exists());
        assert!(intent.retire_owned_commands().is_err());
    }

    #[test]
    #[ignore = "hosted native only; requires AOE_NATIVE_CREATE_EXECUTABLE built from this source"]
    #[serial_test::serial]
    #[cfg(target_os = "linux")]
    fn hosted_create_cancel_retires_actual_original_descendants() {
        let _executable = crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let storage = Arc::new(Storage::new_unwatched("default").unwrap());
        let mut prepared = Instance::new("native cancel", home.path().to_str().unwrap());
        let intent = CreationIntent::reserve_metadata(&storage, &mut prepared).unwrap();
        let cancel = CancellationToken::new();
        let mut command = intent.owned_command("/bin/sh").unwrap();
        command.args(["-c", "(while :; do :; done) & wait"]);
        let running = intent.clone();
        let token = cancel.clone();
        let driver =
            std::thread::spawn(move || running.run_owned_output(&mut command, Some(&token)));
        let owner = intent.borrow_owned_create().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let receipts = owner.native_create.receipts.lock().unwrap();
            let admitted = receipts
                .iter()
                .any(|r| r.state.lock().unwrap().births.len() >= 2);
            drop(receipts);
            if admitted {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "original descendant admission never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        cancel.cancel();
        #[cfg(target_os = "linux")]
        {
            let result = driver.join().unwrap().unwrap();
            assert!(!result.status.success());
            intent.retire_owned_commands().unwrap();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod external_domain_test {
    use super::*;
    #[test]
    #[ignore = "hosted native only; requires actual aoe and python3 executables"]
    #[serial_test::serial]
    fn hosted_create_actual_external_ipc_stays_protected_after_cli_exit() {
        let _executable = crate::session::test_support::require_hosted_creating_native();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(home.path());
        let storage = Storage::new_unwatched("default").unwrap();
        let mut prepared = crate::session::Instance::new(
            "original IPC uncertainty",
            home.path().to_str().unwrap(),
        );
        let intent = CreationIntent::reserve_metadata(&storage, &mut prepared).unwrap();
        let mut command = intent.owned_command("python3").unwrap();
        command.args([
            "-c",
            "import socket; s=socket.socket(socket.AF_INET, socket.SOCK_STREAM); s.close()",
        ]);
        let output = intent.run_owned_output(&mut command, None).unwrap();
        assert!(output.status.success());
        let owner = intent.borrow_owned_create().unwrap();
        let receipts = owner.native_create.receipts.lock().unwrap();
        let record = receipts.last().unwrap().state.lock().unwrap();
        assert!(record.descendants_retired && record.group_retired);
        assert!(record.external_domain);
        assert!(!record.proves_retired());
        drop(record);
        drop(receipts);
        assert!(
            intent.retire_owned_commands().is_err(),
            "CLI death is not external-domain authority"
        );
    }
}

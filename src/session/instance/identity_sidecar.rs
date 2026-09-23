//! Identity extension launches and the Pi/Prime sidecars and transcripts they publish.

use super::*;
use crate::agents::SessionCaptureBackend;
use crate::session::config::container_config::PRIME_AGENT_DIR_IN_CONTAINER;

pub(super) const SESSION_SIDECAR_MAX_BYTES: usize = 4096;

pub(super) fn read_sandbox_sidecar_file(
    store: &Path,
    instance_id: &str,
    leaf: &str,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    crate::session::validate_instance_id(instance_id).ok()?;
    let root = crate::session::AnchoredDir::open(store).ok()?;
    let relative = Path::new("aoe-session").join(instance_id).join(leaf);
    root.read_regular(&relative, max_bytes).ok()?
}

/// `Unreadable` is a statement about our view of the store, never about the
/// conversation; only `Absent` may justify dropping a resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PiTranscriptState {
    Present,
    Absent,
    Unreadable,
}

/// Only a miss under a directory that reads back is `Absent`; `Path::is_file`
/// would fold a denied lookup into a miss.
fn host_transcript_state(host_path: &Path) -> PiTranscriptState {
    match std::fs::metadata(host_path) {
        Ok(metadata) if metadata.is_file() => PiTranscriptState::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match host_path.parent().map(std::fs::metadata) {
                Some(Ok(parent)) if parent.is_dir() => PiTranscriptState::Absent,
                _ => PiTranscriptState::Unreadable,
            }
        }
        _ => PiTranscriptState::Unreadable,
    }
}

impl Instance {
    #[cfg(test)]
    pub(crate) fn mark_pi_extension_launched_for_test(&mut self) {
        self.pi_extension_launched = true;
    }

    fn is_pi(&self) -> bool {
        self.resolved_capture_backend() == Some(SessionCaptureBackend::Pi)
    }

    /// Extension flag and sidecar environment for an identity-publishing backend.
    pub(super) fn identity_extension_launch(&self) -> Option<(String, String)> {
        let backend = self.resolved_capture_backend()?;
        backend.identity_publisher()?;
        if !self.is_sandboxed() {
            if backend != SessionCaptureBackend::Pi
                || launch_command::environment_defines_path(&self.resolved_host_environment())
                || !crate::agents::pi_supports_extension_flag()
            {
                return None;
            }
            let extension = launch_command::session_identity_extension_path().ok()?;
            let sidecar = crate::hooks::ensure_instance_dir_path(&self.id)
                .ok()?
                .join("session_id");
            return Some((
                format!(" -e {}", shell_escape(&extension.to_string_lossy())),
                format!(
                    "AOE_SESSION_ID_FILE={} ",
                    shell_escape(&sidecar.to_string_lossy())
                ),
            ));
        }
        let bind_dir = self.sandbox_capture_store_dir()?;
        let config = self.build_container_config().ok()?;
        let (container_root, flag, sidecar_root) = match backend {
            SessionCaptureBackend::Pi => {
                container_config::install_pi_sandbox_extension_at(&bind_dir).ok()?;
                (
                    "/root/.pi",
                    String::new(),
                    container_config::PI_SIDECAR_DIR_IN_CONTAINER.to_string(),
                )
            }
            SessionCaptureBackend::PrimeAgent => {
                self.prime_agent_capture_plan_with(&config, bind_dir.clone())?;
                container_config::install_prime_sandbox_extension_at(&bind_dir).ok()?;
                (
                    PRIME_AGENT_DIR_IN_CONTAINER,
                    format!(
                        " -e {}",
                        shell_escape(&format!(
                            "{PRIME_AGENT_DIR_IN_CONTAINER}/extensions/aoe-session-id.js"
                        ))
                    ),
                    format!("{PRIME_AGENT_DIR_IN_CONTAINER}/aoe-session"),
                )
            }
            _ => return None,
        };
        if !config.uses_default_container_home()
            || !config.path_is_mounted(&bind_dir, Path::new(container_root), true)
        {
            return None;
        }
        let container = DockerContainer::from_session_id(&self.id);
        let container_known = self
            .sandbox_info
            .as_ref()
            .is_some_and(|sandbox| sandbox.container_id.is_some())
            || container.exists().ok() == Some(true);
        if container_known && container.mount_fingerprint_matches(&config).ok()? != Some(true) {
            return None;
        }
        Some((
            flag,
            format!("AOE_SESSION_ID_FILE={sidecar_root}/{}/session_id ", self.id),
        ))
    }

    /// The conversation this Pi pane published; `any_age` drops the freshness
    /// window, which a final flush wants and a resume does not.
    pub(crate) fn pi_published_session_id(&self, any_age: bool) -> Option<String> {
        match self.pi_sidecar_source()? {
            SessionSidecarSource::HostHooks if any_age => {
                crate::hooks::read_hook_session_id_any_age(&self.id)
            }
            SessionSidecarSource::HostHooks => crate::hooks::read_hook_session_id(&self.id),
            SessionSidecarSource::SandboxDir(_) => {
                let raw = self.read_extension_sandbox_file("session_id")?;
                let id = std::str::from_utf8(&raw).ok()?.trim();
                Uuid::parse_str(id).ok().map(|_| id.to_string())
            }
        }
    }

    /// The transcript path this pane published, as the pane sees it (a
    /// `/root/.pi/` path in a container).
    pub(crate) fn pi_published_session_path(&self) -> Option<String> {
        match self.pi_sidecar_source()? {
            SessionSidecarSource::HostHooks => crate::hooks::read_hook_session_path(&self.id),
            SessionSidecarSource::SandboxDir(_) => {
                let raw = self.read_extension_sandbox_file("session_path")?;
                let path = std::str::from_utf8(&raw).ok()?.trim();
                path.starts_with('/').then(|| path.to_string())
            }
        }
    }

    pub(crate) fn pi_sidecar_source(&self) -> Option<SessionSidecarSource> {
        self.is_pi().then(|| self.extension_sidecar_source())?
    }

    fn extension_sidecar_source(&self) -> Option<SessionSidecarSource> {
        self.resolved_capture_backend()?.identity_publisher()?;
        if !self.is_sandboxed() {
            return Some(SessionSidecarSource::HostHooks);
        }
        crate::session::validate_instance_id(&self.id).ok()?;
        Some(SessionSidecarSource::SandboxDir(
            self.sandbox_capture_store_dir()?
                .join("aoe-session")
                .join(&self.id),
        ))
    }

    fn read_extension_sandbox_file(&self, leaf: &str) -> Option<Vec<u8>> {
        read_sandbox_sidecar_file(
            &self.sandbox_capture_store_dir()?,
            &self.id,
            leaf,
            SESSION_SIDECAR_MAX_BYTES,
        )
    }

    fn sandbox_store_root(&self) -> Option<crate::session::AnchoredDir> {
        crate::session::AnchoredDir::open(&self.sandbox_capture_store_dir()?).ok()
    }

    /// A published Pi path as the host filesystem sees it.
    pub(super) fn pi_host_view_of(&self, published: &str) -> Option<PathBuf> {
        if !self.is_sandboxed() {
            return Some(PathBuf::from(published));
        }
        let rest = published.strip_prefix("/root/.pi/")?;
        Some(self.sandbox_capture_store_dir()?.join(rest))
    }

    fn pi_recorded_transcript_state(&self, path: &str) -> PiTranscriptState {
        if !self.is_sandboxed() {
            return self
                .pi_host_view_of(path)
                .map_or(PiTranscriptState::Unreadable, |host| {
                    host_transcript_state(&host)
                });
        }
        let Some(relative) = path.strip_prefix("/root/.pi/").map(Path::new) else {
            return PiTranscriptState::Unreadable;
        };
        let (Some(parent), Some(root)) = (relative.parent(), self.sandbox_store_root()) else {
            return PiTranscriptState::Unreadable;
        };
        if !matches!(root.directory_modified(parent), Ok(Some(_))) {
            return PiTranscriptState::Unreadable;
        }
        match root.regular_lookup(relative) {
            Ok(Some(true)) => PiTranscriptState::Present,
            Ok(None) => PiTranscriptState::Absent,
            Ok(Some(false)) | Err(_) => PiTranscriptState::Unreadable,
        }
    }

    /// The recorded transcript path when its file name carries the id this row owns.
    fn pi_recorded_transcript(&self) -> Option<(&str, PiTranscriptState)> {
        let path = self.pi_session_path.as_deref()?;
        let id = self.agent_session_id.as_deref()?;
        pi_transcript_names(path, id).then(|| (path, self.pi_recorded_transcript_state(path)))
    }

    pub(super) fn pi_resumable_transcript(&self) -> Option<String> {
        let (path, state) = self.pi_recorded_transcript()?;
        (state == PiTranscriptState::Present).then(|| path.to_string())
    }

    /// Positive evidence only: pi writes transcripts lazily, so a never-prompted
    /// conversation has none, but an unreadable store proves nothing.
    pub(super) fn pi_recorded_transcript_missing(&self) -> bool {
        matches!(
            self.pi_recorded_transcript(),
            Some((_, PiTranscriptState::Absent))
        )
    }

    pub(crate) fn absorb_published_pi_session(&mut self) {
        let Some(path) = self.pi_published_session_path() else {
            return;
        };
        if self.pi_session_path.as_deref() == Some(path.as_str()) {
            return;
        }
        self.pi_session_path = Some(path.clone());
        // The sidecar lives in a temp dir a reboot clears; only the durable copy survives.
        if let Ok(storage) = crate::session::storage::Storage::new(
            &self.effective_profile(),
            self.resolve_file_watch(),
        ) {
            self.store_pi_session_path(&storage, self.agent_session_id.as_deref(), &path);
        }
    }

    /// Persist the transcript path a poller observation carried. False only while the write keeps
    /// failing, so the caller holds the observation for a retry.
    pub(crate) fn persist_observed_pi_transcript(
        &mut self,
        observation: &crate::session::poller::SessionIdObservation,
    ) -> bool {
        let crate::session::poller::SessionIdGuard::InstanceSidecar {
            transcript: Some(path),
        } = &observation.guard
        else {
            return true;
        };
        if !pi_transcript_names(path, &observation.sid) {
            return true;
        }
        match crate::session::storage::Storage::new(
            &self.effective_profile(),
            self.resolve_file_watch(),
        ) {
            Ok(storage) => self.persist_pi_transcript_into(&storage, &observation.sid, path),
            Err(_) => false,
        }
    }

    pub(super) fn persist_pi_transcript_into(
        &mut self,
        storage: &crate::session::storage::Storage,
        sid: &str,
        path: &str,
    ) -> bool {
        match self.store_pi_session_path(storage, Some(sid), path) {
            Some(true) => {
                self.pi_session_path = Some(path.to_owned());
                true
            }
            // The row moved to another id; the path is stale, not pending.
            Some(false) => true,
            None => false,
        }
    }

    /// Writes the path only to this row while it still holds `expected_sid`. `None` when the
    /// write failed; `Some(false)` when no row holds that id.
    pub(super) fn store_pi_session_path(
        &self,
        storage: &crate::session::storage::Storage,
        expected_sid: Option<&str>,
        path: &str,
    ) -> Option<bool> {
        match storage.update(|instances, _| {
            #[cfg(test)]
            anyhow::ensure!(
                !FAIL_PI_PATH_WRITES.with(std::cell::Cell::get),
                "injected transcript path write failure"
            );
            let row = instances
                .iter_mut()
                .find(|i| i.id == self.id && i.agent_session_id.as_deref() == expected_sid);
            Ok(row
                .map(|row| row.pi_session_path = Some(path.to_string()))
                .is_some())
        }) {
            Ok(stored) => Some(stored),
            Err(error) => {
                tracing::warn!(
                    target: "session.store",
                    instance = %self.id,
                    "could not persist the Pi transcript path the pane published: {error}",
                );
                None
            }
        }
    }

    pub(crate) fn uses_pi_session_sidecar(&self) -> bool {
        let exists = |source| match source {
            SessionSidecarSource::SandboxDir(_) => self.sandbox_store_root().is_some_and(|root| {
                root.regular_exists(&Path::new("aoe-session").join(&self.id).join("session_id"))
            }),
            SessionSidecarSource::HostHooks => crate::hooks::session_id_sidecar_exists(&self.id),
        };
        self.pi_sidecar_source()
            .is_some_and(|source| self.pi_extension_launched || exists(source))
    }

    pub(super) fn clear_pane_identity_sidecar(&self) {
        // Prime's root_session survives failed launches and is replaced only by a root.
        let host_sidecar = match self.resolved_capture_backend() {
            Some(SessionCaptureBackend::Claude | SessionCaptureBackend::HookSidecar) => true,
            Some(SessionCaptureBackend::Pi) => match self.extension_sidecar_source() {
                Some(SessionSidecarSource::HostHooks) => true,
                Some(SessionSidecarSource::SandboxDir(_)) => {
                    if let Some(root) = self.sandbox_store_root() {
                        let base = Path::new("aoe-session").join(&self.id);
                        let _ = root.remove_file(&base.join("session_id"));
                        let _ = root.remove_file(&base.join("session_path"));
                    }
                    false
                }
                None => false,
            },
            _ => false,
        };
        if host_sidecar {
            let _ = crate::hooks::unlink_session_id_via_guard(&self.id);
        }
    }

    /// A directly verified host Pi launch with an unmodified PATH may pin `--session-id`.
    pub(super) fn pi_session_id_pinnable(&self) -> bool {
        self.is_pi()
            && !self.is_sandboxed()
            && !launch_command::environment_defines_path(&self.resolved_host_environment())
            && crate::agents::pi_supports_session_id_flag()
    }
}

#[cfg(test)]
thread_local! {
    /// Fails this thread's transcript-path writes while set.
    pub(crate) static FAIL_PI_PATH_WRITES: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Whether a Pi transcript file name (`<timestamp>_<id>.jsonl`) carries `sid`.
fn pi_transcript_names(path: &str, sid: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.rsplit_once('_'))
        .and_then(|(_, tail)| tail.strip_suffix(".jsonl"))
        .is_some_and(|uuid| uuid == sid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;
    use crate::session::test_support::EnvGuard;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    #[serial_test::serial]
    fn fresh_launch_clears_every_host_identity_sidecar() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        for tool in ["cursor", "pi"] {
            let mut inst = Instance::new(tool, "/tmp/test");
            inst.tool = tool.to_string();
            inst.detect_as = tool.to_string();
            crate::hooks::write_session_id_via_guard(&inst.id, "stale-sid").unwrap();
            assert!(crate::hooks::session_id_sidecar_exists(&inst.id));

            inst.clear_pane_identity_sidecar();
            assert!(
                !crate::hooks::session_id_sidecar_exists(&inst.id),
                "{tool} retained stale pane identity"
            );
        }
    }

    #[test]
    fn clearing_the_conversation_drops_its_transcript_path() {
        let mut inst = tool_instance("pi", "/tmp/pi-clear");
        inst.agent_session_id = Some("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa".to_string());
        inst.pi_session_path = Some(
            "/store/2026-01-01T00-00-00-000Z_aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa.jsonl"
                .to_string(),
        );
        inst.resume_intent = ResumeIntent::Cleared;

        let (sid, is_existing) = inst.acquire_session_id_with(&|_| None);

        assert_eq!(sid, None, "no pin without a mint seam");
        assert!(!is_existing);
        assert_eq!(
            inst.pi_session_path, None,
            "the dropped conversation's transcript must not linger"
        );
    }

    #[test]
    #[serial_test::serial]
    fn an_unresolvable_sandbox_source_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set(&[("HOME", temp.path())]);

        let mut inst = Instance::new("pi-unresolvable", "/tmp/pi-unresolvable");
        inst.id = "../escape".to_string();
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(test_sandbox("aoe-pi-unresolvable", None));

        assert_eq!(
            inst.pi_sidecar_source(),
            None,
            "no source is the safe answer"
        );
        assert!(
            !inst.uses_pi_session_sidecar(),
            "a pane with no resolvable source does not publish"
        );
        assert!(
            !inst.supports_session_poller(),
            "and must not poll, which would read the host sidecar"
        );
        assert_eq!(inst.pi_published_session_id(true), None);
        assert_eq!(inst.pi_published_session_path(), None);

        let host = tool_instance("pi", "/tmp/pi-unresolvable");
        assert_eq!(
            host.pi_sidecar_source(),
            Some(SessionSidecarSource::HostHooks)
        );
    }

    #[test]
    #[serial_test::serial]
    fn reloaded_sandbox_session_still_finds_its_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());

        let mut inst = tool_instance("pi", "/tmp/pi-reload");
        inst.sandbox_info = Some(test_sandbox("aoe-pi-reload", None));
        inst.mark_pi_extension_launched_for_test();

        let reloaded: Instance =
            serde_json::from_str(&serde_json::to_string(&inst).unwrap()).unwrap();
        assert!(
            !reloaded.uses_pi_session_sidecar(),
            "nothing published yet, so nothing to find"
        );

        let dir = reloaded
            .pi_sidecar_source()
            .and_then(|s| match s {
                crate::session::instance::SessionSidecarSource::SandboxDir(d) => Some(d),
                _ => None,
            })
            .expect("a sandboxed pane has a bind-backed sidecar");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session_id"),
            "01a053b6-c470-78de-9d8f-bc00ef05332a\n",
        )
        .unwrap();

        assert!(
            reloaded.uses_pi_session_sidecar(),
            "the published file is what a reloaded session has to go on"
        );
        assert!(
            reloaded.supports_session_poller(),
            "poller repair must stay available after a reload"
        );
        assert_eq!(
            reloaded.pi_published_session_id(true).as_deref(),
            Some("01a053b6-c470-78de-9d8f-bc00ef05332a"),
            "and the final flush must read it"
        );
    }

    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn sandbox_pi_sidecar_reads_are_bounded_and_nonblocking() {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;

        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());
        let mut inst = tool_instance("pi", "/tmp/pi-bounded");
        inst.sandbox_info = Some(test_sandbox("aoe-pi-bounded", None));
        let SessionSidecarSource::SandboxDir(dir) = inst.pi_sidecar_source().unwrap() else {
            panic!("sandboxed Pi must publish into its config bind");
        };
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join("session_id");
        std::fs::write(&sidecar, vec![b'x'; SESSION_SIDECAR_MAX_BYTES + 1]).unwrap();
        assert_eq!(inst.pi_published_session_id(true), None);

        std::fs::remove_file(&sidecar).unwrap();
        mkfifo(&sidecar, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert_eq!(inst.pi_published_session_id(true), None);
        let poll = crate::session::capture::pi_sidecar_poll_fn(
            inst.id.clone(),
            SessionSidecarSource::SandboxDir(dir.clone()),
        );
        assert!(poll().is_none());

        std::fs::remove_file(&sidecar).unwrap();
        let root = dir.parent().and_then(std::path::Path::parent).unwrap();
        std::fs::remove_dir_all(root.join("aoe-session")).unwrap();
        let foreign = temp.path().join("foreign-aoe-session");
        std::fs::create_dir_all(foreign.join(&inst.id)).unwrap();
        std::fs::write(
            foreign.join(&inst.id).join("session_id"),
            "99999999-9999-4999-8999-999999999999",
        )
        .unwrap();
        std::os::unix::fs::symlink(&foreign, root.join("aoe-session")).unwrap();
        assert!(
            poll().is_none(),
            "the poller must anchor above the replaceable aoe-session ancestor"
        );
    }

    #[test]
    #[serial_test::serial]
    fn sandboxed_pi_with_its_own_config_dir_uses_its_mounted_sidecar() {
        const STALE_ID: &str = "01a053b6-c470-78de-9d8f-bc00ef05332a";
        let _guard = crate::session::test_support::isolate_app_dir();
        let app_dir = crate::session::get_app_dir().unwrap();

        let sandboxed_pi = |id: &str| {
            let mut inst = Instance::new(id, "/tmp/pi-own-config");
            inst.tool = "pi".to_string();
            inst.sandbox_info = Some(test_sandbox("aoe-pi-own-config", None));
            inst
        };

        let inst = sandboxed_pi("piownconfig01");
        let SessionSidecarSource::SandboxDir(stale_sidecar) = inst.pi_sidecar_source().unwrap()
        else {
            panic!("sandboxed Pi must publish into its config bind");
        };
        std::fs::create_dir_all(&stale_sidecar).unwrap();
        std::fs::write(stale_sidecar.join("session_id"), format!("{STALE_ID}\n")).unwrap();

        std::fs::write(
            app_dir.join("config.toml"),
            r#"[session.agent_config_dir]
pi = "~/.pi-personal"
"#,
        )
        .unwrap();

        let mut declared = sandboxed_pi("piownconfig01");
        let (_, env_prefix) = declared
            .identity_extension_launch()
            .expect("declared sandbox config supports the pane extension");
        assert!(env_prefix.contains("AOE_SESSION_ID_FILE=/root/.pi/aoe-session/"));
        declared.mark_pi_extension_launched_for_test();
        assert!(declared.pi_sidecar_source().is_some());
        assert!(declared.uses_pi_session_sidecar());
        assert_eq!(declared.pi_published_session_id(true), None);

        let mut cmd = String::from("pi");
        declared.apply_session_flags(&mut cmd, "test").unwrap();
        assert!(
            !cmd.contains(STALE_ID) && !cmd.contains("--session"),
            "a sidecar from the unmounted default store must not reach the launch line: {cmd:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_transcript_paths_validate_in_the_host_namespace() {
        let mut inst = tool_instance("pi", "/tmp/pi-ns");
        inst.sandbox_info = Some(test_sandbox("aoe-pi-ns", None));

        let published = "/root/.pi/sessions/--proj--/2026-01-01T00-00-00-000Z_x.jsonl";
        let host = inst
            .pi_host_view_of(published)
            .expect("a container path maps to the sandbox dir");
        let sandbox_root = inst.sandbox_capture_store_dir().unwrap();
        assert!(host.starts_with(&sandbox_root),);
        assert!(host.ends_with("sessions/--proj--/2026-01-01T00-00-00-000Z_x.jsonl"));
        assert_eq!(
            inst.pi_host_view_of("/elsewhere/x.jsonl"),
            None,
            "a path outside the bind cannot be mapped"
        );

        let host_inst = tool_instance("pi", "/tmp/pi-ns");
        assert_eq!(
            host_inst.pi_host_view_of("/home/u/.pi/x.jsonl"),
            Some(std::path::PathBuf::from("/home/u/.pi/x.jsonl"))
        );
    }

    #[test]
    #[serial_test::serial]
    fn pi_only_calls_a_transcript_missing_when_its_store_was_readable() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let id = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
        let leaf = format!("2026-01-01T00-00-00-000Z_{id}.jsonl");

        let mut inst = tool_instance("pi", "/tmp/pi-store");
        inst.agent_session_id = Some(id.to_string());
        inst.sandbox_info = Some(test_sandbox("aoe-pi-store", None));
        inst.pi_session_path = Some(format!("/root/.pi/agent/sessions/--proj--/{leaf}"));

        assert!(
            !inst.pi_recorded_transcript_missing(),
            "an uninspectable store is not evidence the conversation is gone"
        );
        assert_eq!(inst.pi_resumable_transcript(), None);

        let store = inst.sandbox_capture_store_dir().expect("bind dir");
        let sessions = store.join("agent").join("sessions").join("--proj--");
        std::fs::create_dir_all(&sessions).unwrap();

        assert!(
            inst.pi_recorded_transcript_missing(),
            "a readable store with no file is the pane's own answer"
        );

        std::fs::write(sessions.join(&leaf), "{}\n").unwrap();
        assert!(!inst.pi_recorded_transcript_missing());
        assert_eq!(
            inst.pi_resumable_transcript(),
            Some(format!("/root/.pi/agent/sessions/--proj--/{leaf}")),
            "the pane resumes its own transcript by the path it published"
        );

        inst.pi_session_path = Some(format!("/home/u/.pi/agent/sessions/--proj--/{leaf}"));
        assert!(!inst.pi_recorded_transcript_missing());
    }

    #[test]
    fn pi_never_calls_a_transcript_missing_on_a_store_it_could_not_ask_about() {
        let temp = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
        let leaf = format!("2026-01-01T00-00-00-000Z_{id}.jsonl");
        let store = temp.path().join("sessions");

        let mut inst = tool_instance("pi", "/tmp/pi-host-store");
        inst.agent_session_id = Some(id.to_string());
        inst.pi_session_path = Some(store.join(&leaf).to_string_lossy().into_owned());

        assert!(
            !inst.pi_recorded_transcript_missing(),
            "a store directory that is not there says nothing about the conversation"
        );

        std::fs::create_dir_all(&store).unwrap();
        assert!(
            inst.pi_recorded_transcript_missing(),
            "a readable store with no file is the pane's own answer"
        );

        std::fs::write(store.join(&leaf), "{}\n").unwrap();
        assert!(!inst.pi_recorded_transcript_missing());

        let not_a_dir = temp.path().join("occupied");
        std::fs::write(&not_a_dir, "").unwrap();
        inst.pi_session_path = Some(not_a_dir.join(&leaf).to_string_lossy().into_owned());
        assert!(
            !inst.pi_recorded_transcript_missing(),
            "a store path that is not a directory says nothing about the conversation"
        );
        inst.pi_session_path = Some(store.join(&leaf).to_string_lossy().into_owned());

        if !nix::unistd::Uid::effective().is_root() {
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();
            let denied = !inst.pi_recorded_transcript_missing();
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(
                denied,
                "a transcript AoE is not allowed to stat must not read as gone"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn pi_launch_drops_the_failing_session_selector_when_the_transcript_is_gone() {
        let (_hooks, _base, _hooks_tmp) = crate::hooks::test_support::BaseGuard::ready();
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());

        let profile = "pi-missing-transcript";
        let profile_dir = crate::session::get_profile_dir(profile).unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            "environment = [\"PATH=/usr/bin\"]\n",
        )
        .unwrap();

        let id = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
        let transcript = temp
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));

        let mut inst = tool_instance("pi", "/tmp/pi-gone");
        inst.command = "pi".to_string();
        inst.source_profile = profile.to_string();
        inst.agent_session_id = Some(id.to_string());
        inst.pi_session_path = Some(transcript.to_string_lossy().into_owned());

        let mut cmd = "pi".to_string();
        let resumed = inst.apply_session_flags(&mut cmd, "test").unwrap();
        assert_eq!(cmd, "pi", "no selector may be handed to a doomed resume");
        assert!(!resumed, "nothing was resumed");
        assert_eq!(
            inst.agent_session_id.as_deref(),
            Some(id),
            "acquisition leaves the row's id alone; only the selector is dropped"
        );

        std::fs::write(&transcript, "{}\n").unwrap();
        let mut cmd = "pi".to_string();
        assert!(inst.apply_session_flags(&mut cmd, "test").unwrap());
        assert_eq!(cmd, format!("pi --session '{}'", transcript.display()));
    }

    #[test]
    #[serial_test::serial]
    fn an_observed_transcript_path_stays_retryable_until_stored() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "pi-path-retry";
        let sid = "01a05234-8889-72e2-a7c9-7ebc27b25b78";
        let mut inst = Instance::new("pipathretry00001", "/tmp/pi-path-retry");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(test_sandbox("aoe-pi-path-retry", None));
        inst.agent_session_id = Some(sid.to_string());
        let mut storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let seed = inst.clone();
        storage
            .update(|instances, _| {
                *instances = vec![seed.clone()];
                Ok(())
            })
            .unwrap();
        let stored = |storage: &crate::session::storage::Storage| {
            storage.load().unwrap()[0].pi_session_path.clone()
        };
        let published =
            format!("/root/.pi/agent/sessions/--proj--/2026-01-01T00-00-00-000Z_{sid}.jsonl");

        // No sidecar exists to re-read: only the observation carries the path.
        storage.set_fail_writes_for_test(true);
        assert!(!inst.persist_pi_transcript_into(&storage, sid, &published));
        assert_eq!(
            inst.pi_session_path, None,
            "an unstored path must not look current"
        );
        storage.set_fail_writes_for_test(false);
        assert_eq!(stored(&storage), None);

        let observation = crate::session::poller::SessionIdObservation::instance_sidecar(
            sid.to_string(),
            Some(published.clone()),
        );
        assert!(inst.persist_observed_pi_transcript(&observation));
        assert_eq!(stored(&storage), Some(published.clone()));
        assert_eq!(inst.pi_session_path, Some(published));

        let foreign = crate::session::poller::SessionIdObservation::instance_sidecar(
            sid.to_string(),
            Some("/root/.pi/agent/sessions/--proj--/2026-01-01T00-00-00-000Z_other.jsonl".into()),
        );
        let before = stored(&storage);
        assert!(inst.persist_observed_pi_transcript(&foreign));
        assert_eq!(
            stored(&storage),
            before,
            "a path naming another id is not stored"
        );

        inst.pi_session_path = None;
        let moved_on =
            format!("/root/.pi/agent/sessions/--proj--/2026-01-02T00-00-00-000Z_{sid}.jsonl");
        storage
            .update(|instances, _| {
                instances[0].agent_session_id = Some("row-moved-on".into());
                Ok(())
            })
            .unwrap();
        assert!(
            inst.persist_pi_transcript_into(&storage, sid, &moved_on),
            "a row that moved to another id makes the path stale, not pending"
        );
        assert_eq!(
            stored(&storage),
            before,
            "a row that no longer holds the id is not written"
        );
    }

    #[test]
    fn pi_resumes_by_published_path_only_for_its_own_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
        let other = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
        let mine = temp
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::write(&mine, "{}\n").unwrap();
        let theirs = temp
            .path()
            .join(format!("2026-01-01T00-00-00-000Z_{other}.jsonl"));
        std::fs::write(&theirs, "{}\n").unwrap();

        let mut inst = tool_instance("pi", "/tmp/pi-path");
        inst.agent_session_id = Some(id.to_string());

        assert_eq!(
            inst.pi_resumable_transcript(),
            None,
            "no path published yet"
        );

        inst.pi_session_path = Some(mine.to_string_lossy().to_string());
        assert_eq!(
            inst.pi_resumable_transcript().as_deref(),
            Some(mine.to_string_lossy().as_ref()),
            "the pane's own transcript resumes by path"
        );

        inst.pi_session_path = Some(theirs.to_string_lossy().to_string());
        assert_eq!(
            inst.pi_resumable_transcript(),
            None,
            "a path for another conversation must not be resumed"
        );

        inst.agent_session_id = Some("aaaaaaaa".to_string());
        inst.pi_session_path = Some(mine.to_string_lossy().to_string());
        assert_eq!(inst.pi_resumable_transcript(), None, "partial pin");
        inst.agent_session_id = Some(id.to_string());

        inst.pi_session_path = Some(
            temp.path()
                .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl.gone"))
                .to_string_lossy()
                .to_string(),
        );
        assert_eq!(inst.pi_resumable_transcript(), None, "the file must exist");
    }

    #[test]
    fn pi_relaunch_of_an_unwritten_pin_uses_the_creating_flag() {
        let inst = tool_instance("pi", "/tmp/pi-pinned");

        let minted = Some("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa");
        for (label, pinnable, sid, explicit, expected) in [
            ("minted, pinnable", true, minted, false, false),
            ("minted, old binary", false, minted, true, true),
            ("user-pinned partial", true, Some("aaaaaaaa"), true, true),
            ("user-pinned full uuid", true, minted, true, true),
            ("no id", false, None, false, false),
        ] {
            assert_eq!(
                inst.resume_flag_arm_is_existing(sid.is_some(), pinnable, sid, explicit),
                expected,
                "{label}"
            );
        }

        let claude = tool_instance("claude", "/tmp/pi-pinned");
        assert!(claude.resume_flag_arm_is_existing(true, true, minted, false));
        assert!(!claude.pi_session_id_pinnable());
    }
}

//! Session-id capture for Oh My Pi (`omp`): resolving a capture plan,
//! gating the launch marker, and recording the launch generation.

use super::*;

pub(crate) fn persist_omp_session_to_storage(
    profile: &str,
    instance_id: &str,
    session_id: &str,
    expected_prior: Option<&str>,
    expected_generation: Option<&str>,
    file_watch: &std::sync::Arc<crate::file_watch::FileWatchService>,
) -> SidWrite {
    persist_session_to_storage_guarded(
        profile,
        instance_id,
        session_id,
        expected_prior,
        true,
        expected_generation,
        file_watch,
    )
}

/// Build a post-login routing fingerprint check without embedding any routing
/// value in argv. The pane hashes its live environment through stdin; if no
/// SHA-256 utility exists or startup files changed routing, capture is skipped
/// and the original OMP command runs untouched.
fn omp_routing_fingerprint_check(plan: &OmpCapturePlan) -> String {
    let keys = crate::session::capture::OMP_STORE_ENV_KEYS.join(" ");
    format!(
        "route_payload() {{ \
           for k in {keys}; do \
             eval \"s=\\${{$k+x}};v=\\${{$k-}}\"; \
             if [ \"$s\" ]; then printf '%s\\0001\\000%s\\000' \"$k\" \"$v\"; \
             else printf '%s\\0000\\000\\000' \"$k\"; fi; \
           done; \
         }}; \
          if command -v sha256sum >/dev/null 2>&1; then \
           route_fingerprint=$(route_payload | command sha256sum) || launch_raw; \
          elif command -v shasum >/dev/null 2>&1; then \
           route_fingerprint=$(route_payload | command shasum -a 256) || launch_raw; \
          else launch_raw; fi; \
          route_fingerprint=${{route_fingerprint%% *}}; \
          [ \"$route_fingerprint\" = {} ] || launch_raw; ",
        shell_escape(&plan.routing_fingerprint)
    )
}

/// Wait briefly for the parent to publish this launch generation's hidden
/// capture metadata. A timeout runs the uninstrumented command, so capture
/// fails closed without preventing the agent from starting.
pub(super) fn gate_omp_launch(
    raw_command: &str,
    marked_command: &str,
    plan: &OmpCapturePlan,
) -> String {
    let expected = format!(
        "{}={}",
        crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
        plan.launch_id
    );
    let script = format!(
        "expected={}; ready=; attempt=0; \
         while [ \"$attempt\" -lt 100 ]; do \
           ready=$(tmux show-environment -h -t \"$TMUX_PANE\" {} 2>/dev/null) || ready=; \
           [ \"$ready\" = \"$expected\" ] && break; \
           attempt=$((attempt + 1)); sleep 0.05; \
         done\n\
         if [ \"$ready\" = \"$expected\" ]; then\n\
           exec env {marked_command}\n\
         else\n\
           exec env {raw_command}\n\
         fi",
        shell_escape(&expected),
        crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
    );
    shell_stdin_command("sh", false, &script, "AOE_OMP_CAPTURE_GATE")
}

/// Apply profile assignments to the marker wrapper itself, not only to its
/// eventual OMP command. The routing fingerprint must observe the same
/// effective environment that OMP inherits.
pub(super) fn wrap_omp_host_launch(
    env_prefix: &str,
    tool_cmd: &str,
    plan: &OmpCapturePlan,
) -> String {
    format!("{env_prefix}{}", wrap_omp_launch(tool_cmd, plan))
}

/// Bind capture to the exact launch PTY. A valid pre-launch breadcrumb is
/// rewritten to a lexically different but equivalent session path; the marker
/// records that pending path so capture waits until OMP rewrites the breadcrumb.
/// If no breadcrumb exists, install a fresh sentinel from a private directory
/// by a no-clobber hardlink. Invalid breadcrumbs, collisions, symlinks, and
/// write failures launch raw OMP without capture.
pub(super) fn wrap_omp_launch(tool_cmd: &str, plan: &OmpCapturePlan) -> String {
    let breadcrumb_tmp_leaf = format!(".aoe-omp-breadcrumb-{}", plan.launch_id);
    let pending_sentinel = plan
        .layout
        .managed_sessions
        .join(format!(".aoe-pending-{}", plan.launch_id))
        .join(format!("aoe-pending_{}.jsonl", plan.launch_id));
    let fingerprint_check = omp_routing_fingerprint_check(plan);
    let marked_launch = format!(
        "tool_cmd={}; \
         launch_raw() {{ exec sh -c \"$tool_cmd\"; }}; \
         {}\
         tty_path=$(tty) || launch_raw; \
         terminal_id=${{tty_path#/dev/}}; \
         [ \"$terminal_id\" != \"$tty_path\" ] && [ -n \"$terminal_id\" ] || launch_raw; \
         terminal_id=$(printf '%s' \"$terminal_id\" | tr '/' '-') || launch_raw; \
         terminal_dir={}; \
         [ -d \"$terminal_dir\" ] && [ ! -L \"$terminal_dir\" ] || launch_raw; \
         pending=; \
         breadcrumb=\"$terminal_dir/$terminal_id\"; \
         if [ -f \"$breadcrumb\" ] && [ ! -L \"$breadcrumb\" ]; then \
           breadcrumb_bytes=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | LC_ALL=C wc -c | tr -d '[:space:]'); \
           case \"$breadcrumb_bytes\" in ''|*[!0-9]*) breadcrumb_bytes=16385 ;; esac; \
           [ \"$breadcrumb_bytes\" -le 16384 ] || launch_raw; \
           crumb_cwd=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '1p') || launch_raw; \
           crumb_path=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '2p') || launch_raw; \
           crumb_marker=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '3p') || launch_raw; \
           crumb_lines=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '$=') || launch_raw; \
           case \"$crumb_lines:$crumb_marker\" in '2:'|'3:fresh') ;; *) launch_raw ;; esac; \
           [ -n \"$crumb_cwd\" ] && [ -n \"$crumb_path\" ] || launch_raw; \
           case \"$crumb_path\" in \
             /*) crumb_dir=${{crumb_path%/*}}; crumb_base=${{crumb_path##*/}}; \
                 [ -n \"$crumb_dir\" ] || crumb_dir=/; \
                 if [ \"$crumb_dir\" = / ]; then pending=\"/./$crumb_base\"; \
                 else pending=\"$crumb_dir/./$crumb_base\"; fi ;; \
             *) pending=\"./$crumb_path\" ;; \
           esac; \
           if [ \"$crumb_marker\" = fresh ]; then \
             rewritten_bytes=$(printf '%s\\n%s\\nfresh\\n' \"$crumb_cwd\" \"$pending\" | LC_ALL=C wc -c | tr -d '[:space:]'); \
           else \
             rewritten_bytes=$(printf '%s\\n%s\\n' \"$crumb_cwd\" \"$pending\" | LC_ALL=C wc -c | tr -d '[:space:]'); \
           fi; \
           case \"$rewritten_bytes\" in ''|*[!0-9]*) rewritten_bytes=16385 ;; esac; \
           [ \"$rewritten_bytes\" -le 16384 ] || launch_raw; \
           breadcrumb_tmp_dir=\"$terminal_dir\"/{}.tmp.$$; \
           (umask 077; mkdir \"$breadcrumb_tmp_dir\") || launch_raw; \
           breadcrumb_tmp=\"$breadcrumb_tmp_dir/breadcrumb\"; \
           if [ \"$crumb_marker\" = fresh ]; then \
             (umask 077; set -C; printf '%s\\n%s\\nfresh\\n' \"$crumb_cwd\" \"$pending\" > \"$breadcrumb_tmp\") || launch_raw; \
           else \
             (umask 077; set -C; printf '%s\\n%s\\n' \"$crumb_cwd\" \"$pending\" > \"$breadcrumb_tmp\") || launch_raw; \
           fi; \
           mv -f -- \"$breadcrumb_tmp\" \"$breadcrumb\" || launch_raw; \
           rmdir \"$breadcrumb_tmp_dir\" 2>/dev/null || :; \
         elif [ ! -e \"$breadcrumb\" ] && [ ! -L \"$breadcrumb\" ]; then \
           crumb_cwd=$(pwd -P) || launch_raw; \
           [ -n \"$crumb_cwd\" ] || launch_raw; \
           pending={}; \
           rewritten_bytes=$(printf '%s\\n%s\\nfresh\\n' \"$crumb_cwd\" \"$pending\" | LC_ALL=C wc -c | tr -d '[:space:]'); \
           case \"$rewritten_bytes\" in ''|*[!0-9]*) rewritten_bytes=16385 ;; esac; \
           [ \"$rewritten_bytes\" -le 16384 ] || launch_raw; \
           breadcrumb_tmp_dir=\"$terminal_dir\"/{}.tmp.$$; \
           (umask 077; mkdir \"$breadcrumb_tmp_dir\") || launch_raw; \
           breadcrumb_tmp=\"$breadcrumb_tmp_dir/breadcrumb\"; \
           (umask 077; set -C; printf '%s\\n%s\\nfresh\\n' \"$crumb_cwd\" \"$pending\" > \"$breadcrumb_tmp\") || launch_raw; \
           ln -n \"$breadcrumb_tmp\" \"$breadcrumb\" || launch_raw; \
           rm -f -- \"$breadcrumb_tmp\" || launch_raw; \
           rmdir \"$breadcrumb_tmp_dir\" 2>/dev/null || :; \
         else \
           launch_raw; \
         fi; \
         [ -n \"$pending\" ] || launch_raw; \
         marker_tmp_dir={}.tmp.$$; \
         (umask 077; mkdir \"$marker_tmp_dir\") || launch_raw; \
         marker_tmp=\"$marker_tmp_dir/marker\"; \
         (umask 077; set -C; printf '%s\\n%s\\n%s\\n%s\\n' \"$terminal_id\" {} \"$pending\" \"$route_fingerprint\" > \"$marker_tmp\") || launch_raw; \
         mv -f -- \"$marker_tmp\" {} || launch_raw; \
         rmdir \"$marker_tmp_dir\" 2>/dev/null || :; \
         exec sh -c \"$tool_cmd\"",
        shell_escape(tool_cmd),
        fingerprint_check,
        shell_escape(&plan.layout.terminal_sessions.to_string_lossy()),
        shell_escape(&breadcrumb_tmp_leaf),
        shell_escape(&pending_sentinel.to_string_lossy()),
        shell_escape(&breadcrumb_tmp_leaf),
        shell_escape(&plan.launch_marker),
        shell_escape(&plan.launch_id),
        shell_escape(&plan.launch_marker),
    );
    shell_stdin_command("sh", false, &marked_launch, "AOE_OMP_MARKED_LAUNCH")
}

impl Instance {
    /// Capture is safe only for the built-in OMP command and a transparent,
    /// parseable argv. Benign arguments remain supported; store-selecting
    /// flags are interpreted by the capture resolver.
    pub(super) fn omp_capture_options(&self) -> Option<OmpCliCaptureOptions> {
        if self.tool != "omp" || self.has_command_override() {
            return None;
        }
        let args = crate::session::config::quote_model_value_in_args(&self.extra_args);
        OmpCliCaptureOptions::parse(&args).ok()
    }

    /// Resolve OMP's store, routing environment, and per-launch marker after
    /// `on_launch`. Environment values remain transient; the marker and layout
    /// survive in capture metadata.
    pub(super) fn resolve_omp_capture_plan(
        &self,
        options: &OmpCliCaptureOptions,
    ) -> Option<OmpCapturePlan> {
        let resolved = if self.is_sandboxed() {
            let sandbox = self.sandbox_info.as_ref()?;
            let launch_environment = resolved_sandbox_environment(
                &self.source_profile,
                sandbox,
                Path::new(&self.project_path),
            );
            resolve_omp_store_layout_in_container_with_environment(
                &sandbox.container_name,
                &self.container_workdir(),
                &launch_environment,
                options,
            )
        } else {
            resolve_omp_store_layout_with_environment(
                &self.resolved_host_environment(),
                &self.project_path,
                options,
            )
        };
        let (layout, routing_fingerprint) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::warn!(
                    target: "session.store",
                    instance = %self.id,
                    "OMP capture disabled because launch routing could not be resolved: {error}"
                );
                return None;
            }
        };
        let launch_marker = if self.is_sandboxed() {
            omp_sandbox_launch_marker(&self.id)
        } else {
            match crate::hooks::ensure_instance_dir_path(&self.id) {
                Ok(path) => path.join("omp_launch").to_string_lossy().into_owned(),
                Err(error) => {
                    tracing::warn!(
                        target: "session.store",
                        instance = %self.id,
                        "OMP capture disabled because its launch marker directory is unavailable: {error}"
                    );
                    return None;
                }
            }
        };
        Some(OmpCapturePlan {
            layout,
            routing_fingerprint,
            launch_id: Uuid::new_v4().to_string(),
            launch_marker,
            container_runtime: self.is_sandboxed().then(|| {
                crate::session::config::Config::load()
                    .map(|config| config.sandbox.container_runtime)
                    .unwrap_or_default()
            }),
        })
    }

    /// Reconstruct metadata only for a legacy pane which predates launch
    /// snapshots. New launches transport their already-resolved plan directly
    /// into `finalize_launch` and never call this method.
    fn resolve_legacy_omp_capture_metadata(
        &self,
        options: &OmpCliCaptureOptions,
        launched_at_ms: u64,
    ) -> Option<OmpCaptureMetadata> {
        if launched_at_ms == 0 || self.is_sandboxed() {
            return None;
        }
        let layout = resolve_omp_store_layout(
            &self.resolved_host_environment(),
            &self.project_path,
            options,
        )
        .ok()?;
        Some(OmpCaptureMetadata {
            layout,
            launched_at_ms,
            launch_id: format!("legacy-{}-{launched_at_ms}", self.id),
            launch_marker: String::new(),
            routing_fingerprint: String::new(),
            container_runtime: None,
        })
    }

    /// Load typed launch metadata directly from tmux. A pane carrying the
    /// regular bootstrap generation is modern; if its hidden metadata is
    /// absent, capture stays disabled instead of being legacy-migrated.
    pub(super) fn omp_capture_metadata(
        &self,
        session_name: &str,
        options: &OmpCliCaptureOptions,
        launched_at_ms: Option<u64>,
    ) -> Option<OmpCaptureMetadata> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyLayout {
            sessions: PathBuf,
            terminal_sessions: PathBuf,
            kind: OmpStoreKind,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyMetadata {
            layout: LegacyLayout,
            launched_at_ms: u64,
        }

        let bootstrap_generation =
            || crate::tmux::env::get_env(session_name, crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY);
        if let Some(encoded) = crate::tmux::env::get_hidden_env(
            session_name,
            crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
        ) {
            if let Ok(mut metadata) = serde_json::from_str::<OmpCaptureMetadata>(&encoded) {
                if metadata.launch_id.trim().is_empty() {
                    if bootstrap_generation().is_some() || self.omp_capture_generation.is_some() {
                        return None;
                    }
                    metadata.launch_id = format!("legacy-{}-{}", self.id, metadata.launched_at_ms);
                    let encoded = serde_json::to_string(&metadata).ok()?;
                    crate::tmux::env::set_hidden_env(
                        session_name,
                        crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                        &encoded,
                    )
                    .ok()?;
                }
                if validate_omp_capture_metadata(&metadata).is_err() {
                    return None;
                }
                let ready_generation = || {
                    crate::tmux::env::get_hidden_env(
                        session_name,
                        crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
                    )
                };
                match bootstrap_generation() {
                    Some(pane_generation)
                        if pane_generation == metadata.launch_id
                            && self.omp_capture_generation.as_deref()
                                == Some(metadata.launch_id.as_str())
                            && ready_generation().as_deref()
                                == Some(metadata.launch_id.as_str()) => {}
                    Some(_) => return None,
                    None if !metadata.launch_marker.is_empty()
                        || self.omp_capture_generation.is_some() =>
                    {
                        return None;
                    }
                    None => {}
                }
                return Some(metadata);
            }

            let legacy: LegacyMetadata = serde_json::from_str(&encoded).ok()?;
            if legacy.launched_at_ms == 0
                || !legacy.layout.sessions.is_absolute()
                || !legacy.layout.terminal_sessions.is_absolute()
                || bootstrap_generation().is_some()
                || self.omp_capture_generation.is_some()
            {
                return None;
            }
            let managed_sessions = legacy.layout.terminal_sessions.parent()?.join("sessions");
            let metadata = OmpCaptureMetadata {
                layout: crate::session::capture::OmpStoreLayout {
                    sessions: legacy.layout.sessions,
                    managed_sessions,
                    terminal_sessions: legacy.layout.terminal_sessions,
                    kind: legacy.layout.kind,
                },
                launched_at_ms: legacy.launched_at_ms,
                launch_id: format!("legacy-{}-{}", self.id, legacy.launched_at_ms),
                launch_marker: String::new(),
                routing_fingerprint: String::new(),
                container_runtime: None,
            };
            let encoded = serde_json::to_string(&metadata).ok()?;
            crate::tmux::env::set_hidden_env(
                session_name,
                crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                &encoded,
            )
            .ok()?;
            return Some(metadata);
        }

        if bootstrap_generation().is_some() || self.omp_capture_generation.is_some() {
            return None;
        }
        let legacy_watermark_ms = launched_at_ms.or_else(|| {
            crate::tmux::Session::from_name(session_name)
                .created_at_ms()
                .ok()
        })?;
        let metadata = self.resolve_legacy_omp_capture_metadata(options, legacy_watermark_ms)?;
        if bootstrap_generation().is_some()
            || self.omp_capture_generation.is_some()
            || crate::tmux::env::get_hidden_env(
                session_name,
                crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
            )
            .is_some()
        {
            return None;
        }
        let encoded = serde_json::to_string(&metadata).ok()?;
        crate::tmux::env::set_hidden_env(
            session_name,
            crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
            &encoded,
        )
        .ok()?;
        Some(metadata)
    }

    /// Publish the capture plan's generation, or mint a tombstone generation
    /// for an OMP launch whose capture plan could not be resolved.
    pub(super) fn publish_omp_launch_generation(
        &mut self,
        profile: &str,
        metadata: Option<&OmpCaptureMetadata>,
        expected_prior: Option<&str>,
    ) -> bool {
        if let Some(metadata) = metadata {
            return self.persist_omp_capture_generation(
                profile,
                &metadata.launch_id,
                expected_prior,
            );
        }
        if self.tool != "omp" {
            return true;
        }
        // No capture plan: persist a distinct sentinel so any observation still
        // carrying the prior generation fails the CAS. The `tombstone-` prefix
        // marks it as never-captured in storage/logs (compared for equality,
        // never parsed).
        let tombstone = format!("tombstone-{}", Uuid::new_v4());
        self.persist_omp_capture_generation(profile, &tombstone, expected_prior)
    }

    /// CAS-persist one OMP capture generation and reload the durable winner
    /// when another writer has already advanced it.
    fn persist_omp_capture_generation(
        &mut self,
        profile: &str,
        generation: &str,
        expected_prior: Option<&str>,
    ) -> bool {
        let storage =
            match crate::session::storage::Storage::new(profile, self.resolve_file_watch()) {
                Ok(storage) => storage,
                Err(error) => {
                    tracing::warn!(
                        target: "session.store",
                        instance = %self.id,
                        "Failed to open storage for OMP generation persist: {error}"
                    );
                    return false;
                }
            };
        let outcome = storage.update(|instances, _groups| {
            let Some(instance) = instances.iter_mut().find(|instance| instance.id == self.id)
            else {
                return Ok(SidWrite::Failed);
            };
            if instance.omp_capture_generation.as_deref() != expected_prior {
                return Ok(SidWrite::Skipped);
            }
            instance.omp_capture_generation = Some(generation.to_string());
            Ok(SidWrite::Applied)
        });
        if matches!(outcome, Ok(SidWrite::Applied)) {
            self.omp_capture_generation = Some(generation.to_string());
            return true;
        }
        if let Ok(instances) = storage.load() {
            if let Some(instance) = instances.iter().find(|instance| instance.id == self.id) {
                self.omp_capture_generation = instance.omp_capture_generation.clone();
            }
        }
        tracing::warn!(
            target: "session.store",
            instance = %self.id,
            generation,
            "OMP generation CAS failed; launch continues with capture disabled"
        );
        false
    }

    /// Last-chance exact-pane OMP capture while the old pane still exists.
    pub(super) fn capture_omp_before_restart(&mut self, profile: &str) {
        self.reconcile_from_disk();
        if self.tool != "omp"
            || self.agent_session_id.is_some()
            || (self.is_sandboxed() && self.omp_capture_generation.is_none())
        {
            return;
        }
        let Some(captured) = self.try_retroactive_capture() else {
            return;
        };
        match persist_omp_session_to_storage(
            profile,
            &self.id,
            &captured,
            None,
            self.omp_capture_generation.as_deref(),
            &self.resolve_file_watch(),
        ) {
            SidWrite::Applied => {
                self.agent_session_id = Some(captured);
                self.resume_probe_failed_sid = None;
            }
            SidWrite::Skipped => self.reconcile_from_disk(),
            SidWrite::Failed => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::launch_command::wrap_command_ignore_suspend;

    #[test]
    fn omp_capture_accepts_benign_args_and_rejects_opaque_launches() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.tool = "omp".to_string();
        inst.extra_args =
            "--model sonnet --profile first --profile=work --session-dir '/tmp/omp sessions'"
                .to_string();

        let options = inst
            .omp_capture_options()
            .expect("benign argv must capture");
        assert_eq!(options.profile.as_deref(), Some("work"));
        assert_eq!(
            options.session_dir.as_deref(),
            Some(std::path::Path::new("/tmp/omp sessions"))
        );
        inst.extra_args = "--model ${model:---profile=work}".to_string();
        assert!(
            inst.omp_capture_options().is_some(),
            "model values are shell-quoted before both capture parsing and launch"
        );
        for arg in ["--continue", "-c", "--continue=false"] {
            inst.extra_args = arg.to_string();
            assert!(
                inst.omp_capture_options().is_some(),
                "{arg} remains a transparent OMP launch argument"
            );
        }
        inst.extra_args = "--model sonnet[1m]".to_string();
        assert!(
            inst.omp_capture_options().is_some(),
            "the launch path quotes model context suffixes before shell expansion"
        );

        inst.extra_args = "--no-session".to_string();
        assert!(inst.omp_capture_options().is_none());
        inst.extra_args = "'unterminated".to_string();
        assert!(inst.omp_capture_options().is_none());
        inst.extra_args.clear();
        inst.command = "omp".to_string();
        assert!(inst.omp_capture_options().is_some());
        inst.command = "omp-wrapper".to_string();
        assert!(inst.omp_capture_options().is_none());
    }

    #[test]
    fn omp_launch_rejects_api_keys_in_extra_args() {
        let mut instance = Instance::new("test", "/tmp/test");
        instance.tool = "omp".to_string();
        for extra_args in [
            "--api-key secret",
            "--api-key=secret",
            "--api-key$EMPTY secret",
        ] {
            instance.extra_args = extra_args.to_string();
            let error = instance
                .build_launch_command()
                .err()
                .expect("inline OMP credentials must abort before launch");
            if extra_args.contains('$') {
                assert!(error.to_string().contains("opaque shell syntax"), "{error}");
            } else {
                assert!(
                    error.to_string().contains("through the environment"),
                    "{extra_args}: {error}"
                );
            }
        }
    }

    fn omp_test_plan() -> OmpCapturePlan {
        OmpCapturePlan {
            layout: crate::session::capture::OmpStoreLayout {
                sessions: PathBuf::from("/tmp/omp/sessions"),
                managed_sessions: PathBuf::from("/tmp/omp/managed/sessions"),
                terminal_sessions: PathBuf::from("/tmp/omp/terminal-sessions"),
                kind: OmpStoreKind::Managed,
            },
            routing_fingerprint: "a".repeat(64),
            launch_id: "launch-unit-123".to_string(),
            launch_marker: "/tmp/aoe-omp.marker".to_string(),
            container_runtime: None,
        }
    }

    #[test]
    fn omp_routing_fingerprint_check_never_embeds_values() {
        let routing_values = ["/resolved omp/home", "default", "$resolved-secret-route"];
        let plan = omp_test_plan();

        let command = omp_routing_fingerprint_check(&plan);
        for value in routing_values {
            assert!(
                !command.contains(value),
                "resolved routing value leaked into command: {value}"
            );
        }
        assert!(command.contains("${$k+x}"));
        assert!(command.contains("${$k-}"));
        assert!(command.contains("sha256sum"));
        assert!(command.contains("shasum -a 256"));
        assert!(command.contains(&plan.routing_fingerprint));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn omp_routing_fingerprint_accepts_matching_live_env_and_rejects_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (_, fingerprint) = resolve_omp_store_layout_with_environment(
            &[format!("HOME={}", home.display())],
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        let mut plan = omp_test_plan();
        plan.routing_fingerprint = fingerprint;
        let check = omp_routing_fingerprint_check(&plan);
        let script = format!("launch_raw() {{ printf raw; exit 0; }}; {check}printf captured");
        let run = |live_home: &Path| {
            let mut command = std::process::Command::new("sh");
            command
                .args(["-c", &script])
                .env_clear()
                // `env_clear` is here to control which OMP_STORE_ENV_KEYS the
                // fingerprint folds in, not to pin a filesystem layout. The
                // child still needs a PATH that resolves `sha256sum` / `tr`,
                // so it inherits the caller's.
                .env("PATH", std::env::var_os("PATH").unwrap_or_default());
            // Pin the exact routing environment a host launch installs into the
            // pane for this HOME, so the check reproduces the fingerprint's env
            // instead of assuming the ambient OMP_STORE_ENV_KEYS are empty. They
            // are not on every runner, and host_launcher_environment folds them
            // into the fingerprint, so forcing empties here would diverge from
            // the digest on any host that exports one of those keys.
            for mutation in omp_host_routing_environment(&[format!("HOME={}", live_home.display())])
            {
                match mutation {
                    tmux::PaneEnvMutation::Set { key, value } => {
                        command.env(key, value);
                    }
                    tmux::PaneEnvMutation::Unset { key } => {
                        command.env_remove(key);
                    }
                }
            }
            command.output().unwrap()
        };

        assert_eq!(run(&home).stdout, b"captured");
        assert_eq!(run(&tmp.path().join("drifted")).stdout, b"raw");
    }

    #[test]
    fn omp_launch_wrapper_hashes_live_routing_and_marker_is_noclobber() {
        let routing_values = ["/sandbox home", "default", "/secret/$sandbox-route's"];
        let plan = omp_test_plan();

        let command = wrap_omp_launch("omp --profile work", &plan);
        for value in routing_values {
            assert!(
                !command.contains(value),
                "resolved routing value leaked into wrapper: {value}"
            );
        }
        assert!(command.contains("route_payload"));
        assert!(command.contains("route_fingerprint"));
        assert!(command.contains("tty_path=$(tty) || launch_raw"));
        assert!(command.contains("terminal_id=${tty_path#/dev/}"));
        assert!(command.contains("tr"));
        assert!(command.contains("launch-unit-123"));
        assert!(command.contains("/tmp/aoe-omp.marker"));
        assert!(command.contains("pending="));
        assert!(command.contains("pending=\"./$crumb_path\""));
        assert!(command.contains(".aoe-pending-launch-unit-123"));
        assert!(command.contains("aoe-pending_launch-unit-123.jsonl"));
        assert!(command.contains("mkdir \"$breadcrumb_tmp_dir\""));
        assert!(command.contains("ln -n \"$breadcrumb_tmp\" \"$breadcrumb\" || launch_raw"));

        assert!(command.contains("mkdir \"$marker_tmp_dir\""));
        assert!(command.contains("(umask 077; set -C; printf"));
        assert!(command.contains("> \"$marker_tmp\") || launch_raw"));
        assert!(!command.contains(">| \"$marker_tmp\""));
        assert!(!command.contains("/dev/pts/*"));
        assert!(command.find("printf").unwrap() < command.rfind("exec sh -c").unwrap());
    }

    /// The shim dir, then the caller's `PATH`. Shim first, so the fake `tmux`
    /// wins over any real one; inherited, so a host whose coreutils sit
    /// outside the FHS layout still resolves them. `OsString` throughout: a
    /// `PATH` entry need not be UTF-8.
    #[cfg(unix)]
    fn test_path_with_shim(bin: &std::path::Path) -> std::ffi::OsString {
        // An unset or empty PATH is handled separately: `split_paths("")`
        // yields one EMPTY entry, and an empty PATH element means the current
        // directory, so joining it would hand the child `<shim>:` and put cwd
        // on its PATH.
        let Some(inherited) = std::env::var_os("PATH").filter(|p| !p.is_empty()) else {
            return bin.as_os_str().to_os_string();
        };
        let entries = std::iter::once(bin.to_path_buf())
            .chain(std::env::split_paths(&inherited))
            .collect::<Vec<_>>();
        std::env::join_paths(entries).expect("PATH entries contain no separator")
    }

    /// `#[serial]` because this reads the inherited PATH, and every test that
    /// scrubs PATH process-globally carries that same default-key annotation:
    /// `crate::acp::node`, `crate::acp::acp_client`, and
    /// `crate::update::install`.
    /// Not an `EnvGuard` lock: none of them takes `test_support::ENV_LOCK`, so
    /// a guard would exclude unrelated guard users and leave this window open.
    /// A future PATH mutator outside the default serial group would reopen it.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn omp_capture_gate_executes_nested_stdin_scripts() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tmux = bin.join("tmux");
        let expected = format!(
            "{}=launch-unit-123",
            crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY
        );
        std::fs::write(&tmux, format!("#!/bin/sh\nprintf '%s\\n' {expected:?}\n")).unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();

        let output = temp.path().join("result");
        let raw = format!("printf raw > {}", shell_escape(&output.to_string_lossy()));
        let marked = format!(
            "printf marked > {}",
            shell_escape(&output.to_string_lossy())
        );
        let gate = gate_omp_launch(&raw, &marked, &omp_test_plan());
        let outer = shell_stdin_command("sh", false, &format!("exec env {gate}"), "AOE_TEST_OUTER");
        let script = temp.path().join("launch.sh");
        std::fs::write(&script, outer).unwrap();
        let status = std::process::Command::new("sh")
            .arg(&script)
            .env("PATH", test_path_with_shim(&bin))
            .env("TMUX_PANE", "%1")
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "marked");

        // A valid 70 KiB prompt makes the capture gate body larger than
        // Linux's per-argument exec limit because the raw and marked branches
        // both contain it. The launch must still execute from the descriptor.
        let payload = "x".repeat(70 * 1024);
        let large_command = format!(
            "printf '%s' {} > {}",
            shell_escape(&payload),
            shell_escape(&output.to_string_lossy())
        );
        let large_gate = gate_omp_launch(&large_command, &large_command, &omp_test_plan());
        let large_outer = wrap_command_ignore_suspend(&large_gate, temp.path().to_str().unwrap());
        assert!(!large_outer.lines().next().unwrap().contains("-c"));
        std::fs::write(&script, large_outer).unwrap();
        let status = std::process::Command::new("sh")
            .arg(&script)
            .env("PATH", test_path_with_shim(&bin))
            .env("TMUX_PANE", "%1")
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::metadata(output).unwrap().len(),
            payload.len() as u64
        );
    }

    #[cfg(unix)]
    #[test]
    fn omp_private_paths_reject_symlink_fifo_and_breadcrumb_races() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        let marker_link = dir.path().join("marker-link.tmp");
        let marker_fifo = dir.path().join("marker-fifo.tmp");
        std::fs::write(&victim, "unchanged").unwrap();
        std::os::unix::fs::symlink(&victim, &marker_link).unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(&marker_fifo)
            .status()
            .unwrap()
            .success());

        for collision in [&marker_link, &marker_fifo] {
            let output = std::process::Command::new("sh")
                .args(["-c", "(umask 077; mkdir \"$1\")", "sh"])
                .arg(collision)
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "private-dir creation must reject an existing path"
            );
        }

        let placeholder = dir.path().join("placeholder");
        let raced_file = dir.path().join("breadcrumb-file");
        let raced_link = dir.path().join("breadcrumb-link");
        std::fs::write(&placeholder, "cwd\nsentinel\nfresh\n").unwrap();
        std::fs::write(&raced_file, "winner").unwrap();
        std::os::unix::fs::symlink(&victim, &raced_link).unwrap();
        let raced_dir_link = dir.path().join("breadcrumb-dir-link");
        std::os::unix::fs::symlink(dir.path(), &raced_dir_link).unwrap();
        for collision in [&raced_file, &raced_link, &raced_dir_link] {
            let output = std::process::Command::new("sh")
                .args(["-c", "ln -n \"$1\" \"$2\"", "sh"])
                .arg(&placeholder)
                .arg(collision)
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "hardlink installation must not clobber a raced destination"
            );
        }
        assert_eq!(std::fs::read_to_string(raced_file).unwrap(), "winner");
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "unchanged");
    }
}

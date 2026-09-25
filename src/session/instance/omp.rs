//! Session-id capture for Oh My Pi (`omp`): resolving a capture plan,
//! gating the launch marker, and recording the launch generation.

use super::*;

/// Build a post-login routing fingerprint check without embedding any routing value in argv.
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

/// Wait briefly for the parent to publish this launch generation's hidden capture metadata.
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

/// Apply profile assignments to the marker wrapper itself, not only to its eventual OMP command.
pub(super) fn wrap_omp_host_launch(
    env_prefix: &str,
    tool_cmd: &str,
    plan: &OmpCapturePlan,
) -> String {
    format!("{env_prefix}{}", wrap_omp_launch(tool_cmd, plan))
}

/// Bind capture to the exact launch PTY. A valid pre-launch breadcrumb is
/// rewritten to a lexically different but equivalent session path while its
/// recognized extras are preserved. The marker records that pending path so
/// capture waits until OMP rewrites the breadcrumb. If no breadcrumb exists,
/// install a fresh sentinel from a private directory by a no-clobber hardlink.
/// Invalid breadcrumbs, collisions, symlinks, and write failures launch raw OMP
/// without capture.
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
           crumb_extra_1=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '3p') || launch_raw; \
           crumb_extra_2=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '4p') || launch_raw; \
           crumb_lines=$(head -c 16385 \"$breadcrumb\" 2>/dev/null | sed -n '$=') || launch_raw; \
           case \"$crumb_lines\" in 2|3|4) ;; *) launch_raw ;; esac; \
           [ -n \"$crumb_cwd\" ] && [ -n \"$crumb_path\" ] || launch_raw; \
           crumb_fresh=; crumb_cwdstat=; \
           validate_extra() {{ \
             case \"$1\" in \
               fresh) [ -z \"$crumb_fresh\" ] || launch_raw; crumb_fresh=1 ;; \
               'cwdstat '*) \
                 [ -z \"$crumb_cwdstat\" ] || launch_raw; \
                 cwdstat_values=${{1#cwdstat }}; \
                 cwdstat_dev=${{cwdstat_values%% *}}; \
                 cwdstat_ino=${{cwdstat_values#* }}; \
                 [ \"$cwdstat_ino\" != \"$cwdstat_values\" ] \
                   && [ -n \"$cwdstat_dev\" ] && [ -n \"$cwdstat_ino\" ] || launch_raw; \
                 case \"$cwdstat_dev$cwdstat_ino\" in *[!0-9]*) launch_raw ;; esac; \
                 crumb_cwdstat=1 ;; \
               *) launch_raw ;; \
             esac; \
           }}; \
           [ \"$crumb_lines\" -lt 3 ] || validate_extra \"$crumb_extra_1\"; \
           [ \"$crumb_lines\" -lt 4 ] || validate_extra \"$crumb_extra_2\"; \
           case \"$crumb_path\" in \
             /*) crumb_dir=${{crumb_path%/*}}; crumb_base=${{crumb_path##*/}}; \
                 [ -n \"$crumb_dir\" ] || crumb_dir=/; \
                 if [ \"$crumb_dir\" = / ]; then pending=\"/./$crumb_base\"; \
                 else pending=\"$crumb_dir/./$crumb_base\"; fi ;; \
             *) pending=\"./$crumb_path\" ;; \
           esac; \
           write_rewritten() {{ \
             printf '%s\\n%s\\n' \"$crumb_cwd\" \"$pending\" || return 1; \
             [ \"$crumb_lines\" -lt 3 ] || printf '%s\\n' \"$crumb_extra_1\" || return 1; \
             [ \"$crumb_lines\" -lt 4 ] || printf '%s\\n' \"$crumb_extra_2\" || return 1; \
           }}; \
           rewritten_bytes=$(write_rewritten | LC_ALL=C wc -c | tr -d '[:space:]'); \
           case \"$rewritten_bytes\" in ''|*[!0-9]*) rewritten_bytes=16385 ;; esac; \
           [ \"$rewritten_bytes\" -le 16384 ] || launch_raw; \
           breadcrumb_tmp_dir=\"$terminal_dir\"/{}.tmp.$$; \
           (umask 077; mkdir \"$breadcrumb_tmp_dir\") || launch_raw; \
           breadcrumb_tmp=\"$breadcrumb_tmp_dir/breadcrumb\"; \
           (umask 077; set -C; write_rewritten > \"$breadcrumb_tmp\") || launch_raw; \
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
    /// Capture is safe only for the built-in OMP command and a transparent, parseable argv.
    pub(super) fn omp_capture_options(&self) -> Option<OmpCliCaptureOptions> {
        if self.resolved_capture_backend() != Some(crate::agents::SessionCaptureBackend::Omp) {
            return None;
        }
        let args = crate::session::config::quote_model_value_in_args(&self.extra_args);
        OmpCliCaptureOptions::parse(&args).ok()
    }

    /// Instrument the already resolved launch without resolving routing again.
    pub(super) fn resolve_omp_capture_plan(
        &self,
        context: &crate::session::capture::OmpResolvedContext,
        container_runtime: Option<crate::session::config::ContainerRuntimeName>,
    ) -> Option<OmpCapturePlan> {
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
            layout: context.layout.clone(),
            routing_fingerprint: context.routing_fingerprint.clone(),
            launch_id: Uuid::new_v4().to_string(),
            launch_marker,
            container_runtime,
        })
    }

    /// Reconstruct metadata only for a legacy pane which predates launch snapshots.
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

    /// Load typed launch metadata directly from tmux. A pane carrying the regular bootstrap
    /// generation is modern.
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
        if self.resolved_capture_backend() != Some(crate::agents::SessionCaptureBackend::Omp) {
            return true;
        }
        // No capture plan: persist a distinct sentinel so any observation still carrying the prior
        // generation fails the CAS.
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
        if self.source_capture_backend() != Some(crate::agents::SessionCaptureBackend::Omp)
            || (self.is_sandboxed() && self.omp_capture_generation.is_none())
        {
            return;
        }
        let Some(observation) = self.try_retroactive_capture() else {
            return;
        };
        match persist_session_to_storage(
            profile,
            &self.id,
            &observation,
            &self.conversation_state(),
            &self.resolve_file_watch(),
        ) {
            SidWrite::Applied => {
                self.apply_conversation_observation(&observation);
                self.resume_probe_failed_sid = None;
            }
            // A pinned-foreign publication is a deliberate non-write; like a
            // divergence skip, it carries no update worth reconciling.
            SidWrite::Skipped | SidWrite::PinnedForeign => self.reconcile_from_disk(),
            SidWrite::Failed => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::capture::resolve_omp_store_layout_with_environment;
    use crate::session::instance::launch_command::wrap_command_ignore_suspend;
    use crate::session::instance::test_helpers::*;

    #[test]
    fn omp_capture_accepts_benign_args_and_rejects_opaque_launches() {
        let mut inst = tool_instance("omp", "/tmp/test");
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
    #[serial_test::serial]
    fn omp_alias_without_capture_plan_tombstones_generation() {
        const PROFILE: &str = "omp-alias-generation-test";
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::storage::Storage::new_unwatched(PROFILE).unwrap();
        let mut inst = Instance::new("alias", "/tmp/test");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "omp-alias".to_string();
        inst.detect_as = "omp".to_string();
        inst.command = "omp".to_string();
        let persisted = inst.clone();
        storage
            .update(|instances, groups| {
                *instances = vec![persisted.clone()];
                *groups =
                    crate::session::GroupTree::new_with_groups(instances, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        assert_eq!(
            inst.resolved_capture_backend(),
            Some(crate::agents::SessionCaptureBackend::Omp)
        );
        assert!(inst.publish_omp_launch_generation(PROFILE, None, None));
        let generation = inst
            .omp_capture_generation
            .as_deref()
            .expect("OMP alias must publish a tombstone generation");
        assert!(generation.starts_with("tombstone-"));
        assert_eq!(
            storage
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == inst.id)
                .and_then(|row| row.omp_capture_generation),
            Some(generation.to_string())
        );
    }

    #[test]
    fn omp_launch_rejects_api_keys_in_extra_args() {
        let mut instance = tool_instance("omp", "/tmp/test");
        for extra_args in [
            "--api-key secret",
            "--api-key=secret",
            "--api-key$EMPTY secret",
        ] {
            instance.extra_args = extra_args.to_string();
            let error = instance
                .build_launch_command(None)
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

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn omp_routing_fingerprint_accepts_matching_live_env_and_rejects_drift() {
        // The live side reads host routing env; the expected fingerprint sees only HOME.
        let _env = crate::session::test_support::EnvGuard::unset(
            &crate::session::capture::OMP_STORE_ENV_KEYS,
        );
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let context = resolve_omp_store_layout_with_environment(
            std::collections::HashMap::from([("HOME".into(), home.display().to_string())]),
            project.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        let mut plan = omp_test_plan();
        plan.routing_fingerprint = context.routing_fingerprint.clone();
        let check = omp_routing_fingerprint_check(&plan);
        let script = format!("launch_raw() {{ printf raw; exit 0; }}; {check}printf captured");
        let run = |live_home: &Path| {
            let mut command = std::process::Command::new("sh");
            command
                .args(["-c", &script])
                .env_clear()
                // `env_clear` is here to control which OMP_STORE_ENV_KEYS the fingerprint folds in,
                // not to pin a filesystem layout.
                .env("PATH", std::env::var_os("PATH").unwrap_or_default());
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

    #[cfg(unix)]
    fn exercise_omp_wrapper(collision: Option<(&str, &str)>) {
        use crate::tmux::test_helpers::{only_pane_id, pane_field, TmuxTestSession};
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::session::test_support::EnvGuard::unset(
            &crate::session::capture::OMP_STORE_ENV_KEYS,
        );
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = dir.path().canonicalize().unwrap();
        let root = canonical_root.as_path();
        let home = root.join("home");
        let bin = root.join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let context = resolve_omp_store_layout_with_environment(
            std::collections::HashMap::from([("HOME".into(), home.display().to_string())]),
            root.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        std::fs::create_dir_all(&context.layout.terminal_sessions).unwrap();
        let plan = OmpCapturePlan {
            layout: context.layout.clone(),
            routing_fingerprint: context.routing_fingerprint.clone(),
            launch_id: "native-wrapper-test".to_string(),
            launch_marker: root.join("marker").to_string_lossy().into_owned(),
            container_runtime: None,
        };
        let victim = root.join("victim");
        let attempted = root.join("collision-attempted");
        std::fs::write(&victim, "unchanged").unwrap();
        if let Some((stage, kind)) = collision {
            let command = if stage == "breadcrumb" { "ln" } else { "mkdir" };
            let real = which::which(command).unwrap();
            let ln = which::which("ln").unwrap();
            let mkfifo = which::which("mkfifo").unwrap();
            let target = if stage == "breadcrumb" { "$3" } else { "$1" };
            let predicate = if stage == "marker" {
                "case \"$1\" in */marker.tmp.*)"
            } else {
                "case \"$1\" in *)"
            };
            let create = match kind {
                "symlink" => format!(
                    "{} -s {} \"{target}\"",
                    shell_escape(&ln.to_string_lossy()),
                    shell_escape(&victim.to_string_lossy())
                ),
                "directory-symlink" => format!(
                    "{} -s {} \"{target}\"",
                    shell_escape(&ln.to_string_lossy()),
                    shell_escape(&root.to_string_lossy())
                ),
                "fifo" => format!("{} \"{target}\"", shell_escape(&mkfifo.to_string_lossy())),
                "file" => format!("printf winner > \"{target}\""),
                _ => panic!("unknown collision"),
            };
            let script = format!("#!/bin/sh\n{predicate} {create}; printf '%s' \"{target}\" > {} ;; esac\nexec {} \"$@\"\n", shell_escape(&attempted.to_string_lossy()), shell_escape(&real.to_string_lossy()));
            let shim = bin.join(command);
            std::fs::write(&shim, script).unwrap();
            std::fs::set_permissions(shim, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let output = root.join("launched");
        let raw = format!(
            "printf launched > {}; exec sleep 30",
            shell_escape(&output.to_string_lossy())
        );
        let wrapped = wrap_omp_launch(&raw, &plan);
        let mut env = format!(
            "env -i PATH={} ",
            shell_escape(&test_path_with_shim(&bin).to_string_lossy())
        );
        for (key, value) in &context.launcher_routing {
            let Some(value) = value else { continue };
            env.push_str(&shell_escape(&format!("{key}={value}")));
            env.push(' ');
        }
        let script = root.join("launch.sh");
        std::fs::write(&script, format!("exec {env}{wrapped}")).unwrap();
        let session = TmuxTestSession::new("omp_wrapper");
        let command = format!("sh {}", shell_escape(&script.to_string_lossy()));
        let result = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                session.name(),
                "-c",
                root.to_str().unwrap(),
                &command,
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let pane = only_pane_id(session.name());
        let terminal_id = pane_field(&pane, "#{pane_tty}")
            .strip_prefix("/dev/")
            .unwrap()
            .replace('/', "-");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match std::fs::read_to_string(&output) {
                Ok(content) if content == "launched" => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("cannot read launch output: {error}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "wrapper did not execute the agent command"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "unchanged");
        if let Some((_, kind)) = collision {
            let target =
                std::fs::read_to_string(&attempted).expect("wrapper reached the hostile operation");
            assert!(
                !std::path::Path::new(&plan.launch_marker).exists(),
                "collision must launch raw without publishing a marker"
            );
            if kind == "file" {
                assert_eq!(std::fs::read_to_string(target).unwrap(), "winner");
            }
        } else {
            let marker = std::fs::read_to_string(&plan.launch_marker).unwrap();
            let fields: Vec<_> = marker.lines().collect();
            assert_eq!(fields.len(), 4);
            assert_eq!(fields[0], terminal_id);
            assert_eq!(fields[1], plan.launch_id);
            assert_eq!(fields[3], plan.routing_fingerprint);
            let breadcrumb =
                std::fs::read_to_string(plan.layout.terminal_sessions.join(terminal_id)).unwrap();
            let crumb: Vec<_> = breadcrumb.lines().collect();
            assert_eq!(crumb, [root.to_str().unwrap(), fields[2], "fresh"]);
            assert!(fields[2].ends_with("aoe-pending_native-wrapper-test.jsonl"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn omp_launch_wrapper_hashes_live_routing_and_marker_is_noclobber() {
        exercise_omp_wrapper(None);
    }

    /// The shim dir, then the caller's `PATH`. Shim first, so the fake `tmux` wins over any real
    /// one.
    #[cfg(unix)]
    fn test_path_with_shim(bin: &std::path::Path) -> std::ffi::OsString {
        // An unset or empty PATH is handled separately.
        let Some(inherited) = std::env::var_os("PATH").filter(|p| !p.is_empty()) else {
            return bin.as_os_str().to_os_string();
        };
        let entries = std::iter::once(bin.to_path_buf())
            .chain(std::env::split_paths(&inherited))
            .collect::<Vec<_>>();
        std::env::join_paths(entries).expect("PATH entries contain no separator")
    }

    #[cfg(unix)]
    #[test]
    fn omp_launch_wrapper_preserves_known_breadcrumb_extras_and_rejects_invalid_ones() {
        run_omp_launch_wrapper_test_in_child();
    }

    #[cfg(unix)]
    #[test]
    #[ignore]
    fn omp_launch_wrapper_preserves_known_breadcrumb_extras_and_rejects_invalid_ones_child() {
        println!("OMP_WRAPPER_TEST_CHILD_ENTERED");
        exercise_omp_launch_wrapper();
    }

    #[cfg(unix)]
    fn run_omp_launch_wrapper_test_in_child() {
        let current_thread = std::thread::current();
        let test = current_thread.name().expect("test thread name");
        let child_test = format!("{test}_child");
        let home = tempfile::tempdir().unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                child_test.as_str(),
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .env("breadcrumb_tmp", "preexisting")
            .env("marker_tmp", "preexisting")
            .env("write_count", "1")
            .stdin(std::process::Stdio::null());
        let output = crate::process::run_with_timeout_process_group(
            &mut command,
            std::time::Duration::from_secs(60),
        )
        .expect("spawn isolated OMP wrapper test")
        .expect("isolated OMP wrapper test watchdog expired");
        assert!(
            output.status.success(),
            "isolated OMP wrapper test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout
                .lines()
                .chain(stderr.lines())
                .any(|line| line.contains("OMP_WRAPPER_TEST_CHILD_ENTERED")),
            "isolated OMP wrapper child did not acknowledge entry:\n{}\n{}",
            stdout,
            stderr
        );
    }

    #[cfg(unix)]
    fn exercise_omp_launch_wrapper() {
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::session::test_support::EnvGuard::unset(
            &crate::session::capture::OMP_STORE_ENV_KEYS,
        );
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        let bin = root.join("bin");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let tty = bin.join("tty");
        std::fs::write(&tty, "#!/bin/sh\nprintf '/dev/pts/omp-extra-test\\n'\n").unwrap();
        std::fs::set_permissions(&tty, std::fs::Permissions::from_mode(0o700)).unwrap();
        let real_sh = which::which("sh").unwrap();
        let sh = bin.join("sh");
        // Fail a selected breadcrumb write: only it runs after `breadcrumb_tmp` is set and
        // before `marker_tmp` is. `-ef` on /dev/fd cannot match the file on macOS.
        let injected_shell = r#"printf() {
  if [ -n "${AOE_TEST_FAIL_WRITE-}" ] && [ -n "${breadcrumb_tmp-}" ] \
    && [ -z "${marker_tmp-}" ]; then
    write_count=$(( ${write_count:-0} + 1 ))
    command printf '%s\n%s\n' "$breadcrumb_tmp" "$write_count" >> "$AOE_TEST_WRITE_TRACE"
    if [ "$write_count" -eq "$AOE_TEST_FAIL_WRITE" ]; then
      command printf partial
      command printf injected > "$AOE_TEST_WRITE_FAILURE"
      command printf '%s' "$@" >&-
      return $?
    fi
  fi
  command printf "$@"
}
. /dev/fd/3"#;
        let injected_script = root.join("inject-write-failure.sh");
        std::fs::write(&injected_script, injected_shell).unwrap();
        std::fs::write(
            &sh,
            format!(
                "#!/bin/sh\nif [ \"$1\" = /dev/fd/3 ]; then exec {} {}; fi\nexec {} \"$@\"\n",
                shell_escape(&real_sh.to_string_lossy()),
                shell_escape(&injected_script.to_string_lossy()),
                shell_escape(&real_sh.to_string_lossy()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o700)).unwrap();

        let routing = vec![format!("HOME={}", home.display())];
        let context = resolve_omp_store_layout_with_environment(
            std::collections::HashMap::from([("HOME".into(), home.display().to_string())]),
            root.to_str().unwrap(),
            &OmpCliCaptureOptions::default(),
        )
        .unwrap();
        std::fs::create_dir_all(&context.layout.terminal_sessions).unwrap();
        let plan = OmpCapturePlan {
            layout: context.layout,
            routing_fingerprint: context.routing_fingerprint,
            launch_id: "wrapper-extra-test".to_string(),
            launch_marker: root.join("marker").to_string_lossy().into_owned(),
            container_runtime: None,
        };
        let breadcrumb = plan.layout.terminal_sessions.join("pts-omp-extra-test");
        let launched = root.join("launched");
        let write_failure = root.join("write-failure");
        let raw = format!(
            "printf launched > {}",
            shell_escape(&launched.to_string_lossy())
        );
        let write_trace = root.join("write-trace");
        let cases = [
            ("/work\n/session.jsonl\n", true),
            ("/work\n/session.jsonl\nfresh\n", true),
            ("/work\n/session.jsonl\ncwdstat 12 34\n", true),
            ("/work\n/session.jsonl\nfresh\ncwdstat 12 34\n", true),
            ("/work\n/session.jsonl\ncwdstat 12 34\nfresh\n", true),
            ("/work\n/session.jsonl\nunknown\n", false),
            ("/work\n/session.jsonl\ncwdstat 12\n", false),
            ("/work\n/session.jsonl\ncwdstat 12 34 56\n", false),
            ("/work\n/session.jsonl\nfresh\nfresh\n", false),
            (
                "/work\n/session.jsonl\ncwdstat 12 34\ncwdstat 12 34\n",
                false,
            ),
        ];

        let failed_writes = [
            ("/work\n/session.jsonl\n", false, Some(1)),
            ("/work\n/session.jsonl\nfresh\n", false, Some(2)),
            ("/work\n/session.jsonl\ncwdstat 12 34\n", false, Some(2)),
            (
                "/work\n/session.jsonl\nfresh\ncwdstat 12 34\n",
                false,
                Some(1),
            ),
            (
                "/work\n/session.jsonl\nfresh\ncwdstat 12 34\n",
                false,
                Some(2),
            ),
            (
                "/work\n/session.jsonl\nfresh\ncwdstat 12 34\n",
                false,
                Some(3),
            ),
        ];
        for (content, accepted, failed_write) in cases
            .into_iter()
            .map(|(content, accepted)| (content, accepted, None))
            .chain(failed_writes)
        {
            let _ = std::fs::remove_file(&plan.launch_marker);
            let _ = std::fs::remove_file(&launched);
            let _ = std::fs::remove_file(&write_failure);
            std::fs::write(&breadcrumb, content).unwrap();
            let _ = std::fs::remove_file(&write_trace);
            let wrapped = wrap_omp_launch(&raw, &plan);
            let mut command = std::process::Command::new("sh");
            command
                .arg("-c")
                .arg(wrapped)
                .env("PATH", test_path_with_shim(&bin))
                .env_remove("breadcrumb_tmp")
                .env_remove("marker_tmp")
                .env_remove("write_count")
                .env_remove("AOE_TEST_FAIL_WRITE")
                .stdin(std::process::Stdio::null());
            if let Some(index) = failed_write {
                command
                    .env("AOE_TEST_FAIL_WRITE", index.to_string())
                    .env("AOE_TEST_WRITE_FAILURE", &write_failure)
                    .env("AOE_TEST_WRITE_TRACE", &write_trace);
            }
            for mutation in omp_host_routing_environment(&routing) {
                match mutation {
                    tmux::PaneEnvMutation::Set { key, value } => {
                        command.env(key, value);
                    }
                    tmux::PaneEnvMutation::Unset { key } => {
                        command.env_remove(key);
                    }
                }
            }
            let output = crate::process::run_with_timeout_process_group(
                &mut command,
                std::time::Duration::from_secs(10),
            )
            .expect("run OMP wrapper shell")
            .expect("OMP wrapper shell watchdog expired");
            assert!(
                output.status.success(),
                "{content:?}\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read_to_string(&launched).unwrap(), "launched");
            if let Some(index) = failed_write {
                let expected_temp_prefix = format!(".aoe-omp-breadcrumb-{}.tmp.", plan.launch_id);
                let trace = std::fs::read_to_string(&write_trace).unwrap();
                let mut injected_breadcrumb = None;
                let mut trace_fields = trace.lines();
                for expected_count in 1..=index {
                    let breadcrumb_tmp = trace_fields.next().unwrap();
                    let count = trace_fields.next().unwrap();
                    let breadcrumb_path = std::path::Path::new(breadcrumb_tmp);
                    let mut components = breadcrumb_path
                        .strip_prefix(plan.layout.terminal_sessions.as_path())
                        .unwrap_or_else(|_| {
                            panic!(
                                "injected breadcrumb escaped terminal sessions: {breadcrumb_tmp:?}"
                            )
                        })
                        .components();
                    let temp_dir = components
                        .next()
                        .and_then(|component| match component {
                            std::path::Component::Normal(name) => name.to_str(),
                            _ => None,
                        })
                        .unwrap_or_else(|| {
                            panic!("injected breadcrumb has no normal temp directory: {breadcrumb_tmp:?}")
                        });
                    let pid = temp_dir
                        .strip_prefix(&expected_temp_prefix)
                        .unwrap_or_else(|| {
                            panic!(
                                "injected breadcrumb has unexpected temp directory: {temp_dir:?}"
                            )
                        });
                    assert!(
                        !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()),
                        "injected breadcrumb has invalid temp PID: {pid:?}"
                    );
                    let file = components.next().and_then(|component| match component {
                        std::path::Component::Normal(name) => name.to_str(),
                        _ => None,
                    });
                    assert_eq!(
                        file,
                        Some("breadcrumb"),
                        "unexpected injected breadcrumb file"
                    );
                    assert!(
                        components.next().is_none(),
                        "injected breadcrumb escaped its temp child"
                    );
                    assert_eq!(count, expected_count.to_string());
                    if expected_count == index {
                        injected_breadcrumb = Some(breadcrumb_tmp);
                    }
                }
                assert_eq!(trace_fields.next(), None);
                let injected_breadcrumb = injected_breadcrumb.expect("injected breadcrumb path");
                let partial =
                    std::fs::read_to_string(injected_breadcrumb).unwrap_or_else(|error| {
                        panic!(
                            "injected breadcrumb was not opened: {injected_breadcrumb:?}: {error}"
                        )
                    });
                assert!(
                    partial.ends_with("partial"),
                    "injected write did not produce partial breadcrumb output: {partial:?}"
                );
                assert_eq!(std::fs::read_to_string(&write_failure).unwrap(), "injected");
            } else {
                assert!(
                    !write_failure.exists(),
                    "unexpected injected write for {content:?}"
                );
                assert!(!write_trace.exists());
            }

            if accepted {
                let marker = std::fs::read_to_string(&plan.launch_marker)
                    .unwrap_or_else(|error| panic!("{content:?}: {error}"));
                let marker_fields: Vec<_> = marker.lines().collect();
                assert_eq!(marker_fields.len(), 4, "{content:?}");
                let rewritten = std::fs::read_to_string(&breadcrumb).unwrap();
                let rewritten_fields: Vec<_> = rewritten.lines().collect();
                assert_eq!(rewritten_fields[0], "/work", "{content:?}");
                assert_eq!(rewritten_fields[1], marker_fields[2], "{content:?}");
                assert_ne!(rewritten_fields[1], "/session.jsonl", "{content:?}");
                assert_eq!(
                    &rewritten_fields[2..],
                    &content.lines().collect::<Vec<_>>()[2..],
                    "{content:?}"
                );
            } else {
                assert!(
                    !std::path::Path::new(&plan.launch_marker).exists(),
                    "rejected breadcrumb published a marker: {content:?}, write {failed_write:?}"
                );
                assert_eq!(std::fs::read_to_string(&breadcrumb).unwrap(), content);
            }
        }
    }

    /// Holds `ENV_LOCK` across the `PATH` read that builds the child's.
    #[cfg(unix)]
    #[test]
    fn omp_capture_gate_executes_nested_stdin_scripts() {
        use std::os::unix::fs::PermissionsExt;

        let _env = crate::session::test_support::EnvGuard::read_lock();
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

        // A valid 70 KiB prompt makes the capture gate body larger than Linux's per-argument exec
        // limit because the raw and marked branches both contain it.
        let payload = "x".repeat(70 * 1024);
        let large_command = format!(
            "printf '%s' {} > {}",
            shell_escape(&payload),
            shell_escape(&output.to_string_lossy())
        );
        let large_gate = gate_omp_launch(&large_command, &large_command, &omp_test_plan());
        let large_outer =
            wrap_command_ignore_suspend(&large_gate, temp.path().to_str().unwrap(), &[], &[]);
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
        for collision in [
            ("marker", "symlink"),
            ("marker", "fifo"),
            ("breadcrumb", "file"),
            ("breadcrumb", "symlink"),
            ("breadcrumb", "directory-symlink"),
        ] {
            exercise_omp_wrapper(Some(collision));
        }
    }
}

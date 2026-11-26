//! Oh My Pi (`omp`) launch instrumentation: resolving a capture plan, gating
//! the launch marker, and recording the launch generation.

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
    /// Capture is safe only for the built-in OMP command and a transparent,
    /// parseable argv. Benign arguments remain supported; store-selecting
    /// flags are interpreted by the capture resolver.
    #[cfg(test)]
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

    /// Publish the capture plan's generation, or mint a tombstone generation
    /// for an OMP launch whose capture plan could not be resolved.
    pub(super) fn publish_omp_launch_generation_with_store(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        metadata: Option<&OmpCaptureMetadata>,
        expected_prior: Option<&str>,
    ) -> Result<bool> {
        if let Some(metadata) = metadata {
            return self.persist_omp_capture_generation(
                storage,
                &metadata.launch_id,
                expected_prior,
            );
        }
        if self.resolved_capture_backend() != Some(crate::agents::SessionCaptureBackend::Omp) {
            return Ok(true);
        }
        // A tombstone rejects observations carrying the previous generation.
        let tombstone = format!("tombstone-{}", Uuid::new_v4());
        self.persist_omp_capture_generation(storage, &tombstone, expected_prior)
    }

    #[cfg(test)]
    pub(super) fn publish_omp_launch_generation(
        &mut self,
        profile: &str,
        metadata: Option<&OmpCaptureMetadata>,
        expected_prior: Option<&str>,
    ) -> Result<bool> {
        let storage = crate::session::storage::Storage::new_unwatched(profile)?;
        self.publish_omp_launch_generation_with_store(&storage, metadata, expected_prior)
    }

    /// CAS-persist one OMP capture generation and reload the durable winner
    /// when another writer has already advanced it.
    fn persist_omp_capture_generation(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: &str,
        expected_prior: Option<&str>,
    ) -> Result<bool> {
        let applied = storage.update(|instances, _| {
            let instance = instances
                .iter_mut()
                .find(|row| row.id == self.id)
                .ok_or(LifecycleReservationError::Superseded)?;
            if instance.omp_capture_generation.as_deref() != expected_prior {
                return Ok(false);
            }
            instance.omp_capture_generation = Some(generation.to_owned());
            Ok(true)
        })?;
        if applied {
            self.omp_capture_generation = Some(generation.to_owned());
            return Ok(true);
        }
        let winner = storage
            .load()?
            .into_iter()
            .find(|row| row.id == self.id)
            .ok_or(LifecycleReservationError::Superseded)?;
        self.omp_capture_generation = winner.omp_capture_generation;
        tracing::warn!(
            target: "session.store",
            instance = %self.id,
            generation,
            "OMP generation CAS failed; launch continues with capture disabled"
        );
        Ok(false)
    }
}

//! Hermes 0.21 native state namespaces; positive mixed readers keep marker identity.

#[derive(Clone, Copy, Debug)]
pub(super) struct StateSpec {
    pub(super) pattern: &'static str,
    pub(super) exact: Option<&'static str>,
}

pub(super) const WORKSPACE_MARKER: &str = "workspace";
pub(super) const PROJECTS_MARKER: &str = "projects.db*";
pub(super) const SKILLS_USAGE_MARKER: &str = "skills/.usage.json";
pub(super) const SKILLS_CURATOR_MARKER: &str = "skills/.curator_state";

// H-relative native boundaries. Restore entries name only known final members.
pub(super) const HOME_STATE: &[StateSpec] = &[
    StateSpec {
        pattern: "sessions",
        exact: None,
    },
    StateSpec {
        pattern: "logs",
        exact: None,
    },
    StateSpec {
        pattern: "cache",
        exact: None,
    },
    StateSpec {
        pattern: "pastes",
        exact: None,
    },
    StateSpec {
        pattern: "images",
        exact: None,
    },
    StateSpec {
        pattern: "chrome-debug",
        exact: None,
    },
    StateSpec {
        pattern: "tmp",
        exact: None,
    },
    StateSpec {
        pattern: "state.db*",
        exact: None,
    },
    StateSpec {
        pattern: "memories",
        exact: None,
    },
    StateSpec {
        pattern: "workspace",
        exact: None,
    },
    StateSpec {
        pattern: "home",
        exact: None,
    },
    StateSpec {
        pattern: "cron",
        exact: None,
    },
    StateSpec {
        pattern: "backups",
        exact: None,
    },
    StateSpec {
        pattern: "state-snapshots",
        exact: None,
    },
    StateSpec {
        pattern: "checkpoints",
        exact: None,
    },
    StateSpec {
        pattern: "plans",
        exact: None,
    },
    StateSpec {
        pattern: "hermes_state.db*",
        exact: None,
    },
    StateSpec {
        pattern: "response_store.db*",
        exact: None,
    },
    StateSpec {
        pattern: "gateway.pid",
        exact: None,
    },
    StateSpec {
        pattern: "gateway_state.json",
        exact: None,
    },
    StateSpec {
        pattern: "processes.json",
        exact: None,
    },
    StateSpec {
        pattern: "auth.lock",
        exact: None,
    },
    StateSpec {
        pattern: ".update_check",
        exact: None,
    },
    StateSpec {
        pattern: "errors.log",
        exact: None,
    },
    StateSpec {
        pattern: ".hermes_history",
        exact: None,
    },
    StateSpec {
        pattern: "image_cache",
        exact: None,
    },
    StateSpec {
        pattern: "audio_cache",
        exact: None,
    },
    StateSpec {
        pattern: "document_cache",
        exact: None,
    },
    StateSpec {
        pattern: "browser_screenshots",
        exact: None,
    },
    StateSpec {
        pattern: "sandboxes",
        exact: None,
    },
    StateSpec {
        pattern: "MEMORY.md",
        exact: None,
    },
    StateSpec {
        pattern: "USER.md",
        exact: None,
    },
    StateSpec {
        pattern: "todo.json",
        exact: None,
    },
    StateSpec {
        pattern: "verification_evidence.db*",
        exact: None,
    },
    StateSpec {
        pattern: ".curator_backups",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.curator_backups",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.curator_ledger.jsonl",
        exact: None,
    },
    StateSpec {
        pattern: "pending_messages",
        exact: None,
    },
    StateSpec {
        pattern: "moa-traces",
        exact: None,
    },
    StateSpec {
        pattern: "spawn-trees",
        exact: None,
    },
    StateSpec {
        pattern: "session-exports",
        exact: None,
    },
    StateSpec {
        pattern: "terminal-sessions",
        exact: None,
    },
    StateSpec {
        pattern: "hook_outputs",
        exact: None,
    },
    StateSpec {
        pattern: "pending",
        exact: None,
    },
    StateSpec {
        pattern: "memory_store.db*",
        exact: None,
    },
    StateSpec {
        pattern: "plugin-data",
        exact: None,
    },
    StateSpec {
        pattern: "plugins/hermes-achievements/state.json",
        exact: None,
    },
    StateSpec {
        pattern: "plugins/hermes-achievements/scan_snapshot.json",
        exact: None,
    },
    StateSpec {
        pattern: "plugins/hermes-achievements/scan_checkpoint.json",
        exact: None,
    },
    StateSpec {
        pattern: "runs_idempotency.db*",
        exact: None,
    },
    StateSpec {
        pattern: "disk-cleanup",
        exact: None,
    },
    StateSpec {
        pattern: "telemetry/shared_metrics",
        exact: None,
    },
    StateSpec {
        pattern: "byterover",
        exact: None,
    },
    StateSpec {
        pattern: "openviking/pending_sessions",
        exact: None,
    },
    StateSpec {
        pattern: "openviking/runs",
        exact: None,
    },
    StateSpec {
        pattern: "gateway",
        exact: None,
    },
    StateSpec {
        pattern: "state",
        exact: None,
    },
    StateSpec {
        pattern: "runtime/photon-sidecar.json",
        exact: None,
    },
    StateSpec {
        pattern: "rate_limits/nous.json",
        exact: None,
    },
    StateSpec {
        pattern: "buzz/channel-cursors.json",
        exact: None,
    },
    StateSpec {
        pattern: "channel_directory.json",
        exact: None,
    },
    StateSpec {
        pattern: "*_threads.json",
        exact: None,
    },
    StateSpec {
        pattern: "feishu_seen_message_ids.json",
        exact: None,
    },
    StateSpec {
        pattern: "google_chat_thread_counts.json",
        exact: None,
    },
    StateSpec {
        pattern: "video_cache",
        exact: None,
    },
    StateSpec {
        pattern: "browser_recordings",
        exact: None,
    },
    StateSpec {
        pattern: "artifacts/browser-control",
        exact: None,
    },
    StateSpec {
        pattern: "browser-profile",
        exact: None,
    },
    StateSpec {
        pattern: "modal_snapshots.json",
        exact: None,
    },
    StateSpec {
        pattern: "singularity_snapshots.json",
        exact: None,
    },
    StateSpec {
        pattern: "vercel_sandbox_snapshots.json",
        exact: None,
    },
    StateSpec {
        pattern: ".skills_prompt_snapshot.json",
        exact: None,
    },
    StateSpec {
        pattern: "context_length_cache.yaml",
        exact: None,
    },
    StateSpec {
        pattern: "models_dev_cache.json",
        exact: None,
    },
    StateSpec {
        pattern: "models_dev_cache.etag",
        exact: None,
    },
    StateSpec {
        pattern: "provider_models_cache.json",
        exact: None,
    },
    StateSpec {
        pattern: "ollama_cloud_models_cache.json",
        exact: None,
    },
    StateSpec {
        pattern: "sticker_cache.json",
        exact: None,
    },
    StateSpec {
        pattern: "bootstrap-cache",
        exact: None,
    },
    StateSpec {
        pattern: "interrupt_debug.log",
        exact: None,
    },
    StateSpec {
        pattern: "gateway-starts.log",
        exact: None,
    },
    StateSpec {
        pattern: "perf.log",
        exact: None,
    },
    StateSpec {
        pattern: "gateway.lock",
        exact: None,
    },
    StateSpec {
        pattern: "gateway.sock",
        exact: None,
    },
    StateSpec {
        pattern: "gateway.sock.path",
        exact: None,
    },
    StateSpec {
        pattern: ".gateway-takeover.json",
        exact: None,
    },
    StateSpec {
        pattern: ".gateway-planned-stop.json",
        exact: None,
    },
    StateSpec {
        pattern: ".restart_notify.json",
        exact: None,
    },
    StateSpec {
        pattern: ".restart_pending.json",
        exact: None,
    },
    StateSpec {
        pattern: ".restart_last_processed.json",
        exact: None,
    },
    StateSpec {
        pattern: ".clean_shutdown",
        exact: None,
    },
    StateSpec {
        pattern: ".update_pending.json",
        exact: None,
    },
    StateSpec {
        pattern: ".update_pending.claimed.json",
        exact: None,
    },
    StateSpec {
        pattern: ".update_output.txt",
        exact: None,
    },
    StateSpec {
        pattern: ".update_exit_code",
        exact: None,
    },
    StateSpec {
        pattern: ".update_prompt.json",
        exact: None,
    },
    StateSpec {
        pattern: ".update_response",
        exact: None,
    },
    StateSpec {
        pattern: "fleet_restart_pending",
        exact: None,
    },
    StateSpec {
        pattern: ".hermes-update-in-progress",
        exact: None,
    },
    StateSpec {
        pattern: ".backup.lock",
        exact: None,
    },
    StateSpec {
        pattern: ".mcp-discovery.lock",
        exact: None,
    },
    StateSpec {
        pattern: ".sync.lock",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.curator_state",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.usage.json",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.usage.json.lock",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.usage_????????.tmp",
        exact: Some(r"^skills/\.usage_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "skills/.bundled_manifest",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.bundled_manifest_????????.tmp",
        exact: Some(r"^skills/\.bundled_manifest_[a-z0-9_]{8}\.tmp$"),
    },
    // The hub's own bookkeeping (installed paths, content hashes, scan
    // verdicts, lock file) is host state, not an authored skill.
    StateSpec {
        pattern: "skills/.hub",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.hub/audit.log",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.hub/quarantine",
        exact: None,
    },
    StateSpec {
        pattern: "skills/.hub/index-cache",
        exact: None,
    },
    StateSpec {
        pattern: ".channel_directory_????????.tmp",
        exact: Some(r"^\.channel_directory_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: ".gateway_state_????????.tmp",
        exact: Some(r"^\.gateway_state_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: ".processes_????????.tmp",
        exact: Some(r"^\.processes_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "..gateway-takeover_????????.tmp",
        exact: Some(r"^\.\.gateway\-takeover_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "..gateway-planned-stop_????????.tmp",
        exact: Some(r"^\.\.gateway\-planned\-stop_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "..restart_pending_????????.tmp",
        exact: Some(r"^\.\.restart_pending_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: ".*_threads_????????.tmp",
        exact: Some(r"^\.[^/]*_threads_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: ".feishu_seen_message_ids_????????.tmp",
        exact: Some(r"^\.feishu_seen_message_ids_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "..skills_prompt_snapshot_????????.tmp",
        exact: Some(r"^\.\.skills_prompt_snapshot_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: ".models_dev_cache_????????.tmp",
        exact: Some(r"^\.models_dev_cache_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: ".provider_models_cache_????????.tmp",
        exact: Some(r"^\.provider_models_cache_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: ".ollama_cloud_models_cache_????????.tmp",
        exact: Some(r"^\.ollama_cloud_models_cache_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: ".context_length_cache_????????.tmp",
        exact: Some(r"^\.context_length_cache_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "tmp????????.tmp",
        exact: Some(r"^tmp[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "google_chat_thread_counts.json.tmp",
        exact: None,
    },
    StateSpec {
        pattern: "gateway-starts.tmp",
        exact: None,
    },
    StateSpec {
        pattern: ".update_prompt.tmp",
        exact: None,
    },
    StateSpec {
        pattern: ".update_response.tmp",
        exact: None,
    },
    StateSpec {
        pattern: "runtime/.photon-sidecar.????????.tmp",
        exact: Some(r"^runtime/\.photon\-sidecar\.[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "rate_limits/tmp????????.tmp",
        exact: Some(r"^rate_limits/tmp[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "buzz/.channel-cursors_????????.tmp",
        exact: Some(r"^buzz/\.channel\-cursors_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "pairing/*-pending.json",
        exact: None,
    },
    StateSpec {
        pattern: "pairing/_rate_limits.json",
        exact: None,
    },
    StateSpec {
        pattern: "pairing/tmp????????.tmp",
        exact: Some(r"^pairing/tmp[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "platforms/pairing/*-pending.json",
        exact: None,
    },
    StateSpec {
        pattern: "platforms/pairing/_rate_limits.json",
        exact: None,
    },
    StateSpec {
        pattern: "platforms/pairing/tmp????????.tmp",
        exact: Some(r"^platforms/pairing/tmp[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "models_dev_cache.json.corrupt",
        exact: None,
    },
    StateSpec {
        pattern: ".restart_failure_counts",
        exact: None,
    },
    StateSpec {
        pattern: "..restart_failure_counts_????????.tmp",
        exact: Some(r"^\.\.restart_failure_counts_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: ".drain_request.json",
        exact: None,
    },
    StateSpec {
        pattern: "..drain_request_????????.tmp",
        exact: Some(r"^\.\.drain_request_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "teams_pipeline_store.json",
        exact: None,
    },
    StateSpec {
        pattern: "tmp????????",
        exact: Some(r"^tmp[a-z0-9_]{8}$"),
    },
    StateSpec {
        pattern: "weixin/accounts/*.context-tokens.json",
        exact: None,
    },
    StateSpec {
        pattern: "weixin/accounts/.*.context-tokens_????????.tmp",
        exact: Some(r"^weixin/accounts/\.[^/]*\.context\-tokens_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "weixin/accounts/*.sync.json",
        exact: None,
    },
    StateSpec {
        pattern: "weixin/accounts/.*.sync_????????.tmp",
        exact: Some(r"^weixin/accounts/\.[^/]*\.sync_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "skills/..curator_state_????????.tmp",
        exact: Some(r"^skills/\.\.curator_state_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "desktop/interrupted_turns.json",
        exact: None,
    },
    StateSpec {
        pattern: "desktop/.turn-marker-????????",
        exact: Some(r"^desktop/\.turn\-marker\-[a-z0-9_]{8}$"),
    },
    StateSpec {
        pattern: "a2a_conversations",
        exact: None,
    },
    StateSpec {
        pattern: "a2a_audit.jsonl",
        exact: None,
    },
    StateSpec {
        pattern: "runtime/active_sessions.json",
        exact: None,
    },
    StateSpec {
        pattern: "runtime/active_sessions.lock",
        exact: None,
    },
    StateSpec {
        pattern: "runtime/active_sessions.json.*.*.tmp",
        exact: Some(r"^runtime/active_sessions\.json\.[0-9]+\.[0-9A-Fa-f]{32}\.tmp$"),
    },
    StateSpec {
        pattern: ".tmp_????????.tmp",
        exact: Some(r"^\.tmp_[a-z0-9_]{8}\.tmp$"),
    },
    StateSpec {
        pattern: "tui-theme-boot.json",
        exact: None,
    },
    StateSpec {
        pattern: "tui-theme-boot.json.tmp",
        exact: None,
    },
    StateSpec {
        pattern: "proxy/iron-proxy.pid",
        exact: None,
    },
    StateSpec {
        pattern: "proxy/iron-proxy.nonce",
        exact: None,
    },
    StateSpec {
        pattern: "proxy/iron-proxy.log",
        exact: None,
    },
    StateSpec {
        pattern: "proxy/audit.log",
        exact: None,
    },
    StateSpec {
        pattern: "google_chat_bot_id.json",
        exact: None,
    },
    StateSpec {
        pattern: ".codex_gpt55_autoraise_notice",
        exact: None,
    },
    StateSpec {
        pattern: "lsp/pses",
        exact: None,
    },
    StateSpec {
        pattern: ".gateway-launchd-unsupported",
        exact: None,
    },
    StateSpec {
        pattern: "gateway_state.json.tmp",
        exact: None,
    },
    StateSpec {
        pattern: "pets/.thumbs",
        exact: None,
    },
    StateSpec {
        pattern: "state.db.repair-attempts.json",
        exact: None,
    },
    StateSpec {
        pattern: ".state.db.????????.partial",
        exact: Some(r"^\.state\.db\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "bot_relay",
        exact: None,
    },
    StateSpec {
        pattern: "proxy/management.token",
        exact: None,
    },
    StateSpec {
        pattern: "proxy/ca.key",
        exact: None,
    },
    StateSpec {
        pattern: "proxy/ca.key.staged",
        exact: None,
    },
    StateSpec {
        pattern: "projects.db*",
        exact: None,
    },
    StateSpec {
        pattern: ".gateway.pid.????????.partial",
        exact: Some(r"^\.gateway\.pid\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".gateway_state.json.????????.partial",
        exact: Some(r"^\.gateway_state\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".processes.json.????????.partial",
        exact: Some(r"^\.processes\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".auth.lock.????????.partial",
        exact: Some(r"^\.auth\.lock\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..update_check.????????.partial",
        exact: Some(r"^\.\.update_check\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".errors.log.????????.partial",
        exact: Some(r"^\.errors\.log\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..hermes_history.????????.partial",
        exact: Some(r"^\.\.hermes_history\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".MEMORY.md.????????.partial",
        exact: Some(r"^\.MEMORY\.md\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".USER.md.????????.partial",
        exact: Some(r"^\.USER\.md\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".todo.json.????????.partial",
        exact: Some(r"^\.todo\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "skills/..curator_ledger.jsonl.????????.partial",
        exact: Some(r"^skills/\.\.curator_ledger\.jsonl\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "plugins/hermes-achievements/.state.json.????????.partial",
        exact: Some(r"^plugins/hermes\-achievements/\.state\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "plugins/hermes-achievements/.scan_snapshot.json.????????.partial",
        exact: Some(r"^plugins/hermes\-achievements/\.scan_snapshot\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "plugins/hermes-achievements/.scan_checkpoint.json.????????.partial",
        exact: Some(
            r"^plugins/hermes\-achievements/\.scan_checkpoint\.json\.[a-z0-9_]{8}\.partial$",
        ),
    },
    StateSpec {
        pattern: "runtime/.photon-sidecar.json.????????.partial",
        exact: Some(r"^runtime/\.photon\-sidecar\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "rate_limits/.nous.json.????????.partial",
        exact: Some(r"^rate_limits/\.nous\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "buzz/.channel-cursors.json.????????.partial",
        exact: Some(r"^buzz/\.channel\-cursors\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".channel_directory.json.????????.partial",
        exact: Some(r"^\.channel_directory\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".*_threads.json.????????.partial",
        exact: Some(r"^\.[^/]*_threads\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".feishu_seen_message_ids.json.????????.partial",
        exact: Some(r"^\.feishu_seen_message_ids\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".google_chat_thread_counts.json.????????.partial",
        exact: Some(r"^\.google_chat_thread_counts\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".modal_snapshots.json.????????.partial",
        exact: Some(r"^\.modal_snapshots\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".singularity_snapshots.json.????????.partial",
        exact: Some(r"^\.singularity_snapshots\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".vercel_sandbox_snapshots.json.????????.partial",
        exact: Some(r"^\.vercel_sandbox_snapshots\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..skills_prompt_snapshot.json.????????.partial",
        exact: Some(r"^\.\.skills_prompt_snapshot\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".context_length_cache.yaml.????????.partial",
        exact: Some(r"^\.context_length_cache\.yaml\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".models_dev_cache.json.????????.partial",
        exact: Some(r"^\.models_dev_cache\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".models_dev_cache.etag.????????.partial",
        exact: Some(r"^\.models_dev_cache\.etag\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".provider_models_cache.json.????????.partial",
        exact: Some(r"^\.provider_models_cache\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".ollama_cloud_models_cache.json.????????.partial",
        exact: Some(r"^\.ollama_cloud_models_cache\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".sticker_cache.json.????????.partial",
        exact: Some(r"^\.sticker_cache\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".interrupt_debug.log.????????.partial",
        exact: Some(r"^\.interrupt_debug\.log\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".gateway-starts.log.????????.partial",
        exact: Some(r"^\.gateway\-starts\.log\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".perf.log.????????.partial",
        exact: Some(r"^\.perf\.log\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".gateway.lock.????????.partial",
        exact: Some(r"^\.gateway\.lock\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".gateway.sock.????????.partial",
        exact: Some(r"^\.gateway\.sock\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".gateway.sock.path.????????.partial",
        exact: Some(r"^\.gateway\.sock\.path\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..gateway-takeover.json.????????.partial",
        exact: Some(r"^\.\.gateway\-takeover\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..gateway-planned-stop.json.????????.partial",
        exact: Some(r"^\.\.gateway\-planned\-stop\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..restart_notify.json.????????.partial",
        exact: Some(r"^\.\.restart_notify\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..restart_pending.json.????????.partial",
        exact: Some(r"^\.\.restart_pending\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..restart_last_processed.json.????????.partial",
        exact: Some(r"^\.\.restart_last_processed\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..clean_shutdown.????????.partial",
        exact: Some(r"^\.\.clean_shutdown\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..update_pending.json.????????.partial",
        exact: Some(r"^\.\.update_pending\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..update_pending.claimed.json.????????.partial",
        exact: Some(r"^\.\.update_pending\.claimed\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..update_output.txt.????????.partial",
        exact: Some(r"^\.\.update_output\.txt\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..update_exit_code.????????.partial",
        exact: Some(r"^\.\.update_exit_code\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..update_prompt.json.????????.partial",
        exact: Some(r"^\.\.update_prompt\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..update_response.????????.partial",
        exact: Some(r"^\.\.update_response\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".fleet_restart_pending.????????.partial",
        exact: Some(r"^\.fleet_restart_pending\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..hermes-update-in-progress.????????.partial",
        exact: Some(r"^\.\.hermes\-update\-in\-progress\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..backup.lock.????????.partial",
        exact: Some(r"^\.\.backup\.lock\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..mcp-discovery.lock.????????.partial",
        exact: Some(r"^\.\.mcp\-discovery\.lock\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..sync.lock.????????.partial",
        exact: Some(r"^\.\.sync\.lock\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "skills/..curator_state.????????.partial",
        exact: Some(r"^skills/\.\.curator_state\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "skills/..usage.json.????????.partial",
        exact: Some(r"^skills/\.\.usage\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "skills/..usage.json.lock.????????.partial",
        exact: Some(r"^skills/\.\.usage\.json\.lock\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "skills/.hub/.audit.log.????????.partial",
        exact: Some(r"^skills/\.hub/\.audit\.log\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "pairing/.*-pending.json.????????.partial",
        exact: Some(r"^pairing/\.[^/]*\-pending\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "pairing/._rate_limits.json.????????.partial",
        exact: Some(r"^pairing/\._rate_limits\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "platforms/pairing/.*-pending.json.????????.partial",
        exact: Some(r"^platforms/pairing/\.[^/]*\-pending\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "platforms/pairing/._rate_limits.json.????????.partial",
        exact: Some(r"^platforms/pairing/\._rate_limits\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..restart_failure_counts.????????.partial",
        exact: Some(r"^\.\.restart_failure_counts\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..drain_request.json.????????.partial",
        exact: Some(r"^\.\.drain_request\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".teams_pipeline_store.json.????????.partial",
        exact: Some(r"^\.teams_pipeline_store\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "weixin/accounts/.*.context-tokens.json.????????.partial",
        exact: Some(r"^weixin/accounts/\.[^/]*\.context\-tokens\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "weixin/accounts/.*.sync.json.????????.partial",
        exact: Some(r"^weixin/accounts/\.[^/]*\.sync\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "desktop/.interrupted_turns.json.????????.partial",
        exact: Some(r"^desktop/\.interrupted_turns\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".a2a_audit.jsonl.????????.partial",
        exact: Some(r"^\.a2a_audit\.jsonl\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "runtime/.active_sessions.json.????????.partial",
        exact: Some(r"^runtime/\.active_sessions\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "runtime/.active_sessions.lock.????????.partial",
        exact: Some(r"^runtime/\.active_sessions\.lock\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".tui-theme-boot.json.????????.partial",
        exact: Some(r"^\.tui\-theme\-boot\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "proxy/.iron-proxy.pid.????????.partial",
        exact: Some(r"^proxy/\.iron\-proxy\.pid\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "proxy/.iron-proxy.nonce.????????.partial",
        exact: Some(r"^proxy/\.iron\-proxy\.nonce\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "proxy/.iron-proxy.log.????????.partial",
        exact: Some(r"^proxy/\.iron\-proxy\.log\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "proxy/.audit.log.????????.partial",
        exact: Some(r"^proxy/\.audit\.log\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".google_chat_bot_id.json.????????.partial",
        exact: Some(r"^\.google_chat_bot_id\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..codex_gpt55_autoraise_notice.????????.partial",
        exact: Some(r"^\.\.codex_gpt55_autoraise_notice\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..gateway-launchd-unsupported.????????.partial",
        exact: Some(r"^\.\.gateway\-launchd\-unsupported\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".state.db.repair-attempts.json.????????.partial",
        exact: Some(r"^\.state\.db\.repair\-attempts\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "proxy/.management.token.????????.partial",
        exact: Some(r"^proxy/\.management\.token\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "proxy/.ca.key.????????.partial",
        exact: Some(r"^proxy/\.ca\.key\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".state.db.snap_restore",
        exact: None,
    },
    StateSpec {
        pattern: ".hermes_state.db.????????.partial",
        exact: Some(r"^\.hermes_state\.db\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".hermes_state.db.snap_restore",
        exact: None,
    },
    StateSpec {
        pattern: ".response_store.db.????????.partial",
        exact: Some(r"^\.response_store\.db\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".response_store.db.snap_restore",
        exact: None,
    },
    StateSpec {
        pattern: ".verification_evidence.db.????????.partial",
        exact: Some(r"^\.verification_evidence\.db\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".verification_evidence.db.snap_restore",
        exact: None,
    },
    StateSpec {
        pattern: ".memory_store.db.????????.partial",
        exact: Some(r"^\.memory_store\.db\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".memory_store.db.snap_restore",
        exact: None,
    },
    StateSpec {
        pattern: ".runs_idempotency.db.????????.partial",
        exact: Some(r"^\.runs_idempotency\.db\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".runs_idempotency.db.snap_restore",
        exact: None,
    },
    StateSpec {
        pattern: ".projects.db.????????.partial",
        exact: Some(r"^\.projects\.db\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".projects.db.snap_restore",
        exact: None,
    },
    StateSpec {
        pattern: "google_oauth_pending.json",
        exact: None,
    },
    StateSpec {
        pattern: ".google_oauth_pending.json.????????.partial",
        exact: Some(r"^\.google_oauth_pending\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "google_chat_user_oauth_pending.json",
        exact: None,
    },
    StateSpec {
        pattern: ".google_chat_user_oauth_pending.json.????????.partial",
        exact: Some(r"^\.google_chat_user_oauth_pending\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "google_chat_user_oauth_pending",
        exact: None,
    },
];

// M-relative native boundaries. Restore entries name only known final members.
pub(super) const MACHINE_STATE: &[StateSpec] = &[
    StateSpec {
        pattern: "kanban.db*",
        exact: None,
    },
    StateSpec {
        pattern: "kanban/workspaces",
        exact: None,
    },
    StateSpec {
        pattern: "kanban/attachments",
        exact: None,
    },
    StateSpec {
        pattern: "kanban/logs",
        exact: None,
    },
    StateSpec {
        pattern: "kanban/boards/*/kanban.db*",
        exact: None,
    },
    StateSpec {
        pattern: "kanban/boards/*/workspaces",
        exact: None,
    },
    StateSpec {
        pattern: "kanban/boards/*/attachments",
        exact: None,
    },
    StateSpec {
        pattern: "kanban/boards/*/logs",
        exact: None,
    },
    StateSpec {
        pattern: "spawn-ledger.json",
        exact: None,
    },
    StateSpec {
        pattern: "spawn-ledger.json.tmp*",
        exact: Some(r"^spawn\-ledger\.json\.tmp[0-9]+$"),
    },
    StateSpec {
        pattern: "spawn-ledger.json.corrupt",
        exact: None,
    },
    StateSpec {
        pattern: "runtimes/llamacpp/window_overrides.json",
        exact: None,
    },
    StateSpec {
        pattern: "runtimes/llamacpp/server.json",
        exact: None,
    },
    StateSpec {
        pattern: "runtimes/llamacpp/presets.ini",
        exact: None,
    },
    StateSpec {
        pattern: "runtimes/llamacpp/.api_key",
        exact: None,
    },
    StateSpec {
        pattern: "install_id",
        exact: None,
    },
    StateSpec {
        pattern: ".install_id.lock",
        exact: None,
    },
    StateSpec {
        pattern: ".install_id-????????",
        exact: Some(r"^\.install_id\-[a-z0-9_]{8}$"),
    },
    StateSpec {
        pattern: ".spawn-ledger.json.????????.partial",
        exact: Some(r"^\.spawn\-ledger\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "runtimes/llamacpp/.window_overrides.json.????????.partial",
        exact: Some(r"^runtimes/llamacpp/\.window_overrides\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "runtimes/llamacpp/.server.json.????????.partial",
        exact: Some(r"^runtimes/llamacpp/\.server\.json\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "runtimes/llamacpp/.presets.ini.????????.partial",
        exact: Some(r"^runtimes/llamacpp/\.presets\.ini\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "runtimes/llamacpp/..api_key.????????.partial",
        exact: Some(r"^runtimes/llamacpp/\.\.api_key\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".install_id.????????.partial",
        exact: Some(r"^\.install_id\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "..install_id.lock.????????.partial",
        exact: Some(r"^\.\.install_id\.lock\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".kanban.db.????????.partial",
        exact: Some(r"^\.kanban\.db\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: ".kanban.db.snap_restore",
        exact: None,
    },
    StateSpec {
        pattern: "kanban/boards/*/.kanban.db.????????.partial",
        exact: Some(r"^kanban/boards/[^/]*/\.kanban\.db\.[a-z0-9_]{8}\.partial$"),
    },
    StateSpec {
        pattern: "kanban/boards/*/.kanban.db.snap_restore",
        exact: None,
    },
];

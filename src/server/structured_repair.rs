//! Rebuilding structured session rows from the live worker registry when
//! the stored rows disagree with it.

use crate::session::Instance;
use std::sync::Arc;

use super::state::AppState;

pub(super) struct StructuredRowRepair {
    session_id: String,
    source_profile: String,
    agent_name: Option<String>,
    agent_model: Option<String>,
    acp_session_id: String,
}

pub(super) type LiveStructuredWorkerRecord =
    (crate::process::worker_registry::WorkerRecord, String);

pub(super) fn live_structured_worker_records() -> Vec<LiveStructuredWorkerRecord> {
    use crate::process::worker_registry::{self, is_record_live};

    let records = match worker_registry::list() {
        Ok(records) => records,
        Err(e) => {
            tracing::warn!(
                target: "server.file_watch",
                error = %e,
                "worker registry list failed; structured row repair disabled this tick"
            );
            return Vec::new();
        }
    };
    records
        .into_iter()
        .filter(is_record_live)
        .filter_map(|record| {
            let acp_session_id = record
                .stored_acp_session_id
                .as_deref()
                .filter(|id| !id.is_empty())?
                .to_string();
            Some((record, acp_session_id))
        })
        .collect()
}

/// Runs before the ACP overlay so freshly repaired rows pass the
/// `is_structured()` gate and keep their live worker status.
pub(super) fn repair_structured_rows_from_live_workers(
    merged: &mut [Instance],
    records: Vec<LiveStructuredWorkerRecord>,
) -> Vec<StructuredRowRepair> {
    let live_by_id: std::collections::HashMap<String, LiveStructuredWorkerRecord> = records
        .into_iter()
        .map(|(record, acp_session_id)| (record.session_id.clone(), (record, acp_session_id)))
        .collect();

    let mut repairs = Vec::new();
    for inst in merged.iter_mut() {
        if inst.is_structured() {
            continue;
        }
        let Some((record, acp_session_id)) = live_by_id.get(&inst.id) else {
            continue;
        };
        if inst.acp_session_id.is_some()
            && inst.acp_session_id.as_deref() != Some(acp_session_id.as_str())
        {
            tracing::warn!(
                target: "server.file_watch",
                session = %inst.id,
                disk = ?inst.acp_session_id,
                registry = %acp_session_id,
                "repairing structured session row with mismatched ACP session id"
            );
        }
        inst.view = crate::session::View::Structured;
        if inst.agent_name.is_none() && !record.agent_key.is_empty() {
            inst.agent_name = Some(record.agent_key.clone());
        }
        if inst.agent_model.is_none() {
            inst.agent_model = record.model.clone();
        }
        inst.acp_session_id = Some(acp_session_id.clone());
        tracing::warn!(
            target: "server.file_watch",
            session = %inst.id,
            pid = record.pid,
            "repaired structured session row from live ACP worker registry"
        );
        repairs.push(StructuredRowRepair {
            session_id: inst.id.clone(),
            source_profile: inst.source_profile.clone(),
            agent_name: inst.agent_name.clone(),
            agent_model: inst.agent_model.clone(),
            acp_session_id: acp_session_id.clone(),
        });
    }
    repairs
}

pub(super) fn persist_structured_row_repairs(
    state: &Arc<AppState>,
    repairs: Vec<StructuredRowRepair>,
) {
    if repairs.is_empty() {
        return;
    }
    let state = state.clone();
    let file_watch = state.file_watch.clone();
    let shutdown = state.shutdown.clone();
    crate::task_util::spawn_supervised(
        "server.reload.persist_repairs",
        crate::task_util::PanicPolicy::Log,
        async move {
            let mut by_profile: std::collections::HashMap<String, Vec<StructuredRowRepair>> =
                std::collections::HashMap::new();
            for repair in repairs {
                by_profile
                    .entry(repair.source_profile.clone())
                    .or_default()
                    .push(repair);
            }
            for (profile, repairs) in by_profile {
                if shutdown.is_cancelled() {
                    break;
                }
                let file_watch = file_watch.clone();
                let failed_ids: Vec<String> = repairs
                    .iter()
                    .map(|repair| repair.session_id.clone())
                    .collect();
                let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let storage = crate::session::Storage::new(&profile, file_watch)?;
                    storage.update(|all, _groups| {
                        for repair in repairs {
                            if let Some(inst) = all.iter_mut().find(|i| i.id == repair.session_id) {
                                inst.view = crate::session::View::Structured;
                                if inst.agent_name.is_none() {
                                    inst.agent_name = repair.agent_name;
                                }
                                if inst.agent_model.is_none() {
                                    inst.agent_model = repair.agent_model;
                                }
                                inst.acp_session_id = Some(repair.acp_session_id);
                            } else {
                                tracing::debug!(
                                    target: "server.file_watch",
                                    session = %repair.session_id,
                                    "repair target not found on disk; skipping"
                                );
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                })
                .await;
                match save_result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        rollback_structured_row_repairs(&state, &failed_ids).await;
                        tracing::warn!(target: "server.file_watch", "save after structured row repair: {e}");
                    }
                    Err(join_err) => {
                        rollback_structured_row_repairs(&state, &failed_ids).await;
                        tracing::warn!(
                            target: "server.file_watch",
                            "structured row repair save task panicked: {join_err}"
                        );
                    }
                }
            }
        },
    );
}

pub(super) async fn rollback_structured_row_repairs(state: &Arc<AppState>, failed_ids: &[String]) {
    let mut instances = state.instances.write().await;
    for inst in instances.iter_mut() {
        if failed_ids.iter().any(|id| id == &inst.id) {
            inst.view = crate::session::View::Terminal;
            inst.acp_session_id = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn repair_structured_rows_from_live_workers_restores_structured_session_rows() {
        let temp = tempfile::TempDir::with_prefix_in("aoe-repair-", "/tmp").expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let socket_path = crate::process::worker_registry::workers_dir()
            .expect("workers dir")
            .join("repair-live.sock");
        std::fs::write(&socket_path, b"").expect("socket sentinel");
        let live_record = crate::process::worker_registry::WorkerRecord::new(
            "repair-live".to_string(),
            std::process::id(),
            socket_path,
            "codex-acp".to_string(),
            "codex".to_string(),
            std::path::PathBuf::from("/tmp/repo"),
            Some("gpt-5".to_string()),
            Vec::new(),
            Vec::new(),
            Some("acp-session-1".to_string()),
            Some("default".to_string()),
        );
        crate::process::worker_registry::save(&live_record).expect("save live worker record");

        let existing_socket_path = crate::process::worker_registry::workers_dir()
            .expect("workers dir")
            .join("repair-existing.sock");
        std::fs::write(&existing_socket_path, b"").expect("socket sentinel");
        let existing_record = crate::process::worker_registry::WorkerRecord::new(
            "repair-existing".to_string(),
            std::process::id(),
            existing_socket_path,
            "codex-acp".to_string(),
            "codex".to_string(),
            std::path::PathBuf::from("/tmp/repo"),
            Some("gpt-5".to_string()),
            Vec::new(),
            Vec::new(),
            Some("acp-session-2".to_string()),
            Some("default".to_string()),
        );
        crate::process::worker_registry::save(&existing_record)
            .expect("save existing-field worker record");

        let stale_socket_path = crate::process::worker_registry::workers_dir()
            .expect("workers dir")
            .join("repair-no-id.sock");
        std::fs::write(&stale_socket_path, b"").expect("socket sentinel");
        let no_id_record = crate::process::worker_registry::WorkerRecord::new(
            "repair-no-id".to_string(),
            std::process::id(),
            stale_socket_path,
            "codex-acp".to_string(),
            "codex".to_string(),
            std::path::PathBuf::from("/tmp/repo"),
            None,
            Vec::new(),
            Vec::new(),
            None,
            Some("default".to_string()),
        );
        crate::process::worker_registry::save(&no_id_record).expect("save no-id worker record");

        let empty_socket_path = crate::process::worker_registry::workers_dir()
            .expect("workers dir")
            .join("repair-empty-id.sock");
        std::fs::write(&empty_socket_path, b"").expect("socket sentinel");
        let empty_id_record = crate::process::worker_registry::WorkerRecord::new(
            "repair-empty-id".to_string(),
            std::process::id(),
            empty_socket_path,
            "codex-acp".to_string(),
            "codex".to_string(),
            std::path::PathBuf::from("/tmp/repo"),
            None,
            Vec::new(),
            Vec::new(),
            Some(String::new()),
            Some("default".to_string()),
        );
        crate::process::worker_registry::save(&empty_id_record)
            .expect("save empty-id worker record");

        let mut rows = vec![
            Instance::new("repair-live", "/tmp/repo"),
            Instance::new("repair-existing", "/tmp/repo"),
            Instance::new("repair-no-id", "/tmp/repo"),
            Instance::new("repair-empty-id", "/tmp/repo"),
        ];
        rows[0].id = "repair-live".to_string();
        rows[1].id = "repair-existing".to_string();
        rows[1].agent_name = Some("custom-agent".to_string());
        rows[1].agent_model = Some("custom-model".to_string());
        rows[2].id = "repair-no-id".to_string();
        rows[3].id = "repair-empty-id".to_string();

        let live_records = live_structured_worker_records();
        let repairs = repair_structured_rows_from_live_workers(&mut rows, live_records);

        assert_eq!(repairs.len(), 2);
        assert_eq!(repairs[0].session_id, "repair-live");
        assert_eq!(repairs[0].acp_session_id, "acp-session-1");
        assert_eq!(rows[0].view, crate::session::View::Structured);
        assert_eq!(rows[0].agent_name.as_deref(), Some("codex"));
        assert_eq!(rows[0].agent_model.as_deref(), Some("gpt-5"));
        assert_eq!(rows[0].acp_session_id.as_deref(), Some("acp-session-1"));
        assert_eq!(rows[1].view, crate::session::View::Structured);
        assert_eq!(rows[1].agent_name.as_deref(), Some("custom-agent"));
        assert_eq!(rows[1].agent_model.as_deref(), Some("custom-model"));
        assert_eq!(rows[1].acp_session_id.as_deref(), Some("acp-session-2"));
        assert_eq!(rows[2].view, crate::session::View::Terminal);
        assert_eq!(rows[2].acp_session_id, None);
        assert_eq!(rows[3].view, crate::session::View::Terminal);
        assert_eq!(rows[3].acp_session_id, None);
    }
}

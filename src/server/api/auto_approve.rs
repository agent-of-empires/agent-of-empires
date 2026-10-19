//! One acknowledged approval-policy operation for terminal and structured views.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;

use crate::server::AppState;
use crate::session::auto_approve::AutoApproveResult;
use crate::session::{Instance, Status, Storage};

#[derive(Deserialize)]
pub struct SetAutoApproveRequest {
    pub enabled: bool,
}

fn failure(error: impl std::fmt::Display) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "message": error.to_string() })),
    )
        .into_response()
}

pub async fn set_auto_approve(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SetAutoApproveRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if state.read_only {
        return super::read_only_response();
    }
    if let Some(response) = super::cityhall_block(&state) {
        return response;
    }
    let Json(req) = match req {
        Ok(req) => req,
        Err(error) => return error.into_response(),
    };
    if !state
        .instances
        .read()
        .await
        .iter()
        .any(|instance| instance.id == id)
    {
        return (StatusCode::NOT_FOUND, "Session not found").into_response();
    }
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock_owned().await;
    let Some(instance) = state
        .instances
        .read()
        .await
        .iter()
        .find(|instance| instance.id == id)
        .cloned()
    else {
        return (StatusCode::NOT_FOUND, "Session not found").into_response();
    };
    // A disconnected HTTP caller must not abandon a native policy change
    // between adapter acknowledgement and its durable write-back.
    let task = tokio::spawn(async move {
        let _guard = guard;
        let result = if instance.is_structured() {
            set_structured_auto_approve(&state, &instance, req.enabled).await
        } else {
            let profile = instance.source_profile.clone();
            let id = id.clone();
            tokio::task::spawn_blocking(move || {
                crate::session::auto_approve::set_terminal_auto_approve(&profile, &id, req.enabled)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
        };
        let terminal_restarted =
            !instance.is_structured() && result.as_ref().is_ok_and(|result| result.restarted);
        if terminal_restarted {
            crate::session::recovery::mark_recently_restarted(&state.recently_restarted, &id);
        }
        // Mirror durable policy without replacing live daemon-only bookkeeping.
        let profile = instance.source_profile;
        let id_for_load = id.clone();
        let fresh = tokio::task::spawn_blocking(move || {
            Storage::open_unwatched(&profile)?
                .load()?
                .into_iter()
                .find(|instance| instance.id == id_for_load)
                .ok_or_else(|| anyhow::anyhow!("Session disappeared while updating auto-approve"))
        })
        .await;
        if let Ok(Ok(fresh)) = fresh {
            if let Some(instance) = state
                .instances
                .write()
                .await
                .iter_mut()
                .find(|instance| instance.id == id)
            {
                instance.yolo_mode = fresh.yolo_mode;
                instance.auto_approve_managed = fresh.auto_approve_managed;
                instance.acp_mode_id = fresh.acp_mode_id;
                instance.op_claim = fresh.op_claim;
                if terminal_restarted {
                    instance.status = fresh.status;
                    instance.last_start_time = fresh.last_start_time;
                    instance.last_error = fresh.last_error;
                }
            }
        }
        match result {
            Ok(result) => Json(result).into_response(),
            Err(error) => failure(error),
        }
    });
    match task.await {
        Ok(response) => response,
        Err(error) => failure(format!("Auto-approve operation task failed: {error}")),
    }
}

async fn set_structured_auto_approve(
    state: &Arc<AppState>,
    instance: &Instance,
    enabled: bool,
) -> anyhow::Result<AutoApproveResult> {
    let profile = instance.source_profile.clone();
    let id = instance.id.clone();
    let claimed = tokio::task::spawn_blocking(move || {
        let storage = Storage::open_unwatched(&profile)?;
        let mut claimed = None;
        storage.update(|instances, _| {
            let row = instances
                .iter_mut()
                .find(|instance| instance.id == id)
                .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
            row.source_profile = profile.clone();
            anyhow::ensure!(
                row.is_structured(),
                "Session view changed; retry the auto-approve change"
            );
            anyhow::ensure!(
                !matches!(
                    row.status,
                    Status::Creating | Status::Deleting | Status::Starting
                ) && row.trashed_at.is_none(),
                "Session is not available for an auto-approve change"
            );
            let now = Utc::now();
            anyhow::ensure!(
                !row.op_claim
                    .as_ref()
                    .is_some_and(|claim| now - claim.at < Instance::OP_CLAIM_TTL),
                "Another session operation is in progress"
            );
            row.try_claim(
                crate::session::ClaimOp::AutoApprove,
                Instance::OP_CLAIM_TTL,
                now,
            )
            .map_err(|op| anyhow::anyhow!("Another session operation is in progress: {op:?}"))?;
            claimed = Some(row.clone());
            Ok(())
        })?;
        Ok::<_, anyhow::Error>(claimed.expect("claimed row set on successful update"))
    })
    .await??;
    let native = state
        .acp_supervisor
        .set_auto_approve(&claimed.id, enabled)
        .await;
    let profile = claimed.source_profile.clone();
    tokio::task::spawn_blocking(move || {
        let storage = Storage::open_unwatched(&profile)?;
        storage.update(|instances, _| {
            let row = instances
                .iter_mut()
                .find(|instance| instance.id == claimed.id)
                .ok_or_else(|| anyhow::anyhow!("Session disappeared during approval change"))?;
            anyhow::ensure!(
                row.op_claim == claimed.op_claim,
                "Session operation reservation changed during approval change"
            );
            if let Ok((mode, _)) = &native {
                anyhow::ensure!(
                    row.is_structured(),
                    "Session view changed during approval change"
                );
                row.yolo_mode = enabled;
                row.auto_approve_managed = true;
                if let Some(mode) = mode {
                    // OFF replaces stale bypass metadata as well as the bool.
                    row.acp_mode_id = Some(mode.clone());
                }
            }
            row.op_claim = None;
            Ok(())
        })?;
        let (_, restarted) = native?;
        Ok::<_, anyhow::Error>(AutoApproveResult { enabled, restarted })
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_restricted_or_unknown_targets_before_mutation() {
        let mut state = crate::server::test_support::build_test_app_state(Vec::new());
        for (read_only, cityhall) in [(true, false), (false, true), (false, false)] {
            let state_mut = Arc::get_mut(&mut state).expect("unshared state");
            state_mut.read_only = read_only;
            state_mut.cityhall_mode = cityhall;
            let response = set_auto_approve(
                State(state.clone()),
                Path("missing".into()),
                Ok(Json(SetAutoApproveRequest { enabled: true })),
            )
            .await;
            assert_eq!(
                response.status(),
                if read_only || cityhall {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::NOT_FOUND
                }
            );
            assert!(state.instance_locks.read().await.is_empty());
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_failure_preserves_policy_and_releases_only_its_claim() {
        let _tmp = crate::session::test_support::isolate_app_dir();
        let mut instance = Instance::new("approval", "/tmp");
        instance.source_profile = "approval-profile".into();
        instance.view = crate::session::View::Structured;
        instance.yolo_mode = true;
        instance.acp_mode_id = Some("bypassPermissions".into());
        let storage = Storage::new_unwatched("approval-profile").unwrap();
        for claimed_by_peer in [false, true] {
            if claimed_by_peer {
                instance
                    .try_claim(
                        crate::session::ClaimOp::AutoApprove,
                        Instance::OP_CLAIM_TTL,
                        Utc::now(),
                    )
                    .unwrap();
            }
            storage
                .update(|instances, _| {
                    *instances = vec![instance.clone()];
                    Ok(())
                })
                .unwrap();
            let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
            assert!(set_structured_auto_approve(&state, &instance, false)
                .await
                .is_err());
            let row = storage.load().unwrap().remove(0);
            assert!(row.yolo_mode);
            assert_eq!(row.acp_mode_id.as_deref(), Some("bypassPermissions"));
            assert_eq!(row.op_claim, instance.op_claim);
        }
    }
}

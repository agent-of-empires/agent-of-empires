//! Ordered durable TUI transactions. All flocks and effects stay on the persistence worker.
use super::operations::{
    rekey_tmux_after_persist, worktree_rename_block, worktree_rename_block_message,
};
use super::*;
use crate::session::conversation_carry;
use crate::session::{
    duplicate_session_error, is_duplicate_session, list_profiles, LifecycleOperation,
};
use crate::tui::restart_poller::RestartRequest;
use crate::tui::stop_poller::{SettledEdit, SettlementAction, SettlementRequest};
use std::sync::Arc;

pub(super) struct RowCapture {
    pub before: Box<Instance>,
    pub origin: RequestOrigin,
}

impl RowCapture {
    pub fn capture(before: Instance) -> anyhow::Result<Self> {
        let origin = RequestOrigin::capture(&before)?;
        Ok(Self {
            before: Box::new(before),
            origin,
        })
    }
    fn check(&self, row: &Instance, settled: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            row.id == self.before.id && row.created_at == self.before.created_at,
            "Original session was removed or replaced: {}",
            self.before.id
        );
        anyhow::ensure!(
            row.storage_origin
                .as_ref()
                .is_some_and(|s| self.origin.storage.same_origin_as(s)),
            "Original session storage changed: {}",
            row.id
        );
        anyhow::ensure!(
            settled || self.origin.matches(row),
            "Session lifecycle changed before transaction: {}",
            row.id
        );
        Ok(())
    }
}

pub(super) enum TransactionRequest {
    SetSortOrder(SortOrder),
    SetGroupBy(GroupByMode),
    DisableQuitConfirmation,
    PrepareClaimAbort(RowCapture),
    AbortClaim(crate::session::retained_intents::ClaimAbort),
    ResolveCreation {
        row: RowCapture,
        action: CreationRecoveryAction,
        cancel: Option<tokio_util::sync::CancellationToken>,
    },
    RecoverCreation(CreationConfirmation),
    Ordering(OrderingRequest),
    AttachProject {
        row: RowCapture,
        repo_path: std::path::PathBuf,
    },
    StoreMove {
        row: RowCapture,
        resume: Option<crate::tui::app::Action>,
    },
    Launch {
        row: RowCapture,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    },
    Restart {
        row: RowCapture,
        target: Option<Storage>,
        tool: Option<String>,
        extra: Option<String>,
        command: Option<String>,
        wake_snooze: bool,
        size: Option<(u16, u16)>,
    },
    Rename {
        row: RowCapture,
        target: Option<Storage>,
        title: String,
        group: Option<String>,
        rename_branch: bool,
        settled: Option<SettledEdit>,
    },
    Workdir {
        row: RowCapture,
        name: String,
        rename_branch: bool,
        settled: Option<SettledEdit>,
    },
    Group {
        source: Storage,
        target: Option<Storage>,
        old_path: String,
        new_path: String,
        members: Vec<RowCapture>,
    },
    #[cfg(test)]
    Move {
        row: RowCapture,
        target: Storage,
        requested: Box<Instance>,
        account_swap: bool,
    },
    Metadata {
        edits: Vec<MetadataEdit>,
        after: MetadataContinuation,
    },
    Passive {
        row: RowCapture,
        mark_unread: bool,
    },
    SwitchProfile {
        profile: Option<String>,
        target: Option<Storage>,
    },
    CreateProfile(String),
    DeleteProfile(Storage),
    AdmitCreation {
        storage: Option<Storage>,
        request: Box<CreationAdmission>,
    },
    PublishCreation {
        custody: Arc<crate::session::builder::CreationCustody>,
        cancel: tokio_util::sync::CancellationToken,
    },
    Archive {
        row: RowCapture,
        settled: Option<SettledEdit>,
        reveal: bool,
        successor: Option<String>,
    },
    Trash(RowCapture),
    Restore {
        row: RowCapture,
        owned_generation: Option<u64>,
    },
    DeleteGroup {
        profiles: Vec<Storage>,
        path: String,
        restarting: HashSet<String>,
        options: Option<GroupDeleteOptionsOwned>,
    },
}

pub(super) struct CreationAdmission {
    pub admitted_instance: Instance,
    pub data: NewSessionData,
    pub existing_instances: Vec<Instance>,
    pub hooks: Option<crate::session::config::repo_config::ResolvedHooks>,
    pub cancel: tokio_util::sync::CancellationToken,
}

pub(super) struct GroupDeleteOptionsOwned {
    pub worktrees: bool,
    pub branches: bool,
    pub containers: bool,
    pub force_worktrees: bool,
}
pub(super) enum MetadataContinuation {
    None,
    Reseat(String),
    Snooze { id: String, message: String },
    Unread(String),
}
pub(super) struct MetadataEdit {
    pub row: RowCapture,
    pub after: Instance,
    pub revision: u64,
}
pub(super) enum TransactionEffect {
    PresentationSaved,
    QuitConfirmationDisabled,
    ClaimAbortConfirmation(crate::session::retained_intents::ClaimAbort),
    ClaimAborted(crate::session::retained_intents::ClaimAbortAck),
    CreationConfirmation(CreationConfirmation),
    CreationWithdrawn {
        original: CreationRecoveryCapture,
        ack: crate::session::builder::CreationWithdrawalAck,
    },
    CreationAlreadyPublished(String),
    Ordered {
        rows: Vec<Instance>,
        id: Option<String>,
        destination: Option<String>,
        warning: Option<String>,
        stale: bool,
    },
    Edited {
        rows: Vec<Instance>,
        warning: Option<String>,
    },
    MetadataCommitted {
        rows: Vec<Instance>,
        after: MetadataContinuation,
    },
    Restart {
        request: Box<RestartRequest>,
        origin: RequestOrigin,
        attach: bool,
    },
    Settlement {
        request: Box<SettlementRequest>,
        origin: RequestOrigin,
    },
    AttachProject {
        request: crate::session::attach_project::AttachProjectRequest,
        origin: RequestOrigin,
    },
    StoreMove(Box<crate::tui::store_move_poller::StoreMoveRequest>),
    Switched {
        profile: Option<String>,
    },
    ProfileDeleted(String),
    Admitted(Box<crate::tui::creation_poller::CreationRequest>),
    AdmissionCancelledBeforeEffect(String),
    Created {
        row: Box<Instance>,
        hooks_ran: bool,
        warnings: Vec<String>,
        auto_attach: bool,
    },
    Archived {
        row: Box<Instance>,
        reveal: bool,
        successor: Option<String>,
    },
    Trash(Box<crate::session::trash::TrashRequest>),
    Restored {
        id: String,
        outcome: super::operations::RestoreFromTrash,
    },
    DeleteGroup {
        requests: Vec<crate::tui::deletion_poller::DeletionRequest>,
        profiles: Vec<String>,
        path: String,
    },
}

// This is a single authoritative row, not a second view or an owner of UI state.
struct RowTransaction {
    row: Instance,
    storage: Arc<Storage>,
    target: Option<Storage>,
}
impl RowTransaction {
    fn load(capture: &RowCapture, target: Option<Storage>, settled: bool) -> anyhow::Result<Self> {
        capture.origin.storage.verify_profile_identity()?;
        if let Some(target) = &target {
            target.verify_profile_identity()?;
        }
        let mut row = capture
            .origin
            .storage
            .load()?
            .into_iter()
            .find(|r| r.id == capture.before.id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", capture.before.id))?;
        capture.check(&row, settled)?;
        row.merge_runtime_from_reload(&capture.before);
        Ok(Self {
            row,
            storage: capture.origin.storage.clone(),
            target,
        })
    }
    fn get_instance(&self, id: &str) -> Option<&Instance> {
        (self.row.id == id).then_some(&self.row)
    }
    fn mutate_instance(&mut self, id: &str, f: impl FnOnce(&mut Instance)) {
        assert_eq!(self.row.id, id);
        f(&mut self.row);
    }
    fn tie_workdir_applies_for(&self, _: &str) -> bool {
        self.row.tie_workdir_applies(
            crate::session::resolve_config_or_warn(&self.row.source_profile)
                .session
                .tie_workdir_to_name,
        )
    }
    fn apply_user_action_under_workspace_claim_lock(
        &mut self,
        id: &str,
        mutate: impl FnOnce(&mut Instance),
    ) -> anyhow::Result<()> {
        let before = self.row.clone();
        let mut after = before.clone();
        mutate(&mut after);
        self.row = self
            .storage
            .update_under_workspace_claim_lock(|rows, groups| {
                let row = rows
                    .iter_mut()
                    .find(|r| r.id == id && r.created_at == before.created_at)
                    .ok_or_else(|| anyhow::anyhow!("Original session disappeared: {id}"))?;
                anyhow::ensure!(
                    row.lifecycle_generation == before.lifecycle_generation,
                    "Session lifecycle changed: {id}"
                );
                row.merge_user_action_diff(&before, &after);
                let committed = row.clone();
                if !committed.group_path.is_empty() {
                    let mut tree = GroupTree::new_with_groups(rows, groups);
                    tree.create_group(&committed.group_path);
                    *groups = tree.get_all_groups();
                }
                Ok(committed)
            })?;
        self.row.merge_runtime_from_reload(&after);
        Ok(())
    }
    fn move_to_profile_with_effect(
        &mut self,
        id: &str,
        target: &str,
        requested: Instance,
        baseline: Option<&Instance>,
        account_swap: bool,
        effect: impl FnOnce(&Instance) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.row.status != crate::session::Status::Creating
                && self.row.status != crate::session::Status::Deleting
                && !self
                    .row
                    .has_active_lifecycle_reservation(chrono::Utc::now()),
            "Cannot move session {id} while a lifecycle operation is in progress"
        );
        let destination = self
            .target
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Original target storage was not captured"))?;
        anyhow::ensure!(destination.profile() == target, "Target profile changed");
        destination.verify_profile_identity()?;
        let before = baseline.unwrap_or(&self.row);
        let mut committed = self.storage.move_instance_to_with_effect(
            destination,
            before,
            &requested,
            account_swap,
            |rows, candidate| {
                if crate::session::is_duplicate_session(
                    rows.iter(),
                    &candidate.title,
                    &candidate.project_path,
                    None,
                ) {
                    return Err(crate::session::duplicate_session_error(&candidate.title));
                }
                Ok(())
            },
            effect,
        )?;
        committed.merge_runtime_for_profile_move(&self.row);
        self.storage = committed.original_storage()?;
        self.row = committed;
        Ok(())
    }
}

fn settlement(row: Instance, action: SettlementAction) -> anyhow::Result<TransactionEffect> {
    let origin = RequestOrigin::capture(&row)?;
    let request = SettlementRequest {
        session_id: row.id.clone(),
        storage: origin.storage.as_ref().clone(),
        instance: row,
        action,
    };
    Ok(TransactionEffect::Settlement {
        request: Box::new(request),
        origin,
    })
}

pub(super) fn perform(
    request: TransactionRequest,
    file_watch: &Arc<crate::file_watch::FileWatchService>,
) -> anyhow::Result<TransactionEffect> {
    match request {
        TransactionRequest::PrepareClaimAbort(row) => {
            let selection =
                crate::session::retained_intents::capture(&row.origin.storage, &row.before.id)?;
            selection.check_visible(&row.before)?;
            Ok(TransactionEffect::ClaimAbortConfirmation(selection))
        }
        TransactionRequest::AbortClaim(selection) => {
            crate::session::retained_intents::abort(&selection).map(TransactionEffect::ClaimAborted)
        }
        TransactionRequest::SetSortOrder(order) => {
            update_app_state(|state| state.sort_order = Some(order))?;
            Ok(TransactionEffect::PresentationSaved)
        }
        TransactionRequest::SetGroupBy(mode) => {
            update_app_state(|state| state.group_by = Some(mode))?;
            Ok(TransactionEffect::PresentationSaved)
        }
        TransactionRequest::DisableQuitConfirmation => {
            update_config(|config| config.session.confirm_before_quit = false)?;
            Ok(TransactionEffect::QuitConfirmationDisabled)
        }
        TransactionRequest::ResolveCreation {
            row,
            action,
            cancel,
        } => {
            row.origin.storage.verify_profile_identity()?;
            let mut found = None;
            for custody in crate::session::builder::CreationCustody::retained() {
                if custody.session_id() == row.before.id
                    && custody.created_at() == row.before.created_at
                    && custody.storage().same_origin_as(&row.origin.storage)
                    && custody.generation() == Some(row.origin.generation)
                    && custody.matches_original(
                        &row.origin.storage,
                        &row.before.id,
                        row.before.created_at,
                        row.origin.generation,
                    )?
                {
                    found = Some(custody);
                    break;
                }
            }
            let custody=found.ok_or_else(||anyhow::anyhow!("Original Creating custody is unavailable for this physical profile/ID/DOB/generation. Its lost original PFD/native receipts cannot be reconstructed from a stored row; the actual original custodian is required."))?;
            let capture = CreationRecoveryCapture {
                custody,
                id: row.before.id,
                created_at: row.before.created_at,
                generation: row.origin.generation,
                storage: row.origin.storage,
                cancel,
            };
            Ok(TransactionEffect::CreationConfirmation(
                CreationConfirmation { capture, action },
            ))
        }
        TransactionRequest::RecoverCreation(confirmation) => {
            let CreationConfirmation { capture, action } = confirmation;
            capture.check()?;
            match action {
                CreationRecoveryAction::RetryPublication => {
                    if matches!(
                        capture.custody.undo_verdict(),
                        crate::session::builder::CreationUndoVerdict::AlreadyPublished
                    ) {
                        return Ok(TransactionEffect::CreationAlreadyPublished(capture.id));
                    }
                    let ready=capture.custody.ready().ok_or_else(||anyhow::anyhow!("Original creation is not prepared for publication; its original resources remain retained"))?;
                    let mut row = capture.custody.retry_publication()?;
                    row.merge_runtime_from_reload(&ready.instance);
                    Ok(TransactionEffect::Created {
                        row: Box::new(row),
                        hooks_ran: ready.on_launch_hooks_ran,
                        warnings: ready.warnings,
                        auto_attach: true,
                    })
                }
                CreationRecoveryAction::Undo => {
                    if let Some(cancel) = &capture.cancel {
                        cancel.cancel();
                    }
                    let ack = capture.custody.withdraw()?;
                    anyhow::ensure!(ack.matches_original(&capture.storage,&capture.id,capture.created_at,capture.generation)?,"Original native/resource withdrawal acknowledgement mismatched; no UI withdrawal was acknowledged");
                    Ok(TransactionEffect::CreationWithdrawn {
                        original: capture,
                        ack,
                    })
                }
            }
        }
        TransactionRequest::Ordering(request) => ordering(request),
        TransactionRequest::AttachProject { row, repo_path } => {
            let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
            let _identity = crate::session::acquire_session_identity_lock()?;
            let _title = crate::session::acquire_session_title_lock(&row.before.id)?;
            let _lifecycle = row
                .origin
                .storage
                .acquire_instance_lifecycle_lock(&row.before.id)?;
            let tx = RowTransaction::load(&row, None, false)?;
            anyhow::ensure!(
                !tx.row.status.blocks_worktree_edit()
                    && !tx.row.is_archived()
                    && !tx.row.is_trashed(),
                "Original session cannot attach a project in its current lifecycle"
            );
            let original = crate::session::LaunchOrigin::capture(&tx.row)?;
            let origin = RequestOrigin::capture(&tx.row)?;
            Ok(TransactionEffect::AttachProject {
                request: crate::session::attach_project::AttachProjectRequest {
                    original,
                    repo_path,
                },
                origin,
            })
        }
        TransactionRequest::StoreMove { row, resume } => {
            let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
            let _identity = crate::session::acquire_session_identity_lock()?;
            let _title = crate::session::acquire_session_title_lock(&row.before.id)?;
            let _lifecycle = row
                .origin
                .storage
                .acquire_instance_lifecycle_lock(&row.before.id)?;
            let tx = RowTransaction::load(&row, None, false)?;
            Ok(TransactionEffect::StoreMove(Box::new(
                crate::tui::store_move_poller::StoreMoveRequest {
                    instance: tx.row,
                    resume,
                },
            )))
        }
        TransactionRequest::Launch {
            row,
            size,
            skip_on_launch,
        } => {
            let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
            let _identity = crate::session::acquire_session_identity_lock()?;
            let _title = crate::session::acquire_session_title_lock(&row.before.id)?;
            let _lifecycle = row
                .origin
                .storage
                .acquire_instance_lifecycle_lock(&row.before.id)?;
            let tx = RowTransaction::load(&row, None, false)?;
            let origin = RequestOrigin::capture(&tx.row)?;
            Ok(TransactionEffect::Restart {
                origin,
                attach: true,
                request: Box::new(RestartRequest {
                    session_id: tx.row.id.clone(),
                    instance: tx.row,
                    size,
                    wake_message: String::new(),
                    skip_on_launch,
                    bound_hooks: false,
                    discard_sandbox_container: false,
                    conversation_carry: None,
                }),
            })
        }
        TransactionRequest::Restart {
            row,
            target,
            tool,
            extra,
            command,
            wake_snooze,
            size,
        } => restart(row, target, tool, extra, command, wake_snooze, size),
        TransactionRequest::Rename {
            row,
            target,
            title,
            group,
            rename_branch,
            settled,
        } => rename(
            row,
            target,
            &title,
            group.as_deref(),
            rename_branch,
            settled,
        ),
        TransactionRequest::Workdir {
            row,
            name,
            rename_branch,
            settled,
        } => workdir(row, &name, rename_branch, settled),
        TransactionRequest::Group {
            source,
            target,
            old_path,
            new_path,
            members,
        } => group(source, target, &old_path, &new_path, members),
        #[cfg(test)]
        TransactionRequest::Move {
            row,
            target,
            requested,
            account_swap,
        } => {
            let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
            let _identity = crate::session::acquire_session_identity_lock()?;
            let _title = crate::session::acquire_session_title_lock(&row.before.id)?;
            let _lifecycle = row
                .origin
                .storage
                .acquire_instance_lifecycle_lock(&row.before.id)?;
            let mut tx = RowTransaction::load(&row, Some(target), false)?;
            let target = tx.target.as_ref().unwrap().profile().to_owned();
            if target == tx.row.source_profile {
                let before = tx.row.clone();
                tx.apply_user_action_under_workspace_claim_lock(&row.before.id, |r| {
                    r.merge_user_action_diff(&before, &requested)
                })?;
            } else {
                tx.move_to_profile_with_effect(
                    &row.before.id,
                    &target,
                    *requested,
                    Some(&row.before),
                    account_swap,
                    |_| Ok(()),
                )?;
            }
            Ok(TransactionEffect::Edited {
                rows: vec![tx.row],
                warning: None,
            })
        }
        TransactionRequest::Metadata { edits, after } => metadata(edits, after),
        TransactionRequest::Passive { row, mark_unread } => passive(row, mark_unread),
        TransactionRequest::SwitchProfile { profile, target } => {
            if let Some(target) = target {
                target.verify_profile_identity()?;
            }
            Ok(TransactionEffect::Switched { profile })
        }
        TransactionRequest::CreateProfile(name) => {
            crate::session::create_profile(&name)?;
            Ok(TransactionEffect::Switched {
                profile: Some(name),
            })
        }
        TransactionRequest::DeleteProfile(storage) => {
            crate::session::delete_original_profile(&storage)?;
            Ok(TransactionEffect::ProfileDeleted(
                storage.profile().to_owned(),
            ))
        }
        TransactionRequest::AdmitCreation { storage, request } => {
            admit_creation(storage, *request, file_watch)
        }
        TransactionRequest::PublishCreation { custody, cancel } => {
            anyhow::ensure!(
                !cancel.is_cancelled(),
                "Creation publication cancelled; original custody and resources remain retained"
            );
            let ready = custody.ready().ok_or_else(|| {
                anyhow::anyhow!("Original creation has no complete prepared result")
            })?;
            let mut row = custody.retry_publication()?;
            row.merge_runtime_from_reload(&ready.instance);
            Ok(TransactionEffect::Created {
                row: Box::new(row),
                hooks_ran: ready.on_launch_hooks_ran,
                warnings: ready.warnings,
                auto_attach: true,
            })
        }
        TransactionRequest::Archive {
            row,
            settled,
            reveal,
            successor,
        } => archive(row, settled, reveal, successor),
        TransactionRequest::Trash(row) => trash(row),
        TransactionRequest::Restore {
            row,
            owned_generation,
        } => {
            row.origin.storage.verify_profile_identity()?;
            // Existing restore helper takes its own full mutation fences and performs the canonical CAS.
            let outcome = super::operations::restore_from_trash_with_storage(
                &row.origin.storage,
                &row.before.id,
                owned_generation,
                &row.before,
            );
            Ok(TransactionEffect::Restored {
                id: row.before.id,
                outcome,
            })
        }
        TransactionRequest::DeleteGroup {
            profiles,
            path,
            restarting,
            options,
        } => delete_group(profiles, path, restarting, options),
    }
}

fn restart(
    capture: RowCapture,
    target: Option<Storage>,
    tool: Option<String>,
    extra: Option<String>,
    command: Option<String>,
    wake_snooze: bool,
    size: Option<(u16, u16)>,
) -> anyhow::Result<TransactionEffect> {
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    let _title = crate::session::acquire_session_title_lock(&capture.before.id)?;
    let _lifecycle = capture
        .origin
        .storage
        .acquire_instance_lifecycle_lock(&capture.before.id)?;
    let mut tx = RowTransaction::load(&capture, target, false)?;
    anyhow::ensure!(
        !matches!(
            tx.row.status,
            crate::session::Status::Creating | crate::session::Status::Deleting
        ) && !tx.row.is_archived()
            && !tx.row.is_trashed()
            && !tx.row.has_active_lifecycle_reservation(chrono::Utc::now()),
        "Session is not eligible for restart"
    );
    let before = tx.row.clone();
    let tool_swapped = tool.as_deref().is_some_and(|t| t != before.tool);
    let destination = tx
        .target
        .as_ref()
        .map(|s| s.profile())
        .unwrap_or(&before.source_profile);
    let (account_swap, mut carry) = match tool
        .as_deref()
        .filter(|_| tool_swapped)
        .map(|tool| conversation_carry::classify(&before, destination, tool))
    {
        Some(conversation_carry::ToolSwap::KeepConversation(carry)) => (true, carry),
        _ => (false, None),
    };
    let mut requested = before.clone();
    if wake_snooze {
        requested.unsnooze();
    }
    if let Some(tool) = tool.as_deref().filter(|_| tool_swapped) {
        if account_swap {
            requested.swap_account(tool);
        } else {
            requested.swap_tool(tool);
        }
    }
    if let Some(extra) = extra {
        requested.extra_args = extra;
    }
    if let Some(command) = command {
        requested.command = command;
    }
    requested.touch_last_accessed();
    if let Some(target) = tx
        .target
        .as_ref()
        .filter(|s| s.profile() != before.source_profile)
    {
        let profile = target.profile().to_owned();
        tx.move_to_profile_with_effect(
            &before.id,
            &profile,
            requested,
            Some(&before),
            account_swap,
            |_| Ok(()),
        )?;
    } else {
        // Swap against the authoritative row, not a stale TUI conversation snapshot.
        tx.row = tx.storage.update_under_workspace_claim_lock(|rows, _| {
            let disk = rows
                .iter_mut()
                .find(|r| r.id == before.id && r.created_at == before.created_at)
                .ok_or_else(|| anyhow::anyhow!("Original restart row disappeared"))?;
            anyhow::ensure!(
                disk.lifecycle_generation == before.lifecycle_generation,
                "Restart generation changed"
            );
            let ids = conversation_carry::conversation_ids(disk);
            if tool_swapped {
                if account_swap {
                    disk.swap_account(&requested.tool);
                } else {
                    disk.swap_tool(&requested.tool);
                }
            }
            disk.merge_user_action_diff(&before, &requested);
            if let Some(carry) = carry.as_mut() {
                carry.retarget(ids);
            }
            Ok(disk.clone())
        })?;
        tx.row.merge_runtime_from_reload(&requested);
    }
    if let Some(carry) = carry.as_mut() {
        carry.retarget(conversation_carry::conversation_ids(&tx.row));
    }
    let origin = RequestOrigin::capture(&tx.row)?;
    let wake_message = crate::session::resolve_config(&tx.row.source_profile)?
        .session
        .restart_wake_message
        .clone();
    Ok(TransactionEffect::Restart {
        origin,
        attach: false,
        request: Box::new(RestartRequest {
            session_id: tx.row.id.clone(),
            instance: tx.row,
            size,
            wake_message,
            skip_on_launch: false,
            bound_hooks: true,
            discard_sandbox_container: tool_swapped,
            conversation_carry: carry,
        }),
    })
}

fn rename(
    capture: RowCapture,
    target: Option<Storage>,
    new_title: &str,
    new_group: Option<&str>,
    rename_branch: bool,
    settled: Option<SettledEdit>,
) -> anyhow::Result<TransactionEffect> {
    let id = capture.before.id.clone();
    let live = &capture.before;
    let title_changed_by_user = !new_title.is_empty() && new_title != live.title;
    let _workspace_lock = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity_lock = crate::session::acquire_session_identity_lock()?;
    let _mutation_guards = (
        crate::session::acquire_session_title_lock(&id)?,
        capture
            .origin
            .storage
            .acquire_instance_lifecycle_lock(&id)?,
    );
    let mut tx = RowTransaction::load(&capture, target, settled.is_some())?;
    let target_name = tx.target.as_ref().map(|s| s.profile().to_owned());
    let new_profile = target_name.as_deref();
    let previous = tx
        .get_instance(&id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
    let current_profile = previous.source_profile.clone();
    let current_title = previous.title.clone();
    let current_group = previous.group_path.clone();

    // Empty or dialog-unchanged text means preserve the authoritative
    // source title, never the snapshot captured before the locks.
    let effective_title = if !title_changed_by_user {
        current_title.clone()
    } else {
        new_title.to_string()
    };
    let effective_group = match new_group {
        None => current_group.clone(),
        Some(group) => group.to_string(),
    };

    let target_profile = new_profile.unwrap_or(&current_profile);
    if target_profile != current_profile {
        let profiles = list_profiles()?;
        if !profiles.contains(&target_profile.to_string()) {
            anyhow::bail!("Profile '{}' does not exist", target_profile);
        }
    }

    let tied = tx.tie_workdir_applies_for(&id);
    let tied_edit = tied && (current_title != effective_title || rename_branch);
    let duplicate_path = if tied_edit {
        crate::session::worktree_edit::derived_worktree_path(
            std::path::Path::new(&previous.project_path),
            &effective_title,
        )
    } else {
        previous.project_path.clone()
    };
    let pair_changed = current_title != effective_title
        || target_profile != current_profile
        || duplicate_path.trim_end_matches('/') != previous.project_path.trim_end_matches('/');
    if pair_changed {
        let candidates = tx.target.as_ref().unwrap_or(tx.storage.as_ref()).load()?;
        if is_duplicate_session(
            candidates.iter(),
            &effective_title,
            &duplicate_path,
            Some(&id),
        ) {
            let error = duplicate_session_error(&effective_title);
            return Err(error);
        }
    }

    // Tied mode (#1927): a worktree session's directory leaf follows its title,
    // so move the directory in lockstep before persisting the new title. Gated on
    // a stopped session; a running one warns and renames nothing.
    let mut new_path: Option<String> = None;
    let mut new_branch: Option<String> = None;
    // Fire when the title changed (dir follows it) OR the user opted to
    let mut current_instance = tx
        .get_instance(&id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
    let cross_profile_target = new_profile
        .filter(|target| *target != current_instance.source_profile.as_str())
        .map(str::to_string);
    let mut projected_move = current_instance.clone();
    projected_move.title = effective_title.clone();
    projected_move.group_path = effective_group.clone();

    if let Some(target_profile) = cross_profile_target.as_deref() {
        let profiles = list_profiles()?;
        if !profiles.contains(&target_profile.to_string()) {
            anyhow::bail!("Profile '{}' does not exist", target_profile);
        }
        if (current_title != effective_title || rename_branch) && tx.tie_workdir_applies_for(&id) {
            let leaf = crate::session::worktree_edit::worktree_leaf_from_title(&effective_title);
            if let Some(path) = crate::session::worktree_edit::target_worktree_path(
                std::path::Path::new(&projected_move.project_path),
                &leaf,
            ) {
                projected_move.project_path = path.to_string_lossy().to_string();
            }
            if rename_branch {
                if let Some(worktree) = projected_move.worktree_info.as_mut() {
                    worktree.branch = crate::session::builder::git_sanitize_branch_name(&leaf);
                }
            }
        }

        // Advisory preflight before any worktree, container, or branch
        // effect. The dual-locked transaction repeats this check.
        let target_storage = tx
            .target
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Original target storage not captured"))?;
        let target_rows = target_storage.load()?;
        if is_duplicate_session(
            target_rows.iter(),
            &projected_move.title,
            &projected_move.project_path,
            None,
        ) {
            return Err(duplicate_session_error(&projected_move.title));
        }
    }
    let real_move = tied_edit
        && previous.worktree_info.is_some()
        && crate::session::worktree_edit::worktree_move_required(
            std::path::Path::new(&previous.project_path),
            &crate::session::worktree_edit::worktree_leaf_from_title(&effective_title),
        );
    if let Some(proof) = settled {
        let generation = proof.consume_under_locks(&previous)?;
        tx.mutate_instance(&id, |row| {
            row.release_lifecycle_reservation_if_owned(
                crate::session::LifecycleOperation::Stop,
                generation,
            );
        });
        current_instance.release_lifecycle_reservation_if_owned(
            crate::session::LifecycleOperation::Stop,
            generation,
        );
        projected_move.release_lifecycle_reservation_if_owned(
            crate::session::LifecycleOperation::Stop,
            generation,
        );
    } else if real_move {
        anyhow::ensure!(
            !previous.status.blocks_worktree_edit(),
            "Stop the session before moving its checkout"
        );
        drop(_mutation_guards);
        drop(_identity_lock);
        drop(_workspace_lock);
        return settlement(
            previous,
            crate::tui::stop_poller::SettlementAction::Rename {
                title: new_title.to_string(),
                group: new_group.map(str::to_string),
                profile: new_profile.map(str::to_string),
                rename_branch,
            },
        );
    }
    // Fire when the title changed (the dir follows it) or the user asked to
    // rename the branch, which is allowed even with the title unchanged.
    if tied_edit && cross_profile_target.is_none() {
        let snapshot = tx.get_instance(&id).map(|i| {
            (
                i.worktree_info.clone(),
                i.status,
                i.project_path.clone(),
                i.is_sandboxed(),
            )
        });
        if let Some((Some(worktree_info), status, project_path, is_sandboxed)) = snapshot {
            let leaf = crate::session::worktree_edit::worktree_leaf_from_title(&effective_title);
            let container_holds_worktree = !status.blocks_worktree_edit()
                && crate::session::worktree_edit::worktree_move_required(
                    std::path::Path::new(&project_path),
                    &leaf,
                )
                && crate::session::worktree_edit::ensure_sandbox_container_released(
                    &id,
                    is_sandboxed,
                );
            if let Some(reason) =
                worktree_rename_block(status, is_sandboxed, container_holds_worktree)
            {
                anyhow::bail!("{}", worktree_rename_block_message(&reason));
            }
            current_instance
                .original_storage()?
                .ensure_worktree_edit_unclaimed_under_workspace_lock(
                    &id,
                    std::path::Path::new(&project_path),
                    &leaf,
                )?;
            match crate::session::worktree_edit::edit_worktree_workdir(
                crate::session::worktree_edit::WorktreeEditRequest {
                    worktree_info: &worktree_info,
                    current_path: std::path::Path::new(&project_path),
                    new_name: &leaf,
                    rename_branch,
                },
            ) {
                Ok(outcome) => {
                    let dir_moved = outcome.new_path != std::path::Path::new(&project_path);
                    new_path = Some(outcome.new_path.to_string_lossy().to_string());
                    new_branch = outcome.new_branch;
                    if dir_moved {
                        crate::session::worktree_edit::discard_sandbox_container_after_move(
                            &id,
                            is_sandboxed,
                        );
                    }
                }
                Err(crate::session::worktree_edit::WorktreeEditError::Unchanged) => {}
                Err(e) => {
                    anyhow::bail!("Could not move the worktree directory: {e}");
                }
            }
        }
    }

    let rekey_target = crate::tmux::capture_rekey_session(&id, &current_title);
    // Cross-profile worktree and container effects run inside the dual-profile
    // transaction; tmux rekeying waits until persistence and publication succeed.
    if let Some(target_profile) = cross_profile_target.as_deref() {
        let tied_edit =
            (current_title != effective_title || rename_branch) && tx.tie_workdir_applies_for(&id);
        let effect_instance = current_instance.clone();
        let effect_id = id.clone();
        let effect_title = effective_title.clone();
        tx.move_to_profile_with_effect(
            &id,
            target_profile,
            projected_move,
            Some(&current_instance),
            false,
            move |candidate| {
        if tied_edit {
            if let Some(worktree_info) = effect_instance.worktree_info.as_ref() {
                let leaf = crate::session::worktree_edit::worktree_leaf_from_title(
            &effect_title,
                );
                let container_holds_worktree =
            !candidate.status.blocks_worktree_edit()
                && crate::session::worktree_edit::worktree_move_required(
                    std::path::Path::new(
                &effect_instance.project_path,
                    ),
                    &leaf,
                )
                && crate::session::worktree_edit::ensure_sandbox_container_released(
                    &effect_id,
                    candidate.is_sandboxed(),
                );
                if let Some(reason) = worktree_rename_block(
            candidate.status,
            candidate.is_sandboxed(),
            container_holds_worktree,
                ) {
            anyhow::bail!("{}", worktree_rename_block_message(&reason));
                }
                effect_instance.original_storage()?.ensure_worktree_edit_unclaimed_under_workspace_lock(
            &effect_id, std::path::Path::new(&effect_instance.project_path), &leaf)?;
                match crate::session::worktree_edit::edit_worktree_workdir(
            crate::session::worktree_edit::WorktreeEditRequest {
                worktree_info,
                current_path: std::path::Path::new(
                    &effect_instance.project_path,
                ),
                new_name: &leaf,
                rename_branch,
            },
                ) {
            Ok(outcome) => {
                // Both sides derive the leaf from the same
                // title through `target_worktree_path`, so the
                // published row and the directory that moved must
                // agree; assert it so a drift in either sanitizer
                // fails loudly instead of stranding the row.
                debug_assert_eq!(
                    outcome.new_path,
                    std::path::Path::new(&candidate.project_path),
                    "published project_path must match the moved worktree directory"
                );
                if outcome.new_path
                    != std::path::Path::new(&effect_instance.project_path)
                {
                    crate::session::worktree_edit::discard_sandbox_container_after_move(
                &effect_id,
                candidate.is_sandboxed(),
                    );
                }
            }
            Err(crate::session::worktree_edit::WorktreeEditError::Unchanged) => {}
            Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
            },
        )?;
        let tmux_warning =
            rekey_tmux_after_persist(&id, &current_title, &effective_title, rekey_target);
        return Ok(TransactionEffect::Edited {
            rows: vec![tx.row],
            warning: tmux_warning,
        });
    }

    tx.apply_user_action_under_workspace_claim_lock(&id, |inst| {
        inst.title = effective_title.clone();
        inst.group_path = effective_group.clone();
        if let Some(path) = &new_path {
            inst.project_path = path.clone();
        }
        if let Some(branch) = &new_branch {
            if let Some(wt) = inst.worktree_info.as_mut() {
                wt.branch = branch.clone();
            }
        }
    })?;
    let tmux_warning =
        rekey_tmux_after_persist(&id, &current_title, &effective_title, rekey_target);
    Ok(TransactionEffect::Edited {
        rows: vec![tx.row],
        warning: tmux_warning,
    })
}

fn workdir(
    capture: RowCapture,
    new_name: &str,
    rename_branch: bool,
    settled: Option<SettledEdit>,
) -> anyhow::Result<TransactionEffect> {
    let id = capture.before.id.clone();
    let _workspace_lock = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity_lock = crate::session::acquire_session_identity_lock()?;
    let _title_lock = crate::session::acquire_session_title_lock(&id)?;
    let _lifecycle_lock = capture
        .origin
        .storage
        .acquire_instance_lifecycle_lock(&id)?;
    let mut tx = RowTransaction::load(&capture, None, settled.is_some())?;
    let authoritative = tx.row.clone();
    let storage = tx.storage.clone();
    let authoritative_instances = storage.load()?;
    let worktree_info = authoritative.worktree_info.clone();
    let status = authoritative.status;
    let project_path = authoritative.project_path.clone();
    let is_sandboxed = authoritative.is_sandboxed();
    let Some(worktree_info) = worktree_info else {
        anyhow::bail!("Session does not use a worktree");
    };
    let duplicate_path = crate::session::worktree_edit::target_worktree_path(
        std::path::Path::new(&project_path),
        new_name,
    )
    .unwrap_or_else(|| std::path::PathBuf::from(&project_path))
    .to_string_lossy()
    .into_owned();
    if duplicate_path.trim_end_matches('/') != project_path.trim_end_matches('/')
        && is_duplicate_session(
            authoritative_instances.iter(),
            &authoritative.title,
            &duplicate_path,
            Some(&id),
        )
    {
        return Err(crate::session::duplicate_session_error(
            &authoritative.title,
        ));
    }
    if status.blocks_worktree_edit() {
        anyhow::bail!("Stop the session before editing its workdir name");
    }
    // A sandbox container stays up while Idle and bind-mounts the worktree, so the
    // move would hit EBUSY and a reused container would keep the old path; `status`
    // alone does not see this. Gated on the directory actually moving, since the
    // helper discards a stopped container. See #2117, #2414.
    if crate::session::worktree_edit::worktree_move_required(
        std::path::Path::new(&project_path),
        new_name,
    ) && crate::session::worktree_edit::ensure_sandbox_container_released(&id, is_sandboxed)
    {
        anyhow::bail!(
            "Stop the session before editing its workdir name: its sandbox container is \
                 mounting the worktree directory"
        );
    }
    if let Some(proof) = settled {
        let generation = proof.consume_under_locks(&authoritative)?;
        tx.mutate_instance(&id, |row| {
            row.release_lifecycle_reservation_if_owned(
                crate::session::LifecycleOperation::Stop,
                generation,
            );
        });
    } else if crate::session::worktree_edit::worktree_move_required(
        std::path::Path::new(&project_path),
        new_name,
    ) {
        drop(_lifecycle_lock);
        drop(_identity_lock);
        drop(_workspace_lock);
        return settlement(
            authoritative,
            crate::tui::stop_poller::SettlementAction::Workdir {
                name: new_name.to_string(),
                rename_branch,
            },
        );
    }

    storage.ensure_worktree_edit_unclaimed_under_workspace_lock(
        &id,
        std::path::Path::new(&project_path),
        new_name,
    )?;
    let outcome = crate::session::worktree_edit::edit_worktree_workdir(
        crate::session::worktree_edit::WorktreeEditRequest {
            worktree_info: &worktree_info,
            current_path: std::path::Path::new(&project_path),
            new_name,
            rename_branch,
        },
    )?;
    let new_path = outcome.new_path.to_string_lossy().to_string();
    let new_branch = outcome.new_branch.clone();

    // A container's mounts and working dir are baked in at create time and do not
    // follow a host-side `git worktree move`, so drop it and force a fresh create on
    // the next start. Only when the dir moved. Mirrors `rename_selected` (#2117).
    let dir_moved = outcome.new_path != std::path::Path::new(&project_path);
    if dir_moved {
        crate::session::worktree_edit::discard_sandbox_container_after_move(&id, is_sandboxed);
    }

    tx.apply_user_action_under_workspace_claim_lock(&id, |inst| {
        inst.project_path = new_path.clone();
        if let Some(branch) = &new_branch {
            if let Some(wt) = inst.worktree_info.as_mut() {
                wt.branch = branch.clone();
            }
        }
    })?;

    Ok(TransactionEffect::Edited {
        rows: vec![tx.row],
        warning: None,
    })
}

fn group(
    source: Storage,
    target: Option<Storage>,
    old_path: &str,
    new_path: &str,
    mut members: Vec<RowCapture>,
) -> anyhow::Result<TransactionEffect> {
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    source.verify_profile_identity()?;
    if let Some(target) = &target {
        target.verify_profile_identity()?;
    }
    members.sort_by(|a, b| a.before.id.cmp(&b.before.id));
    let mut guards = Vec::with_capacity(members.len());
    for member in &members {
        guards.push((
            crate::session::acquire_session_title_lock(&member.before.id)?,
            member
                .origin
                .storage
                .acquire_instance_lifecycle_lock(&member.before.id)?,
        ));
    }
    let (rows, groups) = source.load_with_groups()?;
    let prefix = format!("{old_path}/");
    let member_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.group_path == old_path || r.group_path.starts_with(&prefix))
        .collect();
    anyhow::ensure!(
        member_rows.len() == members.len(),
        "Group membership changed while mutation was pending"
    );
    let mut changes = Vec::with_capacity(members.len());
    for capture in &members {
        anyhow::ensure!(
            source.same_origin_as(&capture.origin.storage),
            "Group member original storage changed"
        );
        let mut before = member_rows
            .iter()
            .find(|r| r.id == capture.before.id)
            .map(|r| (*r).clone())
            .ok_or_else(|| anyhow::anyhow!("Group member disappeared"))?;
        capture.check(&before, false)?;
        anyhow::ensure!(
            !matches!(
                before.status,
                crate::session::Status::Creating | crate::session::Status::Deleting
            ) && !before.has_active_lifecycle_reservation(chrono::Utc::now()),
            "Cannot mutate group while member {} has a lifecycle operation in progress",
            before.id
        );
        before.merge_runtime_from_reload(&capture.before);
        let mut after = before.clone();
        after.group_path = if before.group_path == old_path {
            new_path.to_owned()
        } else {
            format!(
                "{new_path}/{}",
                before.group_path.strip_prefix(&prefix).unwrap()
            )
        };
        changes.push((before, after));
    }
    let mut moved = if let Some(target) = &target.filter(|s| s.profile() != source.profile()) {
        source.move_instances_to(
            target,
            &changes,
            &crate::session::GroupMovePlan::subtree(old_path, new_path),
            |existing, candidates| {
                for (index, row) in candidates.iter().enumerate() {
                    if crate::session::is_duplicate_session(
                        existing.iter(),
                        &row.title,
                        &row.project_path,
                        None,
                    ) || crate::session::is_duplicate_session(
                        candidates[..index].iter(),
                        &row.title,
                        &row.project_path,
                        None,
                    ) {
                        return Err(crate::session::duplicate_session_error(&row.title));
                    }
                }
                Ok(())
            },
        )?
    } else {
        anyhow::ensure!(
            old_path == new_path || !groups.iter().any(|g| g.path == new_path),
            "A group named '{new_path}' already exists"
        );
        source.update_under_workspace_claim_lock(|rows, groups| {
            for (before, after) in &changes {
                let stored = rows
                    .iter_mut()
                    .find(|r| r.id == before.id && r.created_at == before.created_at)
                    .ok_or_else(|| anyhow::anyhow!("Original group member disappeared"))?;
                stored.merge_user_action_diff(before, after);
            }
            let mut tree = GroupTree::new_with_groups(rows, groups);
            if old_path != new_path {
                tree.rename_group(old_path, new_path);
            }
            *groups = tree.get_all_groups();
            Ok(rows
                .iter()
                .filter(|r| changes.iter().any(|(b, _)| b.id == r.id))
                .cloned()
                .collect::<Vec<_>>())
        })?
    };
    for row in &mut moved {
        if let Some((before, _)) = changes.iter().find(|(b, _)| b.id == row.id) {
            row.merge_runtime_for_profile_move(before);
        }
    }
    Ok(TransactionEffect::Edited {
        rows: moved,
        warning: None,
    })
}

fn metadata(
    edits: Vec<MetadataEdit>,
    after: MetadataContinuation,
) -> anyhow::Result<TransactionEffect> {
    let mut by_profile: HashMap<String, Vec<MetadataEdit>> = HashMap::new();
    for edit in edits {
        by_profile
            .entry(edit.row.origin.storage.profile().to_owned())
            .or_default()
            .push(edit);
    }
    let mut committed = Vec::new();
    for edits in by_profile.into_values() {
        let storage = edits[0].row.origin.storage.clone();
        storage.verify_profile_identity()?;
        let ids: Vec<_> = edits
            .iter()
            .map(|edit| edit.row.before.id.clone())
            .collect();
        let rows = storage.update_metadata(
            crate::session::MetadataSelection::Sessions(std::borrow::Cow::Borrowed(&ids)),
            |rows, _| {
                let mut committed = Vec::new();
                for edit in &edits {
                    anyhow::ensure!(
                        storage.same_origin_as(&edit.row.origin.storage),
                        "Mixed original profile identities"
                    );
                    let row = rows
                        .iter_mut()
                        .find(|r| r.id == edit.row.before.id)
                        .ok_or_else(|| anyhow::anyhow!("Original metadata row disappeared"))?;
                    edit.row.check(row, false)?;
                    row.merge_user_action_diff(&edit.row.before, &edit.after);
                    committed.push(row.clone());
                }
                Ok(committed)
            },
        )?;
        committed.extend(rows);
    }
    Ok(TransactionEffect::MetadataCommitted {
        rows: committed,
        after,
    })
}
fn passive(capture: RowCapture, mark_unread: bool) -> anyhow::Result<TransactionEffect> {
    let patch = crate::session::PassiveStatusPatch::from_instance(&capture.before);
    let row = capture.origin.storage.update_metadata(
        crate::session::MetadataSelection::Session(std::borrow::Cow::Borrowed(&capture.before.id)),
        |rows, _| {
            let disk = rows
                .iter_mut()
                .find(|r| r.id == capture.before.id)
                .ok_or_else(|| anyhow::anyhow!("Original passive status row disappeared"))?;
            capture.check(disk, false)?;
            disk.merge_passive_status_patch(&capture.before.id, &patch);
            if mark_unread {
                disk.mark_unread();
            }
            Ok(disk.clone())
        },
    )?;
    Ok(TransactionEffect::Edited {
        rows: vec![row],
        warning: None,
    })
}

fn admit_creation(
    storage: Option<Storage>,
    request: CreationAdmission,
    file_watch: &Arc<crate::file_watch::FileWatchService>,
) -> anyhow::Result<TransactionEffect> {
    if request.cancel.is_cancelled() {
        return Ok(TransactionEffect::AdmissionCancelledBeforeEffect(
            request.admitted_instance.id,
        ));
    }
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    let storage = match storage {
        Some(original) => original.reopen_preserving_watch()?,
        None => {
            anyhow::ensure!(!crate::session::get_profile_dir_path(&request.data.profile)?.try_exists()?,"Creation profile appeared after its original absence was captured; no replacement profile was adopted");
            Storage::open_or_create(&request.data.profile, file_watch.clone())?
        }
    };
    let storage = Arc::new(storage);
    crate::session::builder::CreationCustody::register(
        storage.clone(),
        &request.admitted_instance,
    )?;
    Ok(TransactionEffect::Admitted(Box::new(
        crate::tui::creation_poller::CreationRequest {
            storage,
            admitted_instance: request.admitted_instance,
            data: request.data,
            existing_instances: request.existing_instances,
            hooks: request.hooks,
            cancel: request.cancel,
        },
    )))
}

fn archive(
    capture: RowCapture,
    settled: Option<SettledEdit>,
    reveal: bool,
    successor: Option<String>,
) -> anyhow::Result<TransactionEffect> {
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    let _title = crate::session::acquire_session_title_lock(&capture.before.id)?;
    let _lifecycle = capture
        .origin
        .storage
        .acquire_instance_lifecycle_lock(&capture.before.id)?;
    let mut tx = RowTransaction::load(&capture, None, settled.is_some())?;
    let Some(proof) = settled else {
        return settlement(tx.row, SettlementAction::Archive { reveal });
    };
    let generation = proof.consume_under_locks(&tx.row)?;
    tx.row.release_lifecycle_reservation_if_owned(
        crate::session::LifecycleOperation::Stop,
        generation,
    );
    tx.row.kill_all_tmux_sessions_locked();
    tx.apply_user_action_under_workspace_claim_lock(&capture.before.id, |r| r.archive())?;
    Ok(TransactionEffect::Archived {
        row: Box::new(tx.row),
        reveal,
        successor,
    })
}

fn trash(capture: RowCapture) -> anyhow::Result<TransactionEffect> {
    let id = capture.before.id.as_str();
    let _workspace_lock = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity_lock = crate::session::acquire_session_identity_lock()?;
    let storage = &*capture.origin.storage;
    storage.verify_profile_identity()?;
    let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(id)?;
    let mut request_instance = storage
        .load()?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| anyhow::anyhow!("Original session disappeared before trash"))?;
    capture.check(&request_instance, false)?;
    request_instance.merge_runtime_from_reload(&capture.before);
    if request_instance.has_managed_worktree_or_workspace() {
        let mut candidate_paths = vec![std::path::PathBuf::from(&request_instance.project_path)];
        if let Some(workspace) = &request_instance.workspace_info {
            candidate_paths.push(std::path::PathBuf::from(&workspace.workspace_dir));
        }
        candidate_paths.extend(
            request_instance
                .all_repos()
                .iter()
                .map(|repo| std::path::PathBuf::from(&repo.worktree_path)),
        );
        crate::session::deletion::ensure_unclaimed_paths(
            crate::session::deletion::SessionPathOwner {
                profile: storage.profile(),
                session_id: id,
            },
            &candidate_paths,
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    }

    let (generation, reservation, trashed_at) =
        storage.update_under_workspace_claim_lock(|instances, _groups| {
            let stored = instances
                .iter_mut()
                .find(|instance| instance.id == id)
                .ok_or_else(|| anyhow::anyhow!("session disappeared before trash"))?;
            let generation = stored
                .try_acquire_lifecycle_reservation(
                    LifecycleOperation::Trash,
                    crate::session::Instance::LIFECYCLE_RESERVATION_TTL,
                    chrono::Utc::now(),
                )
                .map_err(anyhow::Error::new)?;
            stored.trash();
            Ok((
                generation,
                stored.lifecycle_reservation.clone(),
                stored.trashed_at,
            ))
        })?;
    request_instance.trashed_at = trashed_at;
    request_instance.lifecycle_generation = generation;
    request_instance.lifecycle_reservation = reservation;
    Ok(TransactionEffect::Trash(Box::new(
        crate::session::trash::TrashRequest {
            storage: storage.clone(),
            session_id: id.to_owned(),
            instance: request_instance,
            generation,
        },
    )))
}

fn delete_group(
    profiles: Vec<Storage>,
    path: String,
    restarting: HashSet<String>,
    options: Option<GroupDeleteOptionsOwned>,
) -> anyhow::Result<TransactionEffect> {
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    let prefix = format!("{path}/");
    for storage in &profiles {
        storage.verify_profile_identity()?;
        for member in storage
            .load()?
            .iter()
            .filter(|r| r.group_path == path || r.group_path.starts_with(&prefix))
        {
            anyhow::ensure!(
                member.status != crate::session::Status::Creating,
                "A session in this group is still being created"
            );
            anyhow::ensure!(
                !restarting.contains(&member.id)
                    && !member.has_active_lifecycle_reservation(chrono::Utc::now()),
                "A session in this group has a lifecycle operation in progress"
            );
        }
    }
    let profile_names = profiles.iter().map(|s| s.profile().to_owned()).collect();
    let mut requests = Vec::new();
    for storage in profiles {
        let members = storage.update_under_workspace_claim_lock(|rows, groups| {
            let mut members = Vec::new();
            for row in rows
                .iter_mut()
                .filter(|r| r.group_path == path || r.group_path.starts_with(&prefix))
            {
                members.push(row.clone());
                row.group_path.clear();
            }
            groups.retain(|g| g.path != path && !g.path.starts_with(&prefix));
            Ok(members)
        })?;
        for instance in members {
            let Some(options) = &options else { continue };
            let managed = instance.has_managed_worktree_or_workspace();
            let sandbox = instance.sandbox_info.as_ref().is_some_and(|s| s.enabled);
            requests.push(crate::tui::deletion_poller::DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: options.worktrees && managed,
                delete_branch: options.branches && managed,
                delete_sandbox: options.containers && sandbox,
                force_delete: options.force_worktrees,
                detach_hooks: true,
                keep_scratch: false,
            });
        }
    }
    Ok(TransactionEffect::DeleteGroup {
        requests,
        profiles: profile_names,
        path,
    })
}

impl TransactionRequest {
    pub(super) fn try_for_each_captured_row(
        &self,
        mut f: impl FnMut(&RowCapture) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        match self {
            Self::PrepareClaimAbort(row) => f(row)?,
            Self::AttachProject { row, .. }
            | Self::StoreMove { row, .. }
            | Self::Launch { row, .. }
            | Self::Restart { row, .. }
            | Self::Rename { row, .. }
            | Self::Workdir { row, .. }
            | Self::Passive { row, .. }
            | Self::Archive { row, .. }
            | Self::Restore { row, .. }
            | Self::Trash(row)
            | Self::Ordering(OrderingRequest::Row { row, .. })
            | Self::ResolveCreation { row, .. } => f(row)?,
            #[cfg(test)]
            Self::Move { row, .. } => f(row)?,
            Self::Group { members, .. } => {
                for row in members {
                    f(row)?;
                }
            }
            Self::Metadata { edits, .. } => {
                for edit in edits {
                    f(&edit.row)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(super) fn try_for_each_captured_storage(
        &self,
        mut f: impl FnMut(&Storage) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.try_for_each_captured_row(|row| f(&row.origin.storage))?;
        match self {
            Self::AbortClaim(selection) => f(selection.origin())?,
            Self::Restart { target, .. }
            | Self::Rename { target, .. }
            | Self::Group { target, .. }
            | Self::SwitchProfile { target, .. }
            | Self::AdmitCreation {
                storage: target, ..
            } => {
                if let Some(target) = target {
                    f(target)?;
                }
            }
            #[cfg(test)]
            Self::Move { target, .. } => f(target)?,
            Self::PublishCreation { custody, .. } => f(custody.storage())?,
            Self::DeleteGroup { profiles, .. } => {
                for storage in profiles {
                    f(storage)?;
                }
            }
            Self::RecoverCreation(confirmation) => f(&confirmation.capture.storage)?,
            Self::DeleteProfile(storage)
            | Self::Ordering(OrderingRequest::Group { storage, .. }) => f(storage)?,
            _ => {}
        }
        if let Self::Group { source, .. } = self {
            f(source)?;
        }
        Ok(())
    }
}

pub(super) struct FailureContext {
    pub title: &'static str,
    pub creation: Option<String>,
    pub store_move: bool,
    pub metadata: Vec<(Instance, u64)>,
}
impl TransactionRequest {
    pub(super) fn failure_context(&self) -> FailureContext {
        let title = match self {
            Self::SetSortOrder(_) | Self::SetGroupBy(_) => {
                "Presentation not saved (current view retained)"
            }
            Self::DisableQuitConfirmation => "Quit confirmation preference not saved",
            Self::PrepareClaimAbort(_) | Self::AbortClaim(_) => "Intent metadata abort failed",
            Self::Restart { .. } | Self::Launch { .. } => "Restart Failed",
            Self::Rename { .. } => "Rename Failed",
            #[cfg(test)]
            Self::Move { .. } => "Rename Failed",
            Self::Workdir { .. } => "Edit Workdir Name Failed",
            Self::AttachProject { .. } => "Could Not Attach Project",
            Self::StoreMove { .. } => "Agent Store Move Failed",
            Self::AdmitCreation { .. }
            | Self::PublishCreation { .. }
            | Self::ResolveCreation { .. }
            | Self::RecoverCreation(_) => "Creation Failed",
            Self::SwitchProfile { .. } | Self::CreateProfile(_) | Self::DeleteProfile(_) => {
                "Profile transaction failed"
            }
            Self::Archive { .. } => "Session Edit Failed",
            Self::Trash(_) => "Trash Failed",
            Self::Restore { .. } => "Restore Failed",
            Self::Group { .. } | Self::DeleteGroup { .. } | Self::Ordering(_) => {
                "Group transaction failed"
            }
            Self::Metadata { .. } | Self::Passive { .. } => "Metadata transaction failed",
        };
        let creation = match self {
            Self::AdmitCreation { request, .. } => Some(request.admitted_instance.id.clone()),
            Self::PublishCreation { custody, .. } => Some(custody.session_id().to_owned()),
            Self::RecoverCreation(confirmation) => Some(confirmation.capture.id.clone()),
            _ => None,
        };
        FailureContext {
            title,
            creation,
            store_move: matches!(self, Self::StoreMove { .. }),
            metadata: match self {
                Self::Metadata { edits, .. } => edits
                    .iter()
                    .map(|e| ((*e.row.before).clone(), e.revision))
                    .collect(),
                _ => Vec::new(),
            },
        }
    }
}

pub(super) enum OrderingRequest {
    Row {
        row: RowCapture,
        delta: isize,
        direct: Option<String>,
        neighbour: Option<String>,
    },
    Group {
        storage: Storage,
        path: String,
        delta: isize,
        collapsed: HashMap<String, bool>,
    },
}
enum MoveOutcome {
    Moved(Vec<Instance>),
    AtEdge,
    Stale,
}
enum GroupMoveOutcome {
    Moved,
    AtEdge,
    Stale,
}
pub(super) fn group_contains(group: &str, member: &str) -> bool {
    member == group
        || member
            .strip_prefix(group)
            .is_some_and(|rest| rest.starts_with('/'))
}
fn order_row(
    row: &RowCapture,
    delta: isize,
    target_group: Option<String>,
) -> anyhow::Result<MoveOutcome> {
    let id = row.before.id.as_str();
    let profile = row.before.source_profile.clone();
    let source_group = row.before.group_path.clone();
    let storage = &row.origin.storage;
    let moved: Option<Vec<Instance>> = storage.update_metadata(
        crate::session::MetadataSelection::Ordering {
            anchor: std::borrow::Cow::Borrowed(id),
            source: std::borrow::Cow::Borrowed(&source_group),
            destination: std::borrow::Cow::Borrowed(
                target_group.as_deref().unwrap_or(&source_group),
            ),
        },
        |instances, groups| {
            if let Some(current) = instances.iter().find(|r| r.id == id) {
                row.check(current, false)?;
            }
            // The drawn row is a copy taken before the lock. If the store's row has since been
            // archived, trashed, deleted or regrouped, every position computed from the list is
            // about a layout that no longer exists.
            let anchor_is_current = instances.iter().any(|i| {
                i.id == id
                    && i.source_profile == profile
                    && i.group_path == source_group
                    && !i.is_archived()
                    && !i.is_trashed()
            });
            if !anchor_is_current {
                return Ok(None);
            }
            if let Some(destination) = &target_group {
                // The ungrouped bucket is always there; any other destination has to be a
                // group the store still holds, either declared or standing for live members.
                // Writing the membership blind would let the next tree rebuild synthesize a
                // group a peer has just deleted.
                let destination_exists = destination.is_empty()
                    || groups.iter().any(|g| g.path == *destination)
                    || instances.iter().any(|i| {
                        group_contains(destination, &i.group_path)
                            && i.source_profile == profile
                            && !i.is_archived()
                            && !i.is_trashed()
                    });
                if !destination_exists {
                    return Ok(None);
                }
            }
            let group = target_group.clone().unwrap_or_else(|| source_group.clone());
            let mut siblings: Vec<&Instance> = instances
                .iter()
                .filter(|i| {
                    i.group_path == group
                        && i.source_profile == profile
                        && !i.is_archived()
                        && !i.is_trashed()
                        && i.id != id
                })
                .collect();
            siblings.sort_by_key(|i| {
                (
                    i.sort_index.unwrap_or(u32::MAX),
                    std::cmp::Reverse(i.created_at),
                )
            });
            let mut order: Vec<String> = siblings.iter().map(|i| i.id.to_string()).collect();

            match &target_group {
                // Crossing a boundary: land against the edge that was crossed.
                Some(_) if delta < 0 => order.push(id.to_string()),
                Some(_) => order.insert(0, id.to_string()),
                None => {
                    // Within the group, the anchor's own position is read under the lock too.
                    let mut own = instances
                        .iter()
                        .filter(|i| {
                            i.group_path == group
                                && i.source_profile == profile
                                && !i.is_archived()
                                && !i.is_trashed()
                        })
                        .collect::<Vec<_>>();
                    own.sort_by_key(|i| {
                        (
                            i.sort_index.unwrap_or(u32::MAX),
                            std::cmp::Reverse(i.created_at),
                        )
                    });
                    let Some(at) = own.iter().position(|i| i.id == id) else {
                        return Ok(None);
                    };
                    let Some(to) = at.checked_add_signed(delta).filter(|t| *t < own.len()) else {
                        // A genuine edge, told apart from staleness by the anchor check above.
                        return Ok(Some(Vec::new()));
                    };
                    order = own.iter().map(|i| i.id.to_string()).collect();
                    order.swap(at, to);
                }
            }

            let mut applied = Vec::with_capacity(order.len());
            for (position, sibling) in order.iter().enumerate() {
                let position = position as u32;
                if let Some(row) = instances.iter_mut().find(|i| i.id == *sibling) {
                    row.sort_index = Some(position);
                    if row.id == id {
                        if let Some(group) = &target_group {
                            row.group_path = group.clone();
                        }
                    }
                    applied.push(row.clone());
                }
            }
            Ok(Some(applied))
        },
    )?;
    Ok(match moved {
        None => MoveOutcome::Stale,
        Some(rows) if rows.is_empty() => MoveOutcome::AtEdge,
        Some(rows) => MoveOutcome::Moved(rows),
    })
}
fn ordering(request: OrderingRequest) -> anyhow::Result<TransactionEffect> {
    match request {
        OrderingRequest::Row {
            row,
            delta,
            direct,
            neighbour,
        } => {
            row.origin.storage.verify_profile_identity()?;
            let mut destination = direct;
            let mut moved = order_row(&row, delta, destination.clone())?;
            if matches!(moved, MoveOutcome::AtEdge) && destination.is_none() {
                if let Some(next) = neighbour {
                    destination = Some(next.clone());
                    moved = order_row(&row, delta, Some(next))?;
                }
            }
            let mut warning = None;
            if matches!(moved, MoveOutcome::Moved(_)) {
                if let Some(target) = &destination {
                    let mut wanted = Vec::new();
                    let mut walk = target.as_str();
                    loop {
                        wanted.push(walk.to_owned());
                        if let Some((parent, _)) = walk.rsplit_once('/') {
                            if !parent.is_empty() {
                                walk = parent;
                                continue;
                            }
                        }
                        break;
                    }
                    if let Err(error) = row.origin.storage.update_metadata(
                        crate::session::MetadataSelection::Groups(std::borrow::Cow::Borrowed(
                            &wanted,
                        )),
                        |_, groups| {
                            for group in groups.iter_mut().filter(|g| wanted.contains(&g.path)) {
                                group.collapsed = false;
                            }
                            Ok(())
                        },
                    ) {
                        warning = Some(format!("{error:#}"));
                    }
                }
            }
            let stale = matches!(moved, MoveOutcome::Stale);
            let rows = match moved {
                MoveOutcome::Moved(rows) => rows,
                _ => Vec::new(),
            };
            Ok(TransactionEffect::Ordered {
                rows,
                id: Some(row.before.id),
                destination,
                warning,
                stale,
            })
        }
        OrderingRequest::Group {
            storage,
            path,
            delta,
            collapsed,
        } => {
            storage.verify_profile_identity()?;
            let group_path = path.as_str();
            let moved = storage.update_metadata(
                crate::session::MetadataSelection::GroupOrdering {
                    anchor: std::borrow::Cow::Borrowed(group_path),
                    overlays: &collapsed,
                },
                |instances, disk_groups| {
                    let mut tree = GroupTree::new_with_groups(instances, disk_groups);
                    if !tree.get_all_groups().iter().any(|g| g.path == group_path) {
                        // The header under the cursor was drawn from an older read; a peer has since
                        // deleted the group or emptied it.
                        return Ok(GroupMoveOutcome::Stale);
                    }
                    if !tree.move_group(group_path, delta) {
                        return Ok(GroupMoveOutcome::AtEdge);
                    }
                    let mut groups = tree.get_all_groups();
                    for g in &mut groups {
                        if let Some(state) = collapsed.get(&g.path) {
                            g.collapsed = *state;
                        }
                    }
                    *disk_groups = groups;
                    Ok(GroupMoveOutcome::Moved)
                },
            )?;
            let stale = matches!(moved, GroupMoveOutcome::Stale);
            Ok(TransactionEffect::Ordered {
                rows: Vec::new(),
                id: None,
                destination: None,
                warning: None,
                stale,
            })
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum CreationRecoveryAction {
    RetryPublication,
    Undo,
}
pub(super) struct CreationRecoveryCapture {
    custody: Arc<crate::session::builder::CreationCustody>,
    pub id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub generation: u64,
    pub storage: Arc<Storage>,
    cancel: Option<tokio_util::sync::CancellationToken>,
}
impl CreationRecoveryCapture {
    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.custody.matches_original(&self.storage,&self.id,self.created_at,self.generation)?,"Original Creating transaction changed after confirmation was opened; no action was retargeted");
        Ok(())
    }
}
pub(super) struct CreationConfirmation {
    pub capture: CreationRecoveryCapture,
    pub action: CreationRecoveryAction,
}

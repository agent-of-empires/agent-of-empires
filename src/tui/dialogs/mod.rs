//! TUI dialog components

mod changelog;
mod command_palette;
mod confirm;
mod context_menu;
mod custom_instruction;
mod delete_options;
mod group_delete_options;
mod group_picker;
mod hooks_install;
mod info;
mod intro;
mod new_session;
mod no_agents;
mod profile_picker;
mod project_session_picker;
mod projects;
mod rename;
mod repo_trust;
mod restart;
mod send_message;
#[cfg(feature = "serve")]
mod serve;
mod snooze_duration;
mod sort_picker;
mod telemetry_consent;
mod tool_picker;
mod update_confirm;
mod worktree_name;

pub use changelog::ChangelogDialog;
pub use command_palette::{
    builtin_commands, CommandPaletteDialog, PaletteAction, PaletteCommand, PaletteGroup,
};
pub use confirm::ConfirmDialog;
pub use context_menu::{ContextMenuAction, ContextMenuDialog};
pub use custom_instruction::CustomInstructionDialog;
pub use delete_options::{DeleteDialogConfig, DeleteOptions, UnifiedDeleteDialog};
pub use group_delete_options::{GroupDeleteOptions, GroupDeleteOptionsDialog};
pub use group_picker::GroupPickerDialog;
pub use hooks_install::HooksInstallDialog;
pub use info::InfoDialog;
pub use intro::{IntroDialog, IntroOutcome};
pub(crate) use new_session::project_picker_label;
pub use new_session::{NewSessionData, NewSessionDialog};
pub use no_agents::{NoAgentsAction, NoAgentsDialog};
pub use profile_picker::{ProfileEntry, ProfilePickerAction, ProfilePickerDialog};
pub use project_session_picker::ProjectSessionPickerDialog;
pub use projects::ProjectsDialog;
pub use rename::{RenameData, RenameDialog, RenameMode};
pub use repo_trust::{RepoTrustAction, RepoTrustDialog};
pub use restart::{RestartData, RestartDialog};
pub use send_message::SendMessageDialog;
#[cfg(feature = "serve")]
pub use serve::{ServeAction, ServeView};
pub use snooze_duration::SnoozeDurationDialog;
pub use sort_picker::SortPickerDialog;
pub use telemetry_consent::TelemetryConsentDialog;
pub use tool_picker::ToolPickerDialog;
pub use update_confirm::UpdateConfirmDialog;
pub use worktree_name::{WorktreeNameData, WorktreeNameDialog};

pub enum DialogResult<T> {
    Continue,
    Cancel,
    Submit(T),
}

/// Center a dialog of given size within an area, clamping to fit.
pub fn centered_rect(
    area: ratatui::layout::Rect,
    width: u16,
    height: u16,
) -> ratatui::layout::Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    ratatui::layout::Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

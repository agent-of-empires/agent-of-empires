//! TUI dialog components

use crossterm::event::KeyCode;
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear};

use crate::tui::styles::Theme;

pub mod attach_project;
mod changelog;
mod cheats;
mod command_palette;
mod confirm;
mod context_menu;
mod custom_instruction;
mod delete_options;
mod group_delete_options;
mod hooks_install;
mod info;
mod intro;
mod new_session;
mod no_agents;
mod option_picker;
mod permission_response;
mod plugin_manager;
mod profile_picker;
mod project_session_picker;
mod projects;
mod rename;
mod repo_trust;
mod restart;
mod send_message;
mod serve;
mod skills_manager;
mod snooze_duration;
mod telemetry_consent;
mod tips;
mod tool_picker;
mod update_confirm;
mod worktree_name;

pub use attach_project::AttachProjectDialog;
pub use changelog::ChangelogDialog;
pub use command_palette::{
    builtin_commands, CommandPaletteDialog, PaletteAction, PaletteCommand, PaletteGroup,
};
pub use confirm::ConfirmDialog;
pub use context_menu::{ContextMenuAction, ContextMenuDialog};
pub use custom_instruction::CustomInstructionDialog;
pub use delete_options::{DeleteDialogConfig, DeleteOptions, UnifiedDeleteDialog};
pub use group_delete_options::{GroupDeleteOptions, GroupDeleteOptionsDialog};
pub use hooks_install::HooksInstallDialog;
pub use info::InfoDialog;
pub use intro::{IntroDialog, IntroOutcome};
pub(crate) use new_session::project_picker_label;
pub use new_session::{NewSessionData, NewSessionDialog};
pub use no_agents::{NoAgentsAction, NoAgentsDialog};
pub use option_picker::{GroupPickerDialog, SortPickerDialog};
pub use permission_response::{PermissionResponseChoice, PermissionResponseDialog};
pub use plugin_manager::PluginManagerDialog;
pub use profile_picker::{ProfileEntry, ProfilePickerAction, ProfilePickerDialog};
pub use project_session_picker::ProjectSessionPickerDialog;
pub use projects::ProjectsDialog;
pub use rename::{RenameData, RenameDialog, RenameMode};
pub use repo_trust::{RepoTrustAction, RepoTrustDialog};
pub use restart::{RestartData, RestartDialog};
pub use send_message::SendMessageDialog;
pub(crate) use serve::start_local_daemon_and_wait;
pub use serve::{ServeAction, ServeView};
pub use skills_manager::SkillsManagerDialog;
pub use snooze_duration::SnoozeDurationDialog;
pub use telemetry_consent::TelemetryConsentDialog;
pub use tips::{TipsDialog, TipsOutcome};
pub use tool_picker::ToolPickerDialog;
pub use update_confirm::UpdateConfirmDialog;
pub use worktree_name::{WorktreeNameData, WorktreeNameDialog};

#[cfg_attr(test, derive(Debug, PartialEq))]
pub enum DialogResult<T> {
    Continue,
    Cancel,
    Submit(T),
}

/// Insert pasted text into a single-line `Input`, stripping newlines so a paste cannot submit.
pub fn paste_into_input(input: &mut tui_input::Input, text: &str) {
    for ch in text.chars().filter(|c| *c != '\n' && *c != '\r') {
        input.handle(tui_input::InputRequest::InsertChar(ch));
    }
}

/// Center a dialog of given size within an area, clamping to fit.
pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

pub fn contains(area: Rect, col: u16, row: u16) -> bool {
    area.contains(Position::from((col, row)))
}

/// Index of the one-line-per-item row under `(col, row)` in `list`, if any.
pub fn row_index(list: Rect, col: u16, row: u16, len: usize) -> Option<usize> {
    let idx = contains(list, col, row).then(|| (row - list.y) as usize)?;
    (idx < len).then_some(idx)
}

/// Key of the `(key, rect)` click target under `(col, row)`, if any.
pub fn hit<K: Copy>(targets: &[(K, Rect)], col: u16, row: u16) -> Option<K> {
    targets
        .iter()
        .find(|(_, rect)| contains(*rect, col, row))
        .map(|(key, _)| *key)
}

/// The rects of `(key, rect)` click targets, for a `HoverState`.
pub fn target_rects<K>(targets: &[(K, Rect)]) -> Vec<Rect> {
    targets.iter().map(|(_, rect)| *rect).collect()
}

/// Left edge of a `width`-cell line ratatui centers in `area`. Ratatui uses
/// `area.width / 2 - width / 2`, a cell off from `(area.width - width) / 2`
/// when the parities differ.
pub fn centered_x(area: Rect, width: u16) -> u16 {
    area.x + (area.width / 2).saturating_sub(width / 2)
}

/// Rounded, accent-bordered dialog block with a bold `theme.title` title.
pub fn dialog_block<'a>(title: impl Into<Line<'a>>, theme: &Theme) -> Block<'a> {
    toned_dialog_block(title, theme.accent, theme.title)
}

/// Rounded dialog block in an explicit tone: `border` frames it, `title_fg`
/// colors the bold title. Destructive dialogs pass `theme.error`.
pub fn toned_dialog_block<'a>(
    title: impl Into<Line<'a>>,
    border: Color,
    title_fg: Color,
) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(title)
        .title_style(Style::default().fg(title_fg).bold())
}

/// Clear a centered `width` x `height` area, draw `block` there, and return `(dialog, inner)`.
pub fn render_dialog_frame(
    frame: &mut Frame,
    area: Rect,
    width: u16,
    height: u16,
    block: Block,
) -> (Rect, Rect) {
    let dialog = centered_rect(area, width, height);
    frame.render_widget(Clear, dialog);
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    (dialog, inner)
}

/// Apply Up/k, Down/j, Home and End to a list cursor. Returns whether the key was a navigation key.
pub fn navigate_list(selected: &mut usize, len: usize, code: KeyCode) -> bool {
    match code {
        KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            if *selected + 1 < len {
                *selected += 1;
            }
        }
        KeyCode::Home => *selected = 0,
        KeyCode::End => *selected = len.saturating_sub(1),
        _ => return false,
    }
    true
}

/// Move `selected` to the hovered row; returns whether it changed.
pub fn hover_select(selected: &mut usize, hovered: Option<usize>) -> bool {
    match hovered {
        Some(idx) if idx != *selected => {
            *selected = idx;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod test_keys {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    pub fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    pub fn alt_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }
}

#[cfg(test)]
pub(crate) mod test_render {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::prelude::*;
    use ratatui::Terminal;

    use crate::tui::styles::{load_theme, Theme};

    /// Draw once at `width` x `height` and return the buffer.
    pub fn draw(width: u16, height: u16, render: impl FnOnce(&mut Frame, &Theme)) -> Buffer {
        let theme = load_theme("empire");
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, &theme)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Cell of the first occurrence of `needle`, for clicking what was drawn.
    /// Scans by cell, so wide glyphs earlier in the row don't skew the column.
    pub fn find(buf: &Buffer, needle: &str) -> (u16, u16) {
        let area = buf.area;
        (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .find(|&(x, y)| {
                let tail: String = (x..area.width).map(|c| buf[(c, y)].symbol()).collect();
                tail.starts_with(needle)
            })
            .unwrap_or_else(|| panic!("{needle:?} not drawn"))
    }
}

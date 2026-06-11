//! Small popup menu anchored at a screen position, used for right-click
//! context actions on the sidebar list (Rename / Delete).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::styles::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuAction {
    Rename,
    Delete,
    /// Archive (or unarchive) the session (mirrors the `'z'` hotkey). The menu
    /// label flips to "Unarchive" when the row is already archived.
    ToggleArchive,
    /// Snooze (or wake) the session (mirrors the `'h'` hotkey). The menu label
    /// flips to "Unsnooze" when the row is already snoozed. Snoozing an active
    /// row opens the duration picker; unsnoozing wakes it immediately.
    ToggleSnooze,
    /// Open the new-session dialog (mirrors the `'n'` hotkey).
    NewSession,
    /// Open the new-session dialog prefilled from the right-clicked
    /// project/group (mirrors `'N'` "new from selection" on a group),
    /// so the mouse path matches the web sidebar's per-project "+".
    NewFromGroup,
    /// Open the sort-order picker (mirrors `'o'`).
    OpenSortPicker,
    /// Open the group-by mode picker (mirrors `'g'`).
    OpenGroupPicker,
    /// Pin or unpin the project header (project view only; mirrors `'p'`). The
    /// menu label flips to "Unpin project" when the project is already pinned.
    TogglePin,
}

pub struct ContextMenuDialog {
    items: Vec<(ContextMenuAction, &'static str)>,
    selected: usize,
    /// Anchor where the popup's top-left corner wants to sit. The renderer
    /// clamps this into the visible area so a click near the bottom-right
    /// edge of the screen doesn't push the menu off-frame.
    anchor: (u16, u16),
    /// Last rendered rect, captured so click-outside detection can run
    /// without re-deriving the layout.
    last_area: Rect,
}

/// Resolve a `(col, row)` mouse position to the menu item index it
/// would hit, given the last rendered `area` and the number of items.
/// `None` for clicks on any border (top, bottom, or vertical), inside
/// the menu but past the last item, or anywhere outside the menu area.
/// All four border directions must be excluded; otherwise a click on
/// the right-hand vertical border at an item's row would dispatch
/// Rename/Delete the same as clicking the item text, which is not
/// what the user intends when they target the border.
fn row_to_item_idx(area: Rect, items_len: usize, col: u16, row: u16) -> Option<usize> {
    if !area.contains(Position::from((col, row))) {
        return None;
    }
    let inner_x = area.x.saturating_add(1);
    let last_inner_x = area.right().saturating_sub(1);
    if col < inner_x || col >= last_inner_x {
        return None;
    }
    let inner_y = area.y.saturating_add(1);
    let last_item_y = inner_y.saturating_add(items_len as u16);
    if row < inner_y || row >= last_item_y {
        return None;
    }
    Some((row - inner_y) as usize)
}

impl ContextMenuDialog {
    /// Build the session row's menu. `snooze` is `None` when the Snooze entry
    /// should be hidden (the `'h'` keybinding is gated to Attention sort, so
    /// the mouse path matches: no Snooze row outside Attention sort), and
    /// `Some(is_snoozed)` when it should appear, with the label flipping to
    /// "Unsnooze" for an already-snoozed row.
    pub fn for_session(anchor: (u16, u16), is_archived: bool, snooze: Option<bool>) -> Self {
        let archive_label = if is_archived { "Unarchive" } else { "Archive" };
        let mut items = vec![
            (ContextMenuAction::Rename, "Rename"),
            (ContextMenuAction::ToggleArchive, archive_label),
        ];
        if let Some(is_snoozed) = snooze {
            let snooze_label = if is_snoozed { "Unsnooze" } else { "Snooze" };
            items.push((ContextMenuAction::ToggleSnooze, snooze_label));
        }
        items.push((ContextMenuAction::Delete, "Delete"));
        Self::new(anchor, items)
    }

    pub fn for_group(anchor: (u16, u16)) -> Self {
        Self::new(
            anchor,
            vec![
                (ContextMenuAction::NewFromGroup, "New Session"),
                (ContextMenuAction::Rename, "Rename Group"),
                (ContextMenuAction::Delete, "Delete Group"),
            ],
        )
    }

    /// Menu for a project header in project view. Project groups are
    /// automatic, so Rename/Delete don't apply (they'd only show the
    /// "Project groups are automatic" info dialog). It keeps the group menu's
    /// "New Session" (launch under this project) and adds the pin toggle so the
    /// project can persist without any sessions. The pin label flips based on
    /// the current pinned state.
    pub fn for_project_group(anchor: (u16, u16), is_pinned: bool) -> Self {
        let pin_label = if is_pinned {
            "Unpin project"
        } else {
            "Pin project"
        };
        Self::new(
            anchor,
            vec![
                (ContextMenuAction::NewFromGroup, "New Session"),
                (ContextMenuAction::TogglePin, pin_label),
            ],
        )
    }

    /// Menu shown when the user right-clicks the empty area of the
    /// sidebar (below the last session row, or in an empty list).
    /// Holds the entry points the user would otherwise have to reach
    /// via `'n'` / `'o'` / `'g'`, so the mouse-only path matches the
    /// keyboard.
    pub fn for_empty_sidebar(anchor: (u16, u16)) -> Self {
        Self::new(
            anchor,
            vec![
                (ContextMenuAction::NewSession, "New Session"),
                (ContextMenuAction::OpenSortPicker, "Change Sort"),
                (ContextMenuAction::OpenGroupPicker, "Change Grouping"),
            ],
        )
    }

    fn new(anchor: (u16, u16), items: Vec<(ContextMenuAction, &'static str)>) -> Self {
        Self {
            items,
            selected: 0,
            anchor,
            last_area: Rect::default(),
        }
    }

    pub fn selected_action(&self) -> ContextMenuAction {
        self.items[self.selected].0
    }

    /// Test-only accessor: returns the (action, label) pairs in order
    /// so cross-module tests can assert on which menu variant opened
    /// without spinning up a render. Not part of the runtime API.
    #[cfg(test)]
    pub fn items_for_test(&self) -> &[(ContextMenuAction, &'static str)] {
        &self.items
    }

    /// Test-only accessor: returns the area the menu rendered into
    /// last frame. Lets a cross-module test compute the row of a given
    /// item without re-deriving the layout math.
    #[cfg(test)]
    pub fn last_area_for_test(&self) -> Rect {
        self.last_area
    }

    /// Returns true when `(col, row)` falls outside the last rendered area.
    /// Lets the mouse router close the menu on any click that isn't on it,
    /// matching the sidebar's web behavior in `WorkspaceSidebar.tsx`.
    pub fn click_is_outside(&self, col: u16, row: u16) -> bool {
        !self.last_area.contains(Position::from((col, row)))
    }

    /// Route a left-click at `(col, row)` to the menu. Returns:
    ///   - `Some(Submit(action))` when the click lands on an item row,
    ///   - `Some(Continue)` when the click lands on the menu but not on
    ///     an item (e.g. the rounded border), so the menu stays open,
    ///   - `None` when the click is outside the menu area, so the caller
    ///     can close it or fall through to underlying handlers.
    ///
    /// Hover-style selection moves with the click first so a near-miss
    /// still tracks where the user pointed.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<ContextMenuAction>> {
        if !self.last_area.contains(Position::from((col, row))) {
            return None;
        }
        match row_to_item_idx(self.last_area, self.items.len(), col, row) {
            None => {
                // Click on top/bottom border or anywhere inside the menu
                // that isn't an item row. Keep the menu open so the user
                // can try again without re-opening it.
                Some(DialogResult::Continue)
            }
            Some(idx) => {
                self.selected = idx;
                Some(DialogResult::Submit(self.items[idx].0))
            }
        }
    }

    /// Move the selection (and thus the highlighted row) to whichever
    /// item the mouse is hovering, so the visual cue tracks the cursor
    /// the same way a desktop menu does. Returns true when the
    /// highlight actually changed, so the caller can skip a redraw on
    /// every pixel-level mouse twitch.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let Some(idx) = row_to_item_idx(self.last_area, self.items.len(), col, row) else {
            return false;
        };
        if self.selected == idx {
            return false;
        }
        self.selected = idx;
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<ContextMenuAction> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => DialogResult::Submit(self.items[self.selected].0),
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected == 0 {
                    self.selected = self.items.len() - 1;
                } else {
                    self.selected -= 1;
                }
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % self.items.len();
                DialogResult::Continue
            }
            // Quick-pick hotkeys mirror the underlying actions' home-view
            // bindings (r/d for Rename/Delete; n/o/g for New / Sort /
            // Grouping). The hotkey only fires when the corresponding
            // action is actually in the current menu's item list, so the
            // session menu's `r` doesn't accidentally fire on the
            // empty-sidebar menu (which has different items).
            KeyCode::Char(c) => {
                let action = match c {
                    'r' | 'R' => Some(ContextMenuAction::Rename),
                    'd' | 'D' => Some(ContextMenuAction::Delete),
                    'z' | 'Z' => Some(ContextMenuAction::ToggleArchive),
                    'h' | 'H' => Some(ContextMenuAction::ToggleSnooze),
                    // `n` opens a new session from whichever new-session entry
                    // the current menu carries: the group/project menu prefills
                    // from the row (NewFromGroup), the empty-sidebar menu opens
                    // a blank one (NewSession).
                    'n' | 'N' => {
                        if self
                            .items
                            .iter()
                            .any(|(item, _)| *item == ContextMenuAction::NewFromGroup)
                        {
                            Some(ContextMenuAction::NewFromGroup)
                        } else {
                            Some(ContextMenuAction::NewSession)
                        }
                    }
                    'o' | 'O' => Some(ContextMenuAction::OpenSortPicker),
                    'g' | 'G' => Some(ContextMenuAction::OpenGroupPicker),
                    'p' | 'P' => Some(ContextMenuAction::TogglePin),
                    _ => None,
                };
                match action {
                    Some(a) if self.items.iter().any(|(item, _)| *item == a) => {
                        DialogResult::Submit(a)
                    }
                    _ => DialogResult::Continue,
                }
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let label_width = self
            .items
            .iter()
            .map(|(_, label)| label.chars().count() as u16)
            .max()
            .unwrap_or(0);
        // Border columns (2) + horizontal Padding (2) + breathing
        // room for the selection chevron (2).
        let width = (label_width + 6).max(16);
        let height = self.items.len() as u16 + 2;

        let mut x = self.anchor.0;
        let mut y = self.anchor.1;
        if x + width > area.right() {
            x = area.right().saturating_sub(width);
        }
        if y + height > area.bottom() {
            y = area.bottom().saturating_sub(height);
        }
        x = x.max(area.x);
        y = y.max(area.y);
        let dialog_area = Rect {
            x,
            y,
            width: width.min(area.width),
            height: height.min(area.height),
        };
        self.last_area = dialog_area;

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
            .border_style(Style::default().fg(theme.accent));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let rows: Vec<Line> = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, (_, label))| {
                let style = if idx == self.selected {
                    Style::default()
                        .fg(theme.background)
                        .bg(theme.accent)
                        .bold()
                } else {
                    Style::default().fg(theme.text)
                };
                Line::from(format!(" {label} ")).style(style)
            })
            .collect();

        frame.render_widget(Paragraph::new(rows), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn session_menu_starts_on_rename() {
        let menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        assert_eq!(menu.selected_action(), ContextMenuAction::Rename);
    }

    #[test]
    fn down_then_enter_selects_toggle_archive() {
        // Rename -> ToggleArchive is one Down in the 3-item session menu.
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        assert!(matches!(
            menu.handle_key(key(KeyCode::Down)),
            DialogResult::Continue
        ));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::ToggleArchive)
        ));
    }

    #[test]
    fn down_twice_then_enter_selects_snooze() {
        // Rename -> Archive -> Snooze is two Downs in the 4-item session menu.
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::ToggleSnooze)
        ));
    }

    #[test]
    fn down_thrice_then_enter_selects_delete() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::Delete)
        ));
    }

    #[test]
    fn group_menu_offers_new_session_first() {
        let menu = ContextMenuDialog::for_group((0, 0));
        let items: Vec<(ContextMenuAction, &str)> = menu
            .items_for_test()
            .iter()
            .map(|(a, l)| (*a, *l))
            .collect();
        assert_eq!(
            items,
            vec![
                (ContextMenuAction::NewFromGroup, "New Session"),
                (ContextMenuAction::Rename, "Rename Group"),
                (ContextMenuAction::Delete, "Delete Group"),
            ]
        );
    }

    #[test]
    fn n_hotkey_in_group_menu_submits_new_from_group() {
        let mut menu = ContextMenuDialog::for_group((0, 0));
        // Pre-select Delete to prove the hotkey wins over the cursor.
        menu.handle_key(key(KeyCode::Up));
        let result = menu.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::NewFromGroup)
        ));
    }

    #[test]
    fn n_hotkey_in_empty_sidebar_menu_submits_new_session() {
        // The empty-sidebar menu carries the blank NewSession entry, so `n`
        // must resolve there and never to the group-scoped NewFromGroup.
        let mut menu = ContextMenuDialog::for_empty_sidebar((0, 0));
        let result = menu.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::NewSession)
        ));
    }

    #[test]
    fn archived_session_menu_labels_unarchive() {
        let menu = ContextMenuDialog::for_session((0, 0), true, Some(false));
        let labels: Vec<&str> = menu.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(labels, vec!["Rename", "Unarchive", "Snooze", "Delete"]);
    }

    #[test]
    fn snoozed_session_menu_labels_unsnooze() {
        let menu = ContextMenuDialog::for_session((0, 0), false, Some(true));
        let labels: Vec<&str> = menu.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(labels, vec!["Rename", "Archive", "Unsnooze", "Delete"]);
    }

    #[test]
    fn active_session_menu_lists_all_four_actions() {
        let menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        let items: Vec<ContextMenuAction> = menu.items_for_test().iter().map(|(a, _)| *a).collect();
        assert_eq!(
            items,
            vec![
                ContextMenuAction::Rename,
                ContextMenuAction::ToggleArchive,
                ContextMenuAction::ToggleSnooze,
                ContextMenuAction::Delete,
            ]
        );
    }

    #[test]
    fn h_hotkey_submits_toggle_snooze() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        let result = menu.handle_key(key(KeyCode::Char('h')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::ToggleSnooze)
        ));
    }

    #[test]
    fn snooze_none_hides_the_row_and_makes_h_inert() {
        // Outside Attention sort the caller passes `None`, so the menu drops to
        // the three always-available actions and `h` must not fire (it has no
        // Snooze item to resolve to), matching the Attention-gated keybinding.
        let mut menu = ContextMenuDialog::for_session((0, 0), false, None);
        let labels: Vec<&str> = menu.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(labels, vec!["Rename", "Archive", "Delete"]);
        assert!(matches!(
            menu.handle_key(key(KeyCode::Char('h'))),
            DialogResult::Continue
        ));
    }

    #[test]
    fn z_hotkey_submits_toggle_archive() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        let result = menu.handle_key(key(KeyCode::Char('z')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::ToggleArchive)
        ));
    }

    #[test]
    fn enter_on_default_submits_rename() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::Rename)
        ));
    }

    #[test]
    fn esc_cancels() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        let result = menu.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn up_wraps_from_first_to_last() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        menu.handle_key(key(KeyCode::Up));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::Delete)
        ));
    }

    #[test]
    fn down_wraps_from_last_to_first() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        // 4 items: Down x4 walks Rename -> Archive -> Snooze -> Delete -> back
        // to Rename.
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::Rename)
        ));
    }

    #[test]
    fn r_hotkey_submits_rename() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        // Pre-select Delete to prove the hotkey wins over the cursor.
        menu.handle_key(key(KeyCode::Down));
        let result = menu.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::Rename)
        ));
    }

    #[test]
    fn d_hotkey_submits_delete() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        let result = menu.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::Delete)
        ));
    }

    #[test]
    fn project_group_menu_keeps_new_session_and_flips_pin_label() {
        let unpinned = ContextMenuDialog::for_project_group((0, 0), false);
        let labels: Vec<&str> = unpinned.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(labels, vec!["New Session", "Pin project"]);

        let pinned = ContextMenuDialog::for_project_group((0, 0), true);
        let labels: Vec<&str> = pinned.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(labels, vec!["New Session", "Unpin project"]);
    }

    #[test]
    fn p_hotkey_submits_toggle_pin() {
        let mut menu = ContextMenuDialog::for_project_group((0, 0), false);
        let result = menu.handle_key(key(KeyCode::Char('p')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::TogglePin)
        ));
    }

    #[test]
    fn p_hotkey_inert_on_session_menu() {
        // The session menu has no pin entry, so `p` must not fire it.
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        let result = menu.handle_key(key(KeyCode::Char('p')));
        assert!(matches!(result, DialogResult::Continue));
    }

    #[test]
    fn unknown_key_is_continue() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false));
        let result = menu.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(result, DialogResult::Continue));
    }

    #[test]
    fn click_is_outside_before_render_is_true() {
        let menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        // Before a render captures `last_area`, every point should count
        // as "outside" so a stray click can't be mis-classified as "inside
        // the menu" and accidentally kept open.
        assert!(menu.click_is_outside(10, 10));
    }

    /// Stub last_area as if render had run, so click routing can be
    /// tested without spinning up a full Frame.
    fn stub_render(menu: &mut ContextMenuDialog, x: u16, y: u16, w: u16, h: u16) {
        menu.last_area = Rect::new(x, y, w, h);
    }

    #[test]
    fn click_on_first_row_submits_rename() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 4);
        // Item rows live inside the bordered block, so row y+1 is the
        // first item and y+2 is the second.
        let result = menu.handle_click(12, 11);
        assert!(matches!(
            result,
            Some(DialogResult::Submit(ContextMenuAction::Rename))
        ));
    }

    #[test]
    fn click_on_second_row_submits_toggle_archive() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 5);
        let result = menu.handle_click(12, 12);
        assert!(matches!(
            result,
            Some(DialogResult::Submit(ContextMenuAction::ToggleArchive))
        ));
    }

    #[test]
    fn click_on_third_row_submits_toggle_snooze() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 6);
        let result = menu.handle_click(12, 13);
        assert!(matches!(
            result,
            Some(DialogResult::Submit(ContextMenuAction::ToggleSnooze))
        ));
    }

    #[test]
    fn click_on_fourth_row_submits_delete() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 6);
        let result = menu.handle_click(12, 14);
        assert!(matches!(
            result,
            Some(DialogResult::Submit(ContextMenuAction::Delete))
        ));
    }

    #[test]
    fn click_on_border_keeps_menu_open() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 4);
        // Top border row is y itself.
        let result = menu.handle_click(12, 10);
        assert!(matches!(result, Some(DialogResult::Continue)));
    }

    #[test]
    fn click_on_vertical_border_at_item_row_does_not_dispatch() {
        // Regression: the left and right border columns sit on the
        // same row as items, so `row` alone can't distinguish "click
        // on item text" from "click on the border next to the item."
        // The router must reject both vertical borders or a click on
        // the right edge of the menu, at an item's y, would fire
        // Rename / Delete the same as clicking the label.
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 4);
        // (10, 11) = left vertical border, first item's row.
        assert!(matches!(
            menu.handle_click(10, 11),
            Some(DialogResult::Continue)
        ));
        // (23, 11) = right vertical border, first item's row
        // (area.right() - 1 with width 14 starting at x=10).
        assert!(matches!(
            menu.handle_click(23, 11),
            Some(DialogResult::Continue)
        ));
    }

    #[test]
    fn click_outside_returns_none() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 4);
        let result = menu.handle_click(40, 40);
        assert!(result.is_none());
    }

    #[test]
    fn hover_moves_highlight() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 5);
        assert_eq!(menu.selected_action(), ContextMenuAction::Rename);
        let changed = menu.handle_hover(12, 12);
        assert!(changed, "hover onto second row should change highlight");
        assert_eq!(menu.selected_action(), ContextMenuAction::ToggleArchive);
    }

    #[test]
    fn hover_on_same_row_returns_false() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 4);
        // First hover lands on row 1 (Rename, already selected).
        assert!(!menu.handle_hover(12, 11));
        // Same row again -> still no change.
        assert!(!menu.handle_hover(12, 11));
    }

    #[test]
    fn hover_off_menu_leaves_selection_alone() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false));
        stub_render(&mut menu, 10, 10, 14, 6);
        menu.handle_hover(12, 14); // Delete (fourth/last row)
        assert_eq!(menu.selected_action(), ContextMenuAction::Delete);
        assert!(!menu.handle_hover(40, 40));
        assert_eq!(
            menu.selected_action(),
            ContextMenuAction::Delete,
            "hover outside menu must not snap the highlight back"
        );
    }
}

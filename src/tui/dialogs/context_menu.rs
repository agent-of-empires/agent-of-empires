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
    /// Mark the session read or unread (mirrors the `'u'` hotkey). The menu
    /// label flips to "Mark read" when the row is already unread. Only shown
    /// when the unread feature is enabled.
    ToggleUnread,
    /// Open the new-session dialog (mirrors the `'n'` hotkey).
    NewSession,
    /// Open the new-session dialog prefilled from the right-clicked row
    /// (mirrors `'N'` "new from selection"): a session row inherits its
    /// repo path and group, a project/group row borrows a member's path,
    /// so the mouse path matches the web sidebar's per-project "+".
    NewFromSelection,
    /// Fork the right-clicked session into a new independent session that
    /// resumes its captured conversation (mirrors the palette "Fork session").
    Fork,
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
    /// Which item is highlighted, or `None` when the pointer is not over
    /// any item. Opens at `Some(0)` so the keyboard default and the
    /// "Enter submits the first item" behavior hold, but mouse hover can
    /// clear it to `None` so no row stays lit once the cursor leaves every
    /// item (a near-miss onto the border or off the menu entirely).
    highlight: Option<usize>,
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
    ///
    /// `unread` is `None` when the unread feature is disabled (no row shown),
    /// and `Some(is_unread)` when it should appear, with the label flipping to
    /// "Mark read" for an already-unread row. Unlike snooze, the unread toggle
    /// is always-on (not gated to Attention sort), so it shows in any sort.
    /// `can_fork` gates the "Fork session" row: it appears only when the
    /// session's agent can actually fork (a forkable terminal agent, or a
    /// structured agent advertising the ACP fork capability). A resume-only
    /// agent like `aoe-agent` omits the row, matching the palette action's
    /// refusal and the web sidebar's `acp_can_fork` gating.
    pub fn for_session(
        anchor: (u16, u16),
        is_archived: bool,
        snooze: Option<bool>,
        unread: Option<bool>,
        can_fork: bool,
    ) -> Self {
        let archive_label = if is_archived { "Unarchive" } else { "Archive" };
        let mut items = vec![
            (ContextMenuAction::NewFromSelection, "New Session"),
            (ContextMenuAction::Rename, "Rename"),
            (ContextMenuAction::ToggleArchive, archive_label),
        ];
        if let Some(is_snoozed) = snooze {
            let snooze_label = if is_snoozed { "Unsnooze" } else { "Snooze" };
            items.push((ContextMenuAction::ToggleSnooze, snooze_label));
        }
        if let Some(is_unread) = unread {
            let unread_label = if is_unread {
                "Mark read"
            } else {
                "Mark unread"
            };
            items.push((ContextMenuAction::ToggleUnread, unread_label));
        }
        items.push((ContextMenuAction::Delete, "Delete"));
        if can_fork {
            items.push((ContextMenuAction::Fork, "Fork session"));
        }
        Self::new(anchor, items)
    }

    pub fn for_group(anchor: (u16, u16)) -> Self {
        Self::new(
            anchor,
            vec![
                (ContextMenuAction::NewFromSelection, "New Session"),
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
                (ContextMenuAction::NewFromSelection, "New Session"),
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
            highlight: Some(0),
            anchor,
            last_area: Rect::default(),
        }
    }

    /// The action `Enter` would submit. Falls back to the first item when
    /// nothing is highlighted (e.g. mouse hover cleared it), mirroring the
    /// `Some(0)` open state, so the accessor is always total.
    pub fn selected_action(&self) -> ContextMenuAction {
        self.items[self.highlight.unwrap_or(0)].0
    }

    /// Test-only accessor: the currently highlighted item index, or
    /// `None` when the pointer left every item and nothing is lit.
    #[cfg(test)]
    pub fn highlight_for_test(&self) -> Option<usize> {
        self.highlight
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
                self.highlight = Some(idx);
                Some(DialogResult::Submit(self.items[idx].0))
            }
        }
    }

    /// Move the highlight to whichever item the mouse is hovering, so the
    /// visual cue tracks the cursor the same way a desktop menu does. When
    /// the cursor is not over any item (a border row/column, or off the
    /// menu entirely) the highlight clears to `None` so the last-hovered
    /// item doesn't stay lit once the pointer leaves it. Returns true when
    /// the highlight actually changed, so the caller can skip a redraw on
    /// every pixel-level mouse twitch.
    ///
    /// This intentionally diverges from the focus-style dialogs
    /// (`ConfirmDialog` and friends), which keep their selection when the
    /// mouse drifts off so a click still lands on a sensible default. A
    /// multi-row popup menu follows desktop semantics instead: drift off
    /// and nothing is armed, so an accidental near-miss can't leave a
    /// misleading row lit for `Enter`.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let next = row_to_item_idx(self.last_area, self.items.len(), col, row);
        if self.highlight == next {
            return false;
        }
        self.highlight = next;
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<ContextMenuAction> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            // Enter submits the highlighted item. If mouse hover cleared the
            // highlight (cursor left every item) there's nothing to submit,
            // so keep the menu open rather than firing a stale action.
            KeyCode::Enter => match self.highlight {
                Some(idx) => DialogResult::Submit(self.items[idx].0),
                None => DialogResult::Continue,
            },
            // Arrow keys always re-establish a concrete highlight, so the
            // keyboard works even after hover cleared it to `None`.
            KeyCode::Up | KeyCode::Char('k') => {
                let last = self.items.len() - 1;
                self.highlight = Some(match self.highlight {
                    None | Some(0) => last,
                    Some(idx) => idx - 1,
                });
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.highlight = Some(match self.highlight {
                    None => 0,
                    Some(idx) => (idx + 1) % self.items.len(),
                });
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
                    'u' | 'U' => Some(ContextMenuAction::ToggleUnread),
                    // `n` opens a new session from whichever new-session entry
                    // the current menu carries: the session/group/project menu
                    // prefills from the row (NewFromSelection), the empty-sidebar
                    // menu opens a blank one (NewSession).
                    'n' | 'N' => {
                        if self
                            .items
                            .iter()
                            .any(|(item, _)| *item == ContextMenuAction::NewFromSelection)
                        {
                            Some(ContextMenuAction::NewFromSelection)
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
                let style = if Some(idx) == self.highlight {
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
    fn session_menu_starts_on_new_session() {
        let menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        assert_eq!(menu.selected_action(), ContextMenuAction::NewFromSelection);
    }

    #[test]
    fn down_then_enter_selects_rename() {
        // NewFromSelection -> Rename is one Down in the session menu.
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        assert!(matches!(
            menu.handle_key(key(KeyCode::Down)),
            DialogResult::Continue
        ));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::Rename)
        ));
    }

    #[test]
    fn down_thrice_then_enter_selects_snooze() {
        // NewSession -> Rename -> Archive -> Snooze is three Downs in the
        // 5-item session menu.
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::ToggleSnooze)
        ));
    }

    #[test]
    fn down_four_times_then_enter_selects_delete() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        menu.handle_key(key(KeyCode::Down));
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
                (ContextMenuAction::NewFromSelection, "New Session"),
                (ContextMenuAction::Rename, "Rename Group"),
                (ContextMenuAction::Delete, "Delete Group"),
            ]
        );
    }

    #[test]
    fn n_hotkey_in_group_menu_submits_new_from_selection() {
        let mut menu = ContextMenuDialog::for_group((0, 0));
        // Pre-select Delete to prove the hotkey wins over the cursor.
        menu.handle_key(key(KeyCode::Up));
        let result = menu.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::NewFromSelection)
        ));
    }

    #[test]
    fn n_hotkey_in_empty_sidebar_menu_submits_new_session() {
        // The empty-sidebar menu carries the blank NewSession entry, so `n`
        // must resolve there and never to the row-scoped NewFromSelection.
        let mut menu = ContextMenuDialog::for_empty_sidebar((0, 0));
        let result = menu.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::NewSession)
        ));
    }

    #[test]
    fn archived_session_menu_labels_unarchive() {
        let menu = ContextMenuDialog::for_session((0, 0), true, Some(false), None, true);
        let labels: Vec<&str> = menu.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(
            labels,
            vec![
                "New Session",
                "Rename",
                "Unarchive",
                "Snooze",
                "Delete",
                "Fork session"
            ]
        );
    }

    #[test]
    fn snoozed_session_menu_labels_unsnooze() {
        let menu = ContextMenuDialog::for_session((0, 0), false, Some(true), None, true);
        let labels: Vec<&str> = menu.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(
            labels,
            vec![
                "New Session",
                "Rename",
                "Archive",
                "Unsnooze",
                "Delete",
                "Fork session"
            ]
        );
    }

    #[test]
    fn active_session_menu_lists_all_actions() {
        let menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        let items: Vec<ContextMenuAction> = menu.items_for_test().iter().map(|(a, _)| *a).collect();
        assert_eq!(
            items,
            vec![
                ContextMenuAction::NewFromSelection,
                ContextMenuAction::Rename,
                ContextMenuAction::ToggleArchive,
                ContextMenuAction::ToggleSnooze,
                ContextMenuAction::Delete,
                ContextMenuAction::Fork,
            ]
        );
    }

    #[test]
    fn n_hotkey_in_session_menu_submits_new_from_selection() {
        // The session menu carries the prefill-from-row entry, so `n`/`N` must
        // resolve to NewFromSelection, matching the `'N'` keybinding's
        // new-from-selection behavior on a session.
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        // Pre-select Fork (Up wraps to the last item) to prove the hotkey wins over the cursor.
        menu.handle_key(key(KeyCode::Up));
        let result = menu.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::NewFromSelection)
        ));
    }

    #[test]
    fn unread_row_appears_and_label_flips() {
        // Feature on, row read -> "Mark unread"; row unread -> "Mark read".
        let read = ContextMenuDialog::for_session((0, 0), false, None, Some(false), true);
        let labels: Vec<&str> = read.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(
            labels,
            vec![
                "New Session",
                "Rename",
                "Archive",
                "Mark unread",
                "Delete",
                "Fork session"
            ]
        );

        let unread = ContextMenuDialog::for_session((0, 0), false, None, Some(true), true);
        let labels: Vec<&str> = unread.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(
            labels,
            vec![
                "New Session",
                "Rename",
                "Archive",
                "Mark read",
                "Delete",
                "Fork session"
            ]
        );

        // Feature off (None) -> no unread row.
        let off = ContextMenuDialog::for_session((0, 0), false, None, None, true);
        let labels: Vec<&str> = off.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(
            labels,
            vec!["New Session", "Rename", "Archive", "Delete", "Fork session"]
        );
    }

    #[test]
    fn u_hotkey_submits_toggle_unread() {
        // The unread quick-pick is case-insensitive like the menu's other
        // hotkeys (the home-view shortcut itself is Shift+U), and only fires
        // when the Unread row is present (feature enabled).
        let mut menu = ContextMenuDialog::for_session((0, 0), false, None, Some(false), true);
        assert!(matches!(
            menu.handle_key(key(KeyCode::Char('u'))),
            DialogResult::Submit(ContextMenuAction::ToggleUnread)
        ));
        // With no Unread row (feature off), `u` is inert.
        let mut off = ContextMenuDialog::for_session((0, 0), false, None, None, true);
        assert!(matches!(
            off.handle_key(key(KeyCode::Char('u'))),
            DialogResult::Continue
        ));
    }

    #[test]
    fn h_hotkey_submits_toggle_snooze() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
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
        let mut menu = ContextMenuDialog::for_session((0, 0), false, None, None, true);
        let labels: Vec<&str> = menu.items_for_test().iter().map(|(_, l)| *l).collect();
        assert_eq!(
            labels,
            vec!["New Session", "Rename", "Archive", "Delete", "Fork session"]
        );
        assert!(matches!(
            menu.handle_key(key(KeyCode::Char('h'))),
            DialogResult::Continue
        ));
    }

    #[test]
    fn z_hotkey_submits_toggle_archive() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        let result = menu.handle_key(key(KeyCode::Char('z')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::ToggleArchive)
        ));
    }

    #[test]
    fn enter_on_default_submits_new_session() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::NewFromSelection)
        ));
    }

    #[test]
    fn esc_cancels() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        let result = menu.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn up_wraps_from_first_to_last() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        menu.handle_key(key(KeyCode::Up));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::Fork)
        ));
    }

    #[test]
    fn down_wraps_from_last_to_first() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        // 6 items: Down x6 walks NewSession -> Rename -> Archive -> Snooze ->
        // Delete -> Fork -> back to NewSession.
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        menu.handle_key(key(KeyCode::Down));
        let result = menu.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::NewFromSelection)
        ));
    }

    #[test]
    fn r_hotkey_submits_rename() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        // Pre-select Fork (Up wraps to the last item) to prove the hotkey
        // wins over the cursor.
        menu.handle_key(key(KeyCode::Up));
        let result = menu.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(
            result,
            DialogResult::Submit(ContextMenuAction::Rename)
        ));
    }

    #[test]
    fn d_hotkey_submits_delete() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
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
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        let result = menu.handle_key(key(KeyCode::Char('p')));
        assert!(matches!(result, DialogResult::Continue));
    }

    #[test]
    fn unknown_key_is_continue() {
        let mut menu = ContextMenuDialog::for_session((0, 0), false, Some(false), None, true);
        let result = menu.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(result, DialogResult::Continue));
    }

    #[test]
    fn click_is_outside_before_render_is_true() {
        let menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
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
    fn click_on_first_row_submits_new_session() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        // Item rows live inside the bordered block, so row y+1 is the
        // first item and y+2 is the second.
        let result = menu.handle_click(12, 11);
        assert!(matches!(
            result,
            Some(DialogResult::Submit(ContextMenuAction::NewFromSelection))
        ));
    }

    #[test]
    fn click_on_second_row_submits_rename() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        let result = menu.handle_click(12, 12);
        assert!(matches!(
            result,
            Some(DialogResult::Submit(ContextMenuAction::Rename))
        ));
    }

    #[test]
    fn click_on_third_row_submits_toggle_archive() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        let result = menu.handle_click(12, 13);
        assert!(matches!(
            result,
            Some(DialogResult::Submit(ContextMenuAction::ToggleArchive))
        ));
    }

    #[test]
    fn click_on_fourth_row_submits_toggle_snooze() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        let result = menu.handle_click(12, 14);
        assert!(matches!(
            result,
            Some(DialogResult::Submit(ContextMenuAction::ToggleSnooze))
        ));
    }

    #[test]
    fn click_on_border_keeps_menu_open() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
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
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
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
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 4);
        let result = menu.handle_click(40, 40);
        assert!(result.is_none());
    }

    #[test]
    fn hover_moves_highlight() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        assert_eq!(menu.selected_action(), ContextMenuAction::NewFromSelection);
        let changed = menu.handle_hover(12, 12);
        assert!(changed, "hover onto second row should change highlight");
        assert_eq!(menu.selected_action(), ContextMenuAction::Rename);
    }

    #[test]
    fn hover_on_same_row_returns_false() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 4);
        // First hover lands on row 1 (Rename, already selected).
        assert!(!menu.handle_hover(12, 11));
        // Same row again -> still no change.
        assert!(!menu.handle_hover(12, 11));
    }

    #[test]
    fn hover_off_menu_clears_the_highlight() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        menu.handle_hover(12, 15); // Delete (fifth row, index 4)
        assert_eq!(menu.highlight_for_test(), Some(4));
        // Moving the cursor off every item must drop the highlight so no
        // row stays lit once the pointer is no longer over any of them.
        assert!(menu.handle_hover(40, 40));
        assert_eq!(
            menu.highlight_for_test(),
            None,
            "hover off every item must clear the highlight, not keep the last one"
        );
        // A second move that is still off every item is a no-op redraw-wise.
        assert!(!menu.handle_hover(41, 41));
    }

    #[test]
    fn hover_onto_border_clears_the_highlight() {
        // Regression for the reported bug: hovering an item then sliding
        // onto the menu's own border (still inside `last_area`, but not an
        // item row) must unhighlight rather than leave the item lit.
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        menu.handle_hover(12, 12); // Rename (second row)
        assert_eq!(menu.highlight_for_test(), Some(1));
        // Top border row (y itself) is inside the menu but not an item.
        assert!(menu.handle_hover(12, 10));
        assert_eq!(menu.highlight_for_test(), None);
    }

    #[test]
    fn enter_after_hover_cleared_keeps_menu_open() {
        // With nothing highlighted, Enter has no item to submit, so the
        // menu stays open instead of firing a stale action.
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        menu.handle_hover(40, 40); // off every item -> None
        assert!(matches!(
            menu.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
    }

    #[test]
    fn arrow_down_after_hover_cleared_starts_at_first_item() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        menu.handle_hover(40, 40); // off every item -> None
        menu.handle_key(key(KeyCode::Down));
        assert_eq!(menu.selected_action(), ContextMenuAction::NewFromSelection);
    }

    #[test]
    fn arrow_up_after_hover_cleared_starts_at_last_item() {
        let mut menu = ContextMenuDialog::for_session((10, 10), false, Some(false), None, true);
        stub_render(&mut menu, 10, 10, 14, 7);
        menu.handle_hover(40, 40); // off every item -> None
        menu.handle_key(key(KeyCode::Up));
        assert_eq!(menu.selected_action(), ContextMenuAction::Fork);
    }
}

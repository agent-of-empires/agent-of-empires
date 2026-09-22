//! Directory picker overlay component

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::text_input::set_prefixed_input_cursor_position;
use crate::tui::styles::Theme;

pub enum DirPickerResult {
    Continue,
    Cancelled,
    Selected(String),
}

/// Where the picker lists directories from.
#[derive(Clone, Default)]
enum DirSource {
    #[default]
    Local,
    /// A remote daemon's home-scoped `/api/filesystem/browse`.
    Remote(Box<crate::daemon::DaemonClient>),
}

pub struct DirPicker {
    source: DirSource,
    active: bool,
    filter: Input,
    selected: usize,
    cwd: PathBuf,
    dirs: Vec<String>,
    /// Why the directory could not be listed.
    read_error: Option<String>,
    show_hidden: bool,
    show_help: bool,
    /// The remote listing in flight, if any. Replacing it drops the receiver,
    /// so a listing for a directory the user already left is discarded.
    remote_listing: Option<RemoteListing>,
}

type ListingResult = Result<Vec<String>, String>;

struct RemoteListing {
    dir: PathBuf,
    show_hidden: bool,
    rx: std::sync::mpsc::Receiver<ListingResult>,
}

/// Start listing a remote directory's subdirectories on a background thread
/// with a private runtime, so a slow daemon never stalls the TUI.
fn browse_remote(
    client: &crate::daemon::DaemonClient,
    dir: &std::path::Path,
    show_hidden: bool,
) -> std::sync::mpsc::Receiver<ListingResult> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        #[serde(default)]
        is_dir: bool,
    }
    #[derive(serde::Deserialize)]
    struct Listing {
        entries: Vec<Entry>,
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let client = client.clone();
    let path = dir.to_string_lossy().to_string();
    let spawned = std::thread::Builder::new()
        .name("aoe-remote-browse".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|runtime| {
                    runtime
                        .block_on(client.get_api::<Listing>(
                            &["filesystem", "browse"],
                            &[
                                ("path", path.as_str()),
                                ("limit", "1000"),
                                ("show_hidden", if show_hidden { "true" } else { "false" }),
                            ],
                        ))
                        .map(|listing| {
                            listing
                                .entries
                                .into_iter()
                                .filter(|entry| entry.is_dir)
                                .map(|entry| entry.name)
                                .collect()
                        })
                        .map_err(|e| e.summary())
                });
            let _ = tx.send(result);
        });
    if let Err(e) = spawned {
        tracing::debug!(target: "tui.dir_picker", "remote browse thread failed: {e}");
    }
    rx
}

impl Default for DirPicker {
    fn default() -> Self {
        Self::new()
    }
}

impl DirPicker {
    pub fn new() -> Self {
        Self {
            source: DirSource::Local,
            active: false,
            filter: Input::default(),
            selected: 0,
            cwd: PathBuf::new(),
            dirs: Vec::new(),
            read_error: None,
            show_hidden: false,
            show_help: false,
            remote_listing: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Browse a remote daemon's filesystem instead of this machine's. The
    /// daemon confines browsing to its home directory.
    pub fn activate_remote(&mut self, client: crate::daemon::DaemonClient, initial_path: &str) {
        self.source = DirSource::Remote(Box::new(client));
        self.cwd = PathBuf::from(if initial_path.is_empty() {
            "/"
        } else {
            initial_path
        });
        self.filter = Input::default();
        self.selected = 0;
        self.show_help = false;
        self.refresh_dirs();
        self.active = true;
    }

    pub fn activate(&mut self, initial_path: &str) {
        self.source = DirSource::Local;
        let path = if initial_path.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
        } else {
            let p = PathBuf::from(initial_path);
            if p.is_dir() {
                p
            } else {
                p.parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"))
            }
        };
        self.cwd = path;
        self.filter = Input::default();
        self.selected = 0;
        self.show_help = false;
        self.refresh_dirs();
        self.active = true;
    }

    /// Whether a remote listing is still in flight.
    pub fn is_loading(&self) -> bool {
        self.remote_listing.is_some()
    }

    /// Land a finished remote listing. Returns whether the picker changed.
    pub fn poll(&mut self) -> bool {
        let Some(listing) = &self.remote_listing else {
            return false;
        };
        let result = match listing.rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("remote browse ended without a listing".to_string())
            }
        };
        let (dir, show_hidden) = (listing.dir.clone(), listing.show_hidden);
        self.remote_listing = None;
        self.apply_listing(&dir, show_hidden, result)
    }

    fn apply_listing(
        &mut self,
        dir: &std::path::Path,
        show_hidden: bool,
        result: ListingResult,
    ) -> bool {
        if dir != self.cwd || show_hidden != self.show_hidden {
            return false;
        }
        self.set_listing(result);
        true
    }

    /// Show a listing from either source, sorted the same way.
    fn set_listing(&mut self, result: ListingResult) {
        match result {
            Ok(mut dirs) => {
                dirs.sort_by_key(|a| a.to_lowercase());
                self.read_error = None;
                self.dirs = dirs;
            }
            Err(error) => {
                self.read_error = Some(error);
                self.dirs = Vec::new();
            }
        }
    }

    fn refresh_dirs(&mut self) {
        if let DirSource::Remote(client) = &self.source {
            let rx = browse_remote(client, &self.cwd, self.show_hidden);
            self.remote_listing = Some(RemoteListing {
                dir: self.cwd.clone(),
                show_hidden: self.show_hidden,
                rx,
            });
            self.read_error = None;
            self.dirs = Vec::new();
            return;
        }
        self.remote_listing = None;
        let listing = std::fs::read_dir(&self.cwd)
            .map(|entries| {
                entries
                    .flatten()
                    // Follow symlinks: entry.path().is_dir() resolves symlinks,
                    // unlike entry.file_type().is_dir() which does not.
                    .filter(|entry| entry.path().is_dir())
                    .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                    .filter(|name| self.show_hidden || !name.starts_with('.'))
                    .collect()
            })
            .map_err(|_| "permission denied".to_string());
        self.set_listing(listing);
    }

    fn filtered_dirs(&self) -> Vec<String> {
        let filter = self.filter.value().to_lowercase();
        let has_parent = self.cwd.parent().is_some();

        let mut result = Vec::new();

        // "./" (select current directory) shown when filter is empty or matches "."
        if filter.is_empty() || ".".starts_with(&filter) {
            result.push("./".to_string());
        }

        if has_parent && (filter.is_empty() || "..".starts_with(&filter)) {
            result.push("../".to_string());
        }

        for d in &self.dirs {
            if filter.is_empty() || d.to_lowercase().contains(&filter) {
                result.push(d.clone());
            }
        }
        result
    }

    fn resolve_path(&self, name: &str) -> PathBuf {
        if name == "./" {
            self.cwd.clone()
        } else if name == "../" {
            self.cwd
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.cwd.clone())
        } else {
            self.cwd.join(name)
        }
    }

    /// Navigate into a directory: update cwd, clear filter, reset selection, refresh listing.
    fn navigate_to(&mut self, path: PathBuf) {
        self.cwd = path;
        self.filter = Input::default();
        self.selected = 0;
        self.refresh_dirs();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DirPickerResult {
        self.poll();
        if self.show_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.show_help = false;
            }
            return DirPickerResult::Continue;
        }

        if key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.show_hidden = !self.show_hidden;
            self.selected = 0;
            self.refresh_dirs();
            return DirPickerResult::Continue;
        }

        let filtered = self.filtered_dirs();
        let filtered_len = filtered.len();

        match key.code {
            KeyCode::Esc => {
                self.active = false;
                DirPickerResult::Cancelled
            }
            KeyCode::Enter | KeyCode::Right => {
                if filtered_len == 0 {
                    return DirPickerResult::Continue;
                }
                let idx = self.selected.min(filtered_len - 1);
                let name = &filtered[idx];
                if name == "./" {
                    // Select current directory and close picker
                    self.active = false;
                    DirPickerResult::Selected(self.cwd.to_string_lossy().to_string())
                } else {
                    // Navigate into directory (including ../)
                    let path = self.resolve_path(name);
                    self.navigate_to(path);
                    DirPickerResult::Continue
                }
            }
            KeyCode::Left => {
                if let Some(parent) = self.cwd.parent() {
                    self.navigate_to(parent.to_path_buf());
                }
                DirPickerResult::Continue
            }
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                DirPickerResult::Continue
            }
            KeyCode::Down => {
                if filtered_len > 0 && self.selected < filtered_len - 1 {
                    self.selected += 1;
                }
                DirPickerResult::Continue
            }
            KeyCode::Backspace => {
                if self.filter.value().is_empty() {
                    if let Some(parent) = self.cwd.parent() {
                        self.navigate_to(parent.to_path_buf());
                    }
                } else {
                    self.filter.handle_event(&crossterm::event::Event::Key(key));
                    self.selected = 0;
                }
                DirPickerResult::Continue
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                DirPickerResult::Continue
            }
            KeyCode::Char(_) => {
                self.filter.handle_event(&crossterm::event::Event::Key(key));
                self.selected = 0;
                DirPickerResult::Continue
            }
            _ => DirPickerResult::Continue,
        }
    }

    /// Truncate a path display string from the left to fit within max_len characters,
    /// prefixing with "..." when truncated.
    fn truncate_path(path: &str, max_len: usize) -> String {
        let char_count = path.chars().count();
        if char_count <= max_len {
            return path.to_string();
        }
        let ellipsis = "...";
        let ellipsis_len = ellipsis.len(); // 3, all ASCII
        let available = max_len.saturating_sub(ellipsis_len);
        if available == 0 {
            return ellipsis.chars().take(max_len).collect();
        }
        // Take `available` characters from the right end of the path
        let skip = char_count - available;
        let tail: String = path.chars().skip(skip).collect();
        format!("{}{}", ellipsis, tail)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let filtered = self.filtered_dirs();
        let max_visible: usize = 10;
        let list_height = filtered.len().min(max_visible) as u16;
        // filter input (1) + spacer (1) + list + hint (1) + borders (2) + margin (2)
        let dialog_height = (list_height + 7).min(area.height);
        let dialog_width: u16 = 60.min(area.width.saturating_sub(4));

        let dialog_area = crate::tui::dialogs::centered_rect(area, dialog_width, dialog_height);
        frame.render_widget(Clear, dialog_area);

        // " Browse: <path> " with border chars leaves dialog_width - 2 for content,
        // and the "Browse: " prefix + spaces take 10 chars.
        let max_path_len = (dialog_width as usize).saturating_sub(12);
        let path_display = Self::truncate_path(&self.cwd.to_string_lossy(), max_path_len);
        let title = format!(" Browse: {} ", path_display);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(title)
            .title_style(Style::default().fg(theme.title).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // filter input
                Constraint::Length(1), // spacer
                Constraint::Min(1),    // list
                Constraint::Length(1), // hint
            ])
            .split(inner);

        // Filter input
        let filter_value = self.filter.value();
        let filter_line = Line::from(vec![
            Span::styled("Filter: ", Style::default().fg(theme.text)),
            Span::styled(filter_value, Style::default().fg(theme.accent).bold()),
            Span::styled("_", Style::default().fg(theme.accent)),
        ]);
        frame.render_widget(Paragraph::new(filter_line), chunks[0]);
        set_prefixed_input_cursor_position(frame, chunks[0], "Filter: ", &self.filter);

        let visible_height = chunks[2].height as usize;
        let scroll = super::scroll::calculate_scroll(filtered.len(), self.selected, visible_height);

        let mut lines: Vec<Line> = Vec::new();
        if let Some(error) = &self.read_error {
            lines.push(Line::from(Span::styled(
                format!("  ({error})"),
                Style::default().fg(theme.dimmed),
            )));
        } else if self.is_loading() && self.dirs.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (loading…)",
                Style::default().fg(theme.dimmed),
            )));
        } else if filtered.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (empty directory)",
                Style::default().fg(theme.dimmed),
            )));
        } else {
            if scroll.has_more_above {
                lines.push(Line::from(Span::styled(
                    format!("  [{} more above]", scroll.scroll_offset),
                    Style::default().fg(theme.dimmed),
                )));
            }

            for (i, item) in filtered
                .iter()
                .skip(scroll.scroll_offset)
                .take(scroll.list_visible)
                .enumerate()
            {
                let abs_idx = i + scroll.scroll_offset;
                let is_selected = abs_idx == self.selected;
                let prefix = if is_selected { "> " } else { "  " };
                let style = if is_selected {
                    Style::default().fg(theme.accent).bold()
                } else {
                    Style::default().fg(theme.text)
                };
                let display = if item == "./" || item == "../" {
                    item.clone()
                } else {
                    format!("{}/", item)
                };
                lines.push(Line::from(Span::styled(
                    format!("{}{}", prefix, display),
                    style,
                )));
            }

            if scroll.has_more_below {
                let remaining = filtered.len() - scroll.scroll_offset - scroll.list_visible;
                lines.push(Line::from(Span::styled(
                    format!("  [{} more below]", remaining),
                    Style::default().fg(theme.dimmed),
                )));
            }
        }
        frame.render_widget(Paragraph::new(lines), chunks[2]);

        // Hint line
        let hint_line = Line::from(vec![
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" open/select  "),
            Span::styled("\u{2190}", Style::default().fg(theme.hint)),
            Span::raw(" back  "),
            Span::styled("?", Style::default().fg(theme.hint)),
            Span::raw(" help  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" cancel"),
        ]);
        frame.render_widget(Paragraph::new(hint_line), chunks[3]);

        if self.show_help {
            self.render_help_overlay(frame, area, theme);
        }
    }

    fn render_help_overlay(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_width: u16 = 50;
        let dialog_height: u16 = 16;

        let dialog_area = crate::tui::dialogs::centered_rect(area, dialog_width, dialog_height);
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .title(" Browse Help ")
            .title_style(Style::default().fg(theme.title).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let bindings: &[(&str, &str)] = &[
            ("Enter / \u{2192}", "Open directory"),
            ("Enter on ./", "Select current directory"),
            ("\u{2190} / Backspace", "Go to parent directory"),
            ("\u{2191} / \u{2193}", "Move selection"),
            ("Type", "Filter by name"),
            (
                "Ctrl+H",
                if self.show_hidden {
                    "Hide dotfiles"
                } else {
                    "Show dotfiles"
                },
            ),
            ("Esc", "Cancel"),
        ];

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));
        for (key, desc) in bindings {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:20}", key),
                    Style::default().fg(theme.accent).bold(),
                ),
                Span::styled(*desc, Style::default().fg(theme.text)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  Press ", Style::default().fg(theme.dimmed)),
            Span::styled("?", Style::default().fg(theme.hint)),
            Span::styled(" or ", Style::default().fg(theme.dimmed)),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::styled(" to close", Style::default().fg(theme.dimmed)),
        ]));

        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_listing_loads_in_the_background_and_a_failure_is_a_read_error() {
        let client = crate::daemon::DaemonClient::new("http://127.0.0.1:9", None).unwrap();
        let mut picker = DirPicker::new();
        picker.activate_remote(client, "/Users/remote");
        assert!(picker.is_active());
        assert!(
            picker.is_loading(),
            "activation must not wait on the daemon"
        );
        assert_eq!(picker.cwd, PathBuf::from("/Users/remote"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !picker.poll() {
            assert!(std::time::Instant::now() < deadline, "listing never landed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!picker.is_loading());
        assert!(picker.read_error.is_some());
        assert!(picker.dirs.is_empty());
    }

    #[test]
    fn a_listing_for_a_directory_the_user_left_is_dropped() {
        let mut picker = DirPicker::new();
        picker.cwd = PathBuf::from("/Users/remote/b");
        let stale = Ok(vec!["from-a".to_string()]);
        assert!(!picker.apply_listing(std::path::Path::new("/Users/remote/a"), false, stale));
        assert!(picker.dirs.is_empty());
        assert!(!picker.apply_listing(&picker.cwd.clone(), true, Ok(vec!["hidden".into()])));
        assert!(picker.apply_listing(&picker.cwd.clone(), false, Ok(vec!["b1".into()])));
        assert_eq!(picker.dirs, ["b1"]);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A picker opened on a temp dir holding `dirs` plus one regular file, which
    /// must never be listed.
    fn picker_over(dirs: &[&str]) -> (tempfile::TempDir, PathBuf, DirPicker) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().to_path_buf();
        for dir in dirs {
            std::fs::create_dir(base.join(dir)).unwrap();
        }
        std::fs::write(base.join("file.txt"), "hello").unwrap();
        let mut picker = DirPicker::new();
        picker.activate(&base.to_string_lossy());
        (tmp, base, picker)
    }

    /// The standard fixture, listing `./`, `../`, alpha, beta, gamma.
    fn fixture() -> (tempfile::TempDir, PathBuf, DirPicker) {
        picker_over(&["alpha", "beta", "gamma"])
    }

    fn press(picker: &mut DirPicker, codes: &[KeyCode]) -> DirPickerResult {
        let mut result = DirPickerResult::Continue;
        for code in codes {
            result = picker.handle_key(key(*code));
        }
        result
    }

    fn typed(picker: &mut DirPicker, text: &str) {
        for ch in text.chars() {
            picker.handle_key(key(KeyCode::Char(ch)));
        }
    }

    fn selected_path(result: DirPickerResult, what: &str) -> String {
        match result {
            DirPickerResult::Selected(path) => path,
            _ => panic!("expected Selected from {what}"),
        }
    }

    #[test]
    fn activate_lists_directories_only_and_starts_on_the_dot_entry() {
        assert!(!DirPicker::new().is_active());
        let (_tmp, base, picker) = fixture();
        assert!(picker.is_active());
        assert_eq!(picker.cwd, base);
        assert_eq!(picker.filter.value(), "");
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.dirs, vec!["alpha", "beta", "gamma"]);
        assert_eq!(picker.filtered_dirs()[..2], ["./", "../"]);
    }

    #[test]
    fn activate_with_an_empty_path_still_lands_on_a_real_directory() {
        let mut picker = DirPicker::new();
        picker.activate("");
        assert!(picker.is_active());
        assert!(picker.cwd.is_dir());
    }

    #[test]
    fn listing_sorts_case_insensitively() {
        let (_tmp, _base, picker) = picker_over(&["Zebra", "apple", "Banana"]);
        assert_eq!(picker.dirs, vec!["apple", "Banana", "Zebra"]);
    }

    #[cfg(unix)]
    #[test]
    fn listing_includes_symlinked_directories() {
        let (_tmp, base, mut picker) = picker_over(&["real"]);
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        picker.refresh_dirs();
        assert!(picker.dirs.contains(&"link".to_string()));
    }

    /// Hidden directories need an explicit Ctrl+H, which toggles both ways.
    #[test]
    fn ctrl_h_toggles_hidden_directories() {
        let (_tmp, _base, mut picker) = picker_over(&[".hidden", "visible"]);
        assert_eq!(picker.dirs, vec!["visible"]);
        let ctrl_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        picker.handle_key(ctrl_h);
        assert!(picker.show_hidden);
        assert_eq!(picker.dirs, vec![".hidden", "visible"]);
        picker.handle_key(ctrl_h);
        assert!(!picker.show_hidden);
        assert_eq!(picker.dirs, vec!["visible"]);
    }

    #[test]
    fn esc_cancels_and_closes() {
        let (_tmp, _base, mut picker) = fixture();
        assert!(matches!(
            press(&mut picker, &[KeyCode::Esc]),
            DirPickerResult::Cancelled
        ));
        assert!(!picker.is_active());
    }

    /// `Enter` and `Right` both act on the highlighted row: `./` selects the
    /// current directory and closes, a subdirectory navigates into it and resets
    /// the filter and selection.
    #[test]
    fn enter_and_right_select_the_dot_row_or_navigate_into_a_subdir() {
        for accept in [KeyCode::Enter, KeyCode::Right] {
            let (_tmp, base, mut picker) = fixture();
            let path = selected_path(press(&mut picker, &[accept]), "./");
            assert_eq!(path, base.to_string_lossy());
            assert!(!picker.is_active());

            // Rows are ./, ../, alpha, beta, gamma, so two Downs reach alpha.
            let (_tmp, base, mut picker) = fixture();
            let result = press(&mut picker, &[KeyCode::Down, KeyCode::Down, accept]);
            assert!(matches!(result, DirPickerResult::Continue));
            assert!(picker.is_active());
            assert_eq!(picker.cwd, base.join("alpha"));
            assert_eq!(picker.filter.value(), "");
            assert_eq!(picker.selected, 0);
        }
    }

    /// Every way up: `Enter` on `../`, `Left`, and `Backspace` on an empty
    /// filter.
    #[test]
    fn parent_navigation_keys_all_reach_the_parent() {
        let (_tmp, base, mut picker) = fixture();
        let result = press(&mut picker, &[KeyCode::Down, KeyCode::Enter]);
        assert!(matches!(result, DirPickerResult::Continue));
        assert!(picker.is_active());
        assert_eq!(picker.cwd, base.parent().unwrap());

        for code in [KeyCode::Left, KeyCode::Backspace] {
            let (_tmp, base, _) = fixture();
            let mut picker = DirPicker::new();
            picker.activate(&base.join("alpha").to_string_lossy());
            press(&mut picker, &[code]);
            assert_eq!(picker.cwd, base, "{code:?}");
        }
    }

    /// After navigating in, `./` selects the directory just entered.
    #[test]
    fn entering_a_subdir_then_accepting_dot_selects_it() {
        let (_tmp, base, mut picker) = fixture();
        press(&mut picker, &[KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
        assert_eq!(picker.cwd, base.join("alpha"));
        let path = selected_path(press(&mut picker, &[KeyCode::Enter]), "./");
        assert_eq!(path, base.join("alpha").to_string_lossy());
        assert!(!picker.is_active());
    }

    #[test]
    fn up_and_down_clamp_at_both_ends() {
        let (_tmp, _base, mut picker) = fixture();
        press(&mut picker, &[KeyCode::Up]);
        assert_eq!(picker.selected, 0);
        press(&mut picker, &[KeyCode::Down, KeyCode::Down, KeyCode::Up]);
        assert_eq!(picker.selected, 1);
        // Five rows: ./, ../, alpha, beta, gamma.
        press(&mut picker, &[KeyCode::Down; 10]);
        assert_eq!(picker.selected, 4);
    }

    /// Filtering matches directory names, drops the `./` and `../` rows unless
    /// the filter itself looks like them, and resets the highlight.
    #[test]
    fn filter_narrows_the_list_and_resets_the_selection() {
        let (_tmp, _base, mut picker) = fixture();
        press(&mut picker, &[KeyCode::Down, KeyCode::Down]);
        assert_eq!(picker.selected, 2);

        typed(&mut picker, "a");
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.filtered_dirs(), vec!["alpha", "beta", "gamma"]);

        typed(&mut picker, "l");
        assert_eq!(picker.filtered_dirs(), vec!["alpha"]);

        // Backspace with a filter edits it rather than navigating up.
        press(&mut picker, &[KeyCode::Backspace]);
        assert_eq!(picker.filter.value(), "a");
    }

    #[test]
    fn dot_filter_keeps_the_navigation_rows_only_while_it_matches_them() {
        let (_tmp, _base, mut picker) = fixture();
        typed(&mut picker, ".");
        let filtered = picker.filtered_dirs();
        assert!(filtered.contains(&"./".to_string()));
        assert!(filtered.contains(&"../".to_string()));

        typed(&mut picker, "/");
        let filtered = picker.filtered_dirs();
        assert!(!filtered.contains(&"./".to_string()));
        assert!(!filtered.contains(&"../".to_string()));
    }

    /// The filter owns every printable key, so `j`/`k` type rather than move.
    #[test]
    fn printable_keys_type_into_the_filter() {
        let (_tmp, _base, mut picker) = fixture();
        typed(&mut picker, "jk");
        assert_eq!(picker.filter.value(), "jk");
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn enter_on_a_single_filtered_match_navigates_into_it() {
        let (_tmp, base, mut picker) = fixture();
        typed(&mut picker, "al");
        press(&mut picker, &[KeyCode::Enter]);
        assert_eq!(picker.cwd, base.join("alpha"));
        assert_eq!(picker.filter.value(), "");
        assert_eq!(picker.selected, 0);
        assert!(picker.is_active());
    }

    /// Keys with nothing to act on leave the picker exactly as it was.
    #[test]
    fn enter_on_an_empty_match_list_and_tab_are_no_ops() {
        let (_tmp, _base, mut picker) = fixture();
        typed(&mut picker, "zzz");
        assert!(picker.filtered_dirs().is_empty());
        assert!(matches!(
            press(&mut picker, &[KeyCode::Enter]),
            DirPickerResult::Continue
        ));
        assert!(picker.is_active());

        let (_tmp, base, mut picker) = fixture();
        assert!(matches!(
            press(&mut picker, &[KeyCode::Tab]),
            DirPickerResult::Continue
        ));
        assert_eq!(picker.cwd, base);
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn root_has_no_parent_row_but_keeps_the_dot_row() {
        let mut picker = DirPicker::new();
        picker.activate("/");
        let filtered = picker.filtered_dirs();
        assert!(!filtered.contains(&"../".to_string()));
        assert!(filtered.contains(&"./".to_string()));
    }

    #[test]
    fn an_unreadable_directory_lists_nothing_and_flags_the_error() {
        let mut picker = DirPicker::new();
        picker.cwd = PathBuf::from("/nonexistent_path_that_should_not_exist");
        picker.refresh_dirs();
        assert!(picker.read_error.is_some());
        assert!(picker.dirs.is_empty());
    }

    /// Long paths are cut from the left, keeping the tail, and never split a
    /// multi-byte character.
    #[test]
    fn truncate_path_keeps_the_tail_within_the_budget() {
        assert_eq!(DirPicker::truncate_path("/short", 20), "/short");
        assert_eq!(DirPicker::truncate_path("/exact", 6), "/exact");

        let long = "/home/user/very/deeply/nested/directory/structure";
        let cut = DirPicker::truncate_path(long, 30);
        assert!(cut.starts_with("..."));
        assert!(cut.chars().count() <= 30);
        assert!(cut.ends_with("directory/structure"));

        for (path, budget) in [
            ("/home/user/projetcs/donnees/repertoire", 20),
            (
                "/home/\u{00e9}\u{00e8}\u{00ea}/\u{00fc}\u{00f6}\u{00e4}/dir",
                10,
            ),
        ] {
            let cut = DirPicker::truncate_path(path, budget);
            assert!(cut.starts_with("..."), "{path}");
            assert!(cut.chars().count() <= budget, "{path}");
        }
    }
}

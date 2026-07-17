//! Plugin manager: list plugins (builtin and external) with their trust and
//! enabled/approval state, toggle them (reconciling a running daemon's workers
//! live), inspect a plugin's full disclosure (capabilities, keybinds, runtime),
//! and run the whole external-plugin lifecycle in-TUI: install from GitHub
//! discovery, update, re-approve a stale grant, and uninstall, each behind the
//! same consent popup the CLI prompt and the web modals render. The TUI twin of
//! `aoe plugin` and the web Plugins tab.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tokio::sync::oneshot;

use super::{centered_rect, DialogResult};
use crate::plugin::changelog::{ChangelogEntry, UpdateChangelog};
use crate::plugin::discover::{DiscoveryBadge, DiscoveryResult};
use crate::plugin::install::{
    InstallConsent, LiveToggle, ReapproveConsent, UpdateConsent, UpdatePreview,
};
use crate::plugin::update_check::UpdateStatus;
use crate::tui::styles::Theme;

/// An open update review popup. Every update (safe or consent-required) shows
/// the changelog; `consent` is `Some` only when the update also expands access,
/// adding the capability / build / UI / runtime / trust disclosures and the
/// Decline (dismiss) action.
struct Review {
    id: String,
    from_version: String,
    to_version: String,
    fingerprint: String,
    changelog: UpdateChangelog,
    consent: Option<UpdateConsent>,
}

/// Installed-plugin details, captured from the registry when opened so the
/// popup never re-reads a registry that may reload underneath it.
struct Details {
    view: crate::plugin::PluginView,
    commands: Vec<String>,
    keybinds: Vec<String>,
    runtime: Option<String>,
    settings: Vec<String>,
    dir: Option<String>,
}

/// A running or finished lifecycle operation (install / update) whose log file
/// the popup tails, the TUI twin of the dashboard's job progress modal.
struct Progress {
    title: String,
    log_path: PathBuf,
    /// `None` while running; the final outcome line once done.
    done: Option<Result<String, String>>,
}

/// Content for [`PluginManagerDialog::draw_popup`]: a scrollable body over a
/// pinned footer (the decision keys / status), and whether the body should
/// follow its tail as it grows (the running progress log).
struct PopupContent<'a> {
    body: Vec<Line<'a>>,
    footer: Vec<Line<'a>>,
    follow_tail: bool,
    title: &'a str,
}

/// The floating popup owning the keyboard; at most one at a time.
enum Popup {
    /// Update review: changelog plus, when access expands, the consent
    /// disclosure.
    Review(Box<Review>),
    /// Install consent for a discovery result.
    Install(Box<InstallConsent>),
    /// Re-approval consent for an installed plugin whose grant went stale.
    Reapprove(ReapproveConsent),
    /// Uninstall confirmation.
    ConfirmUninstall { id: String },
    /// Installed-plugin details (Enter on a row).
    Details(Box<Details>),
    /// A lifecycle operation streaming its log tail.
    Progress(Progress),
}

/// Which view the manager is showing: the installed list or GitHub discovery
/// results.
#[derive(PartialEq, Eq)]
enum Mode {
    Browse,
    Discover,
}

/// A network task running off the event loop, polled by [`PluginManagerDialog::tick`].
/// The work runs on a spawned tokio task so the TUI never blocks on git or
/// GitHub (a dead remote would otherwise freeze the whole UI).
enum Pending {
    Updates(oneshot::Receiver<Vec<UpdateStatus>>),
    Discover(oneshot::Receiver<Result<Vec<DiscoveryResult>, String>>),
    /// Classifying one plugin's available update (the `u` key).
    Preview(oneshot::Receiver<Result<UpdatePreview, String>>),
    /// Applying an approved update; the Ok string is the final report line.
    Apply(oneshot::Receiver<Result<String, String>>),
    /// An enable/disable running through [`set_enabled_live`] (a running
    /// daemon reconciles its workers); the Ok string reports where it landed.
    Toggle(oneshot::Receiver<Result<String, String>>),
    /// Fetching the install consent disclosure for a discovery result.
    InstallPreview(oneshot::Receiver<Result<InstallConsent, String>>),
    /// Applying an approved install.
    InstallApply(oneshot::Receiver<Result<String, String>>),
    Uninstall(oneshot::Receiver<Result<String, String>>),
}

pub struct PluginManagerDialog {
    /// The shared manager view-model, the same shape the web dashboard renders
    /// from (`crate::plugin::view`). Built straight off the registry, so the
    /// TUI never re-derives plugin fields.
    rows: Vec<crate::plugin::PluginView>,
    load_errors: Vec<String>,
    selected: usize,
    error: Option<String>,
    info: Option<String>,
    /// Set whenever the on-disk plugin config changed (enable/disable,
    /// install, update, uninstall, re-approve). An embedding surface drains it
    /// via [`take_mutated`] to re-sync its own config view; the standalone
    /// modal ignores it.
    mutated: bool,
    /// True when hosted inside the settings screen (vs the command-palette
    /// modal). Only changes the footer hint: Esc returns to the category list.
    embedded: bool,
    /// Set by the settings host when an editable plugin-settings pane renders
    /// beneath the manager, so the footer advertises the Tab sub-focus.
    has_settings_pane: bool,
    mode: Mode,
    /// An in-flight discovery / update-check / lifecycle task; `None` when idle.
    pending: Option<Pending>,
    /// A transient status line shown while a task runs ("Checking for updates…").
    loading: Option<&'static str>,
    /// Update statuses from the last `c` check, keyed by plugin id; drives the
    /// per-row "update!" marker.
    updates: HashMap<String, UpdateStatus>,
    /// Discovery results from the last `d` search, plus the cursor into them.
    discover_rows: Vec<DiscoveryResult>,
    discover_selected: usize,
    /// The free-text GitHub search term, edited with `/` in discover mode.
    discover_query: String,
    /// The `/` input line is active: printable keys edit the query.
    query_editing: bool,
    /// The plugin id a preview/apply is running for, so `tick` knows which row
    /// the result belongs to.
    pending_plugin: Option<String>,
    /// The floating popup owning the keyboard, if any.
    popup: Option<Popup>,
    /// Scroll offset into the open popup's body. A `Cell` so render (`&self`)
    /// can clamp it to the real content height, which only render knows.
    popup_scroll: Cell<u16>,
    /// The user scrolled the open popup themselves; a following popup (the
    /// running progress log) stops auto-scrolling to the tail once set.
    popup_user_scrolled: bool,
}

impl Default for PluginManagerDialog {
    fn default() -> Self {
        Self::new()
    }
}

/// Most changelog lines the review popup renders before linking out to GitHub
/// for the rest. The popup scrolls, so this only bounds the popup body (the
/// entry counts are already capped by the backend; this bounds multi-line
/// release bodies too).
const MAX_CHANGELOG_LINES: usize = 60;

/// How many trailing log lines the progress popup tails.
const PROGRESS_TAIL_LINES: usize = 30;

/// How far back in the log file the tail reads. Build output can grow to
/// megabytes; only the end is ever shown.
const PROGRESS_TAIL_BYTES: u64 = 16 * 1024;

/// Append the changelog to a review popup's lines: release notes or commit
/// subjects, or a single "unavailable" / "none" line. The rendered body is
/// capped at [`MAX_CHANGELOG_LINES`] and the full history is linked via
/// `more_url`.
fn push_changelog_lines(lines: &mut Vec<Line>, changelog: &UpdateChangelog, theme: &Theme) {
    if let Some(reason) = &changelog.unavailable_reason {
        lines.push(Line::from(Span::styled(
            reason.clone(),
            Style::default().fg(theme.dimmed),
        )));
        return;
    }
    if changelog.entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "No changelog available.",
            Style::default().fg(theme.dimmed),
        )));
        return;
    }
    lines.push(Line::from(Span::styled(
        "What's new:",
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    )));
    let mut remaining = MAX_CHANGELOG_LINES;
    let mut clipped = false;
    'entries: for entry in &changelog.entries {
        match entry {
            ChangelogEntry::Release { tag, body, .. } => {
                if remaining == 0 {
                    clipped = true;
                    break;
                }
                lines.push(Line::from(Span::styled(
                    tag.clone(),
                    Style::default().fg(theme.text),
                )));
                remaining -= 1;
                if let Some(body) = body {
                    for line in body.lines() {
                        if remaining == 0 {
                            clipped = true;
                            break 'entries;
                        }
                        lines.push(Line::from(Span::styled(
                            format!("  {line}"),
                            Style::default().fg(theme.dimmed),
                        )));
                        remaining -= 1;
                    }
                }
            }
            ChangelogEntry::Commit { sha, subject, .. } => {
                if remaining == 0 {
                    clipped = true;
                    break;
                }
                let short: String = sha.chars().take(7).collect();
                lines.push(Line::from(Span::styled(
                    format!("  {short} {subject}"),
                    Style::default().fg(theme.dimmed),
                )));
                remaining -= 1;
            }
        }
    }
    // Link to the full history when the changelog was clipped here or already
    // truncated upstream. Fall back to a plain marker if there is no URL.
    if clipped || changelog.truncated {
        let marker = match &changelog.more_url {
            Some(url) => format!("  ... full changelog: {url}"),
            None => "  ... older history on GitHub".to_string(),
        };
        lines.push(Line::from(Span::styled(
            marker,
            Style::default().fg(theme.dimmed),
        )));
    }
}

/// The log file a TUI-run lifecycle operation writes build output to, beside
/// the dashboard's job logs (`<plugins_dir>/jobs/`).
fn tui_job_log(op: &str, id: &str) -> anyhow::Result<PathBuf> {
    Ok(crate::plugin::plugins_dir()?
        .join("jobs")
        .join(format!("tui-{op}-{id}.log")))
}

/// Last `max_lines` lines of a log file, reading at most
/// [`PROGRESS_TAIL_BYTES`] from its end. Returns an empty vec while the file
/// does not exist yet.
fn read_log_tail(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let seeked = len > PROGRESS_TAIL_BYTES;
    if seeked
        && file
            .seek(SeekFrom::End(-(PROGRESS_TAIL_BYTES as i64)))
            .is_err()
    {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    // A mid-file seek almost certainly landed inside a line; drop the partial.
    if seeked && !lines.is_empty() {
        lines.remove(0);
    }
    lines
        .into_iter()
        .rev()
        .take(max_lines)
        .rev()
        .map(str::to_string)
        .collect()
}

/// Rows `line` occupies when rendered with `Wrap { trim: true }` into `width`
/// columns: greedy word wrap, leading indentation counted toward the first
/// row, over-wide words split across rows. Popup sizing and scroll bounds use
/// this so a wrapped line can never push content (like the decision-key
/// footer) off the bottom edge.
fn wrapped_rows(line: &Line, width: u16) -> u16 {
    use unicode_width::UnicodeWidthStr;
    if width == 0 {
        return 1;
    }
    let max = width as usize;
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let trimmed = text.trim_end();
    if trimmed.trim_start().is_empty() {
        return 1;
    }
    let indent = trimmed.len() - trimmed.trim_start().len();
    let mut rows: u16 = 1;
    let mut used = trimmed[..indent].width().min(max);
    let mut first_in_row = used == 0;
    for word in trimmed.split_whitespace() {
        let word_width = word.width().max(1);
        let sep = if first_in_row { 0 } else { 1 };
        if used + sep + word_width <= max {
            used += sep + word_width;
            first_in_row = false;
        } else if word_width <= max {
            rows = rows.saturating_add(1);
            used = word_width;
            first_in_row = false;
        } else {
            // A word wider than the popup hard-splits across rows.
            let full = word_width.div_ceil(max);
            if used > 0 {
                rows = rows.saturating_add(1);
            }
            rows = rows.saturating_add((full - 1) as u16);
            used = word_width - (full - 1) * max;
            first_in_row = false;
        }
    }
    rows
}

fn wrapped_rows_total(lines: &[Line], width: u16) -> u16 {
    lines
        .iter()
        .map(|l| wrapped_rows(l, width))
        .fold(0u16, u16::saturating_add)
}

fn setting_type_label(t: aoe_plugin_api::SettingType) -> &'static str {
    match t {
        aoe_plugin_api::SettingType::String => "string",
        aoe_plugin_api::SettingType::Bool => "bool",
        aoe_plugin_api::SettingType::Integer => "integer",
        aoe_plugin_api::SettingType::Select => "select",
    }
}

impl PluginManagerDialog {
    pub fn new() -> Self {
        let mut dialog = Self {
            rows: Vec::new(),
            load_errors: Vec::new(),
            selected: 0,
            error: None,
            info: None,
            mutated: false,
            embedded: false,
            has_settings_pane: false,
            mode: Mode::Browse,
            pending: None,
            loading: None,
            updates: HashMap::new(),
            discover_rows: Vec::new(),
            discover_selected: 0,
            discover_query: String::new(),
            query_editing: false,
            pending_plugin: None,
            popup: None,
            popup_scroll: Cell::new(0),
            popup_user_scrolled: false,
        };
        dialog.reload();
        dialog.mutated = false; // Initial load is not a user mutation.
        dialog
    }

    /// A manager hosted inside the settings screen rather than the command
    /// palette. Only the footer differs: Esc returns to the category list.
    pub fn embedded() -> Self {
        let mut dialog = Self::new();
        dialog.embedded = true;
        dialog
    }

    /// Take and clear the "config mutated" flag (a lifecycle action wrote to
    /// disk and reloaded the registry).
    pub fn take_mutated(&mut self) -> bool {
        std::mem::take(&mut self.mutated)
    }

    /// Whether the dialog currently owns every key (an open popup, or discover
    /// mode). The settings host checks this before intercepting Space to stage
    /// an enable/disable, so a popup or the discovery search never loses keys
    /// to the staging shortcut.
    pub fn captures_input(&self) -> bool {
        self.popup.is_some() || self.mode == Mode::Discover
    }

    /// Told by the settings host whether an editable plugin-settings pane
    /// renders beneath the manager, so the footer can advertise Tab.
    pub fn set_has_settings_pane(&mut self, has: bool) {
        self.has_settings_pane = has;
    }

    /// Height the inline (settings-embedded) manager wants: its rows plus
    /// chrome. The settings host sizes the master-detail split from this so
    /// the list stops taking half the pane to show two rows.
    pub fn preferred_inline_height(&self) -> u16 {
        let errors: u16 = if self.load_errors.is_empty() { 0 } else { 2 };
        (self.rows.len().max(1) as u16)
            .saturating_add(2) // borders
            .saturating_add(2) // footer
            .saturating_add(errors)
    }

    /// Select the row owning a `plugin:<id>.<field>` settings ident, so a
    /// settings-search jump into the Plugins tab lands on the right plugin's
    /// detail pane. Returns whether a row matched.
    pub fn select_plugin_owning_ident(&mut self, ident: &str) -> bool {
        let Some(rest) = ident.strip_prefix(crate::session::settings_schema::PLUGIN_SECTION_PREFIX)
        else {
            return false;
        };
        // Plugin ids are dotted themselves, so match "<id>." as a prefix of
        // the remainder rather than splitting on the first dot.
        if let Some(idx) = self.rows.iter().position(|r| {
            rest.strip_prefix(r.id.as_str())
                .is_some_and(|tail| tail.starts_with('.'))
        }) {
            self.selected = idx;
            true
        } else {
            false
        }
    }

    fn reload(&mut self) {
        // reload() runs only after a config-mutating action (and once at
        // construction), so it is the single place to flag a mutation.
        self.mutated = true;
        let registry = crate::plugin::reload_registry();
        self.rows = registry.all().iter().map(|p| p.view()).collect();
        self.load_errors = registry.load_errors().to_vec();
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    fn open_popup(&mut self, popup: Popup) {
        self.popup = Some(popup);
        self.popup_scroll.set(0);
        self.popup_user_scrolled = false;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        self.info = None;
        // An open popup owns the keyboard until the user decides.
        if self.popup.is_some() {
            return self.handle_popup_key(key);
        }
        if self.mode == Mode::Discover {
            return self.handle_discover_key(key);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.rows.is_empty() {
                    self.selected = (self.selected + 1).min(self.rows.len() - 1);
                }
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                DialogResult::Continue
            }
            KeyCode::Char(' ') => {
                self.start_toggle();
                DialogResult::Continue
            }
            KeyCode::Enter => {
                self.open_details();
                DialogResult::Continue
            }
            // Re-approve a plugin whose grant no longer covers its manifest.
            KeyCode::Char('a') => {
                self.open_reapprove();
                DialogResult::Continue
            }
            // Explicit, on-demand network actions. They run off the event loop
            // (see `tick`); a second press while one is in flight is ignored.
            KeyCode::Char('c') => {
                self.start_update_check();
                DialogResult::Continue
            }
            KeyCode::Char('d') => {
                self.start_discover();
                DialogResult::Continue
            }
            // Update the selected plugin, but only when the last `c` check found
            // one available (the preview re-fetches and classifies it).
            KeyCode::Char('u') => {
                if let Some(row) = self.rows.get(self.selected) {
                    if self.updates.get(&row.id).is_some_and(|u| u.needs_update) {
                        self.start_preview(row.id.clone());
                    }
                }
                DialogResult::Continue
            }
            KeyCode::Char('x') => {
                if let Some(row) = self.rows.get(self.selected) {
                    if row.builtin {
                        self.info = Some(format!("{} is builtin; disable it instead.", row.id));
                    } else {
                        self.open_popup(Popup::ConfirmUninstall { id: row.id.clone() });
                    }
                }
                DialogResult::Continue
            }
            // Re-read the registry from disk (an external `aoe plugin` command
            // may have changed it while the manager was open).
            KeyCode::Char('r') => {
                self.reload();
                self.info = Some("Refreshed.".to_string());
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn handle_popup_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        // Every popup body scrolls with the same keys.
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.popup_scroll
                    .set(self.popup_scroll.get().saturating_add(1));
                self.popup_user_scrolled = true;
                return DialogResult::Continue;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.popup_scroll
                    .set(self.popup_scroll.get().saturating_sub(1));
                self.popup_user_scrolled = true;
                return DialogResult::Continue;
            }
            _ => {}
        }
        let Some(popup) = self.popup.take() else {
            return DialogResult::Continue;
        };
        match popup {
            Popup::Review(review) => self.handle_review_key(key, *review),
            Popup::Install(consent) => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.start_install_apply(*consent);
                    DialogResult::Continue
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                    self.info = Some("Install cancelled.".to_string());
                    DialogResult::Continue
                }
                _ => {
                    self.popup = Some(Popup::Install(consent));
                    DialogResult::Continue
                }
            },
            Popup::Reapprove(consent) => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    match crate::plugin::install::approve_installed(
                        &consent.id,
                        &consent.manifest_hash,
                    ) {
                        Ok(()) => {
                            self.info = Some(format!("Approved {}.", consent.id));
                            self.error = None;
                            self.reload();
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                    DialogResult::Continue
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => DialogResult::Continue,
                _ => {
                    self.popup = Some(Popup::Reapprove(consent));
                    DialogResult::Continue
                }
            },
            Popup::ConfirmUninstall { id } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.start_uninstall(id);
                    DialogResult::Continue
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => DialogResult::Continue,
                _ => {
                    self.popup = Some(Popup::ConfirmUninstall { id });
                    DialogResult::Continue
                }
            },
            Popup::Details(details) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => DialogResult::Continue,
                _ => {
                    self.popup = Some(Popup::Details(details));
                    DialogResult::Continue
                }
            },
            Popup::Progress(progress) => {
                // Once done, any decision key dismisses the popup. While the
                // operation still runs, Esc hides the popup without cancelling
                // it (a hung network fetch must never trap the keyboard); the
                // result then lands in the footer.
                let dismiss = if progress.done.is_some() {
                    matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter)
                } else {
                    key.code == KeyCode::Esc
                };
                if !dismiss {
                    self.popup = Some(Popup::Progress(progress));
                }
                DialogResult::Continue
            }
        }
    }

    /// Keys while the update review popup is open: approve/update, decline (only
    /// when access expands), or close. The popup was taken out of `self.popup`
    /// by the caller; put it back unless the key decided it.
    fn handle_review_key(&mut self, key: KeyEvent, review: Review) -> DialogResult<()> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.start_apply(review.id, Some(review.fingerprint));
                DialogResult::Continue
            }
            // Decline only applies to a consent-expanding update: record the
            // dismissal so it stops nagging, keep the active version. A safe
            // update has nothing to dismiss, so `n` just closes it.
            KeyCode::Char('n') => {
                if review.consent.is_some() {
                    match crate::plugin::install::dismiss_update(&review.id, &review.fingerprint) {
                        Ok(()) => {
                            // dismiss_update wrote plugin config; flag it so
                            // an embedding settings surface resyncs and a
                            // later save does not clobber the dismissal.
                            self.mutated = true;
                            self.info = Some(format!("Declined update for {}.", review.id));
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
                DialogResult::Continue
            }
            // Close without deciding.
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Continue,
            _ => {
                self.popup = Some(Popup::Review(Box::new(review)));
                DialogResult::Continue
            }
        }
    }

    fn handle_discover_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        if self.query_editing {
            match key.code {
                KeyCode::Esc => self.query_editing = false,
                KeyCode::Enter => {
                    self.query_editing = false;
                    self.start_discover();
                }
                KeyCode::Backspace => {
                    self.discover_query.pop();
                }
                KeyCode::Char(c) => self.discover_query.push(c),
                _ => {}
            }
            return DialogResult::Continue;
        }
        match key.code {
            // Esc/q leave discovery for the installed list, not the whole dialog.
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.discover_rows.is_empty() {
                    self.discover_selected =
                        (self.discover_selected + 1).min(self.discover_rows.len() - 1);
                }
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.discover_selected = self.discover_selected.saturating_sub(1);
                DialogResult::Continue
            }
            // Install the selected result: fetch its consent disclosure, then
            // approve in the same popup the CLI prompt and web modal render.
            KeyCode::Enter => {
                self.start_install_preview();
                DialogResult::Continue
            }
            KeyCode::Char('/') => {
                self.query_editing = true;
                DialogResult::Continue
            }
            KeyCode::Char('d') => {
                self.start_discover();
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    /// Toggle the selected plugin through [`set_enabled_live`], which routes
    /// the write through a running daemon (so its workers reconcile) and falls
    /// back to a local config write. Async because the daemon round-trip is.
    fn start_toggle(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let id = row.id.clone();
        let target = !row.enabled;
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let verb = if target { "Enabled" } else { "Disabled" };
            let message = match crate::plugin::install::set_enabled_live(&id, target).await {
                Ok(LiveToggle::Daemon) => {
                    Ok(format!("{verb} {id}; the daemon reconciled its workers."))
                }
                Ok(LiveToggle::Local) => Ok(format!("{verb} {id}.")),
                Ok(LiveToggle::LocalDaemonStale { reason }) => Ok(format!(
                    "{verb} {id}. Daemon not updated ({reason}); restart it or toggle from the dashboard."
                )),
                Err(e) => Err(format!("{e:#}")),
            };
            let _ = tx.send(message);
        });
        self.pending = Some(Pending::Toggle(rx));
        self.loading = Some("Applying…");
        self.error = None;
    }

    /// Open the details popup for the selected row: the full disclosure
    /// (`aoe plugin info`'s TUI twin).
    fn open_details(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let registry = crate::plugin::registry();
        let Some(plugin) = registry.get(&row.id) else {
            return;
        };
        let m = &plugin.manifest;
        let commands = m
            .commands
            .iter()
            .map(|c| {
                let title = if c.title.is_empty() {
                    c.id.clone()
                } else {
                    format!("{} ({})", c.title, c.id)
                };
                if c.description.is_empty() {
                    title
                } else {
                    format!("{title}: {}", c.description)
                }
            })
            .collect();
        let keybinds = m
            .keybinds
            .iter()
            .map(|kb| {
                let note = match crate::tui::home::bindings::parse_chord(&kb.key) {
                    Some(c) if crate::tui::home::bindings::core_shadows(&c) => {
                        " (shadowed by core)"
                    }
                    Some(_) => "",
                    None => " (invalid key, ignored)",
                };
                format!("{} -> {}{note}", kb.key, kb.command)
            })
            .collect();
        let runtime = m.runtime.as_ref().map(|r| match r {
            aoe_plugin_api::RuntimeSpec::Command {
                command,
                system,
                build,
            } => {
                let mut s = format!("command: {}", command.join(" "));
                if *system {
                    s.push_str(" (resolved on the daemon's PATH)");
                }
                if !build.is_empty() {
                    s.push_str(&format!(
                        "; {} build step(s) at install/update",
                        build.len()
                    ));
                }
                s
            }
            aoe_plugin_api::RuntimeSpec::ReleaseBinary { asset, .. } => {
                format!("release binary: {asset}")
            }
        });
        let settings = m
            .settings
            .iter()
            .map(|s| {
                let label = if s.label.is_empty() {
                    s.key.clone()
                } else {
                    format!("{} ({})", s.label, s.key)
                };
                format!("{label}: {}", setting_type_label(s.value_type))
            })
            .collect();
        let details = Details {
            view: row.clone(),
            commands,
            keybinds,
            runtime,
            settings,
            dir: plugin.dir.as_ref().map(|d| d.display().to_string()),
        };
        self.open_popup(Popup::Details(Box::new(details)));
    }

    /// Open the re-approval consent popup for a plugin whose grant no longer
    /// covers its installed manifest.
    fn open_reapprove(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.needs_reapproval {
            self.info = Some(format!("{} does not need approval.", row.id));
            return;
        }
        match crate::plugin::install::reapprove_consent(&row.id) {
            Ok(consent) => self.open_popup(Popup::Reapprove(consent)),
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn start_update_check(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(crate::plugin::update_check::outdated().await);
        });
        self.pending = Some(Pending::Updates(rx));
        self.loading = Some("Checking for updates…");
        self.error = None;
    }

    fn start_preview(&mut self, id: String) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = oneshot::channel();
        let preview_id = id.clone();
        tokio::spawn(async move {
            let _ = tx.send(
                crate::plugin::install::preview_update(&preview_id)
                    .await
                    .map_err(|e| format!("{e:#}")),
            );
        });
        self.pending_plugin = Some(id);
        self.pending = Some(Pending::Preview(rx));
        self.loading = Some("Checking update…");
        self.error = None;
    }

    fn start_apply(&mut self, id: String, fingerprint: Option<String>) {
        if self.pending.is_some() {
            return;
        }
        let log_path = match tui_job_log("update", &id) {
            Ok(path) => path,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                return;
            }
        };
        let _ = std::fs::remove_file(&log_path);
        let (tx, rx) = oneshot::channel();
        let apply_id = id.clone();
        let task_log = log_path.clone();
        tokio::spawn(async move {
            let result = async {
                let log = crate::plugin::install::OperationLog::file(&task_log)
                    .map_err(|e| format!("{e:#}"))?;
                crate::plugin::install::apply_update(&apply_id, fingerprint, &log)
                    .await
                    .map(|report| format!("Updated {} to {}.", report.id, report.version))
                    .map_err(|e| format!("{e:#}"))
            }
            .await;
            let _ = tx.send(result);
        });
        self.pending_plugin = Some(id.clone());
        self.pending = Some(Pending::Apply(rx));
        self.open_popup(Popup::Progress(Progress {
            title: format!(" Updating {id} "),
            log_path,
            done: None,
        }));
        self.loading = Some("Updating…");
        self.error = None;
    }

    fn start_discover(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let query = {
            let trimmed = self.discover_query.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = crate::plugin::discover::discover(query.as_deref())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.pending = Some(Pending::Discover(rx));
        self.loading = Some("Searching GitHub…");
        self.error = None;
    }

    /// Fetch the consent disclosure for the selected discovery result (the
    /// `preview_install` probe: network-only, installs nothing).
    fn start_install_preview(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let Some(result) = self.discover_rows.get(self.discover_selected) else {
            return;
        };
        if result.badge == DiscoveryBadge::Installed {
            self.info = Some(format!("{} is already installed.", result.slug));
            return;
        }
        let source = result.slug.clone();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(
                crate::plugin::install::preview_install(&source)
                    .await
                    .map_err(|e| format!("{e:#}")),
            );
        });
        self.pending = Some(Pending::InstallPreview(rx));
        self.loading = Some("Fetching plugin…");
        self.error = None;
    }

    /// Apply an approved install, pinned to the fingerprint the consent popup
    /// showed. Build output streams to a job log the progress popup tails.
    fn start_install_apply(&mut self, consent: InstallConsent) {
        if self.pending.is_some() {
            return;
        }
        let log_path = match tui_job_log("install", &consent.id) {
            Ok(path) => path,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                return;
            }
        };
        let _ = std::fs::remove_file(&log_path);
        let (tx, rx) = oneshot::channel();
        let source = consent.source.clone();
        let fingerprint = consent.fingerprint.clone();
        let task_log = log_path.clone();
        tokio::spawn(async move {
            let result = async {
                let log = crate::plugin::install::OperationLog::file(&task_log)
                    .map_err(|e| format!("{e:#}"))?;
                crate::plugin::install::apply_install(&source, &fingerprint, &log)
                    .await
                    .map(|report| format!("Installed {} {}.", report.id, report.version))
                    .map_err(|e| format!("{e:#}"))
            }
            .await;
            let _ = tx.send(result);
        });
        self.pending_plugin = Some(consent.id.clone());
        self.pending = Some(Pending::InstallApply(rx));
        self.open_popup(Popup::Progress(Progress {
            title: format!(" Installing {} ", consent.id),
            log_path,
            done: None,
        }));
        self.loading = Some("Installing…");
        self.error = None;
    }

    fn start_uninstall(&mut self, id: String) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = oneshot::channel();
        let task_id = id.clone();
        tokio::spawn(async move {
            let blocking_id = task_id.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::plugin::install::uninstall(&blocking_id).map_err(|e| format!("{e:#}"))
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
            .map(|()| format!("Uninstalled {task_id}."));
            let _ = tx.send(result);
        });
        self.pending_plugin = Some(id);
        self.pending = Some(Pending::Uninstall(rx));
        self.loading = Some("Uninstalling…");
        self.error = None;
    }

    /// Resolve a finished lifecycle task into the open progress popup (so the
    /// user reads the outcome over the log tail) or, if the popup is gone, the
    /// footer.
    fn finish_operation(&mut self, result: Result<String, String>) {
        let ok = result.is_ok();
        if ok {
            self.reload();
        }
        match &mut self.popup {
            Some(Popup::Progress(progress)) => progress.done = Some(result),
            _ => match result {
                Ok(message) => self.info = Some(message),
                Err(message) => self.error = Some(message),
            },
        }
    }

    /// Poll an in-flight task. Returns true when the result landed (the host
    /// should redraw). Called from the event-loop tick.
    pub fn tick(&mut self) -> bool {
        use oneshot::error::TryRecvError;
        let Some(pending) = &mut self.pending else {
            return false;
        };
        match pending {
            Pending::Updates(rx) => match rx.try_recv() {
                Ok(statuses) => {
                    let outdated = statuses.iter().filter(|s| s.needs_update).count();
                    let errors = statuses.iter().filter(|s| s.error.is_some()).count();
                    // outdated() skips builtins, so an empty result means there
                    // are no external plugins; the dialog still lists builtin
                    // rows, so "all up to date" would read as if they were
                    // checked. Match the CLI's wording instead.
                    let empty = statuses.is_empty();
                    self.updates = statuses.into_iter().map(|s| (s.id.clone(), s)).collect();
                    self.info = Some(if empty {
                        "No external plugins installed.".to_string()
                    } else {
                        match (outdated, errors) {
                            (0, 0) => "All plugins up to date.".to_string(),
                            (n, 0) => format!("{n} plugin(s) have updates available."),
                            (n, e) => format!("{n} update(s) available, {e} check error(s)."),
                        }
                    });
                    self.pending = None;
                    self.loading = None;
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Closed) => {
                    self.error = Some("Update check failed.".to_string());
                    self.pending = None;
                    self.loading = None;
                    true
                }
            },
            Pending::Discover(rx) => match rx.try_recv() {
                Ok(Ok(results)) => {
                    self.discover_rows = results;
                    self.discover_selected = 0;
                    self.mode = Mode::Discover;
                    self.pending = None;
                    self.loading = None;
                    true
                }
                Ok(Err(message)) => {
                    self.error = Some(message);
                    self.pending = None;
                    self.loading = None;
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Closed) => {
                    self.error = Some("Discovery failed.".to_string());
                    self.pending = None;
                    self.loading = None;
                    true
                }
            },
            Pending::Preview(rx) => match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    self.loading = None;
                    match result {
                        Ok(UpdatePreview::NoUpdate) => {
                            self.info = Some("Already up to date.".to_string());
                        }
                        // A safe update needs no consent, but still shows its
                        // changelog in a review popup before applying.
                        Ok(UpdatePreview::SafeUpdate {
                            to_version,
                            fingerprint,
                            changelog,
                        }) => {
                            if let Some(id) = self.pending_plugin.clone() {
                                let from_version = self
                                    .rows
                                    .iter()
                                    .find(|r| r.id == id)
                                    .map(|r| r.version.clone())
                                    .unwrap_or_default();
                                self.open_popup(Popup::Review(Box::new(Review {
                                    id,
                                    from_version,
                                    to_version,
                                    fingerprint,
                                    changelog,
                                    consent: None,
                                })));
                            }
                        }
                        // An already-dismissed version must not re-prompt; it
                        // surfaces again only when a new version appears.
                        Ok(UpdatePreview::ConsentRequired { consent, dismissed }) => {
                            if dismissed {
                                self.info = Some(format!(
                                    "Update for {} was already declined.",
                                    consent.id
                                ));
                            } else {
                                self.open_popup(Popup::Review(Box::new(Review {
                                    id: consent.id.clone(),
                                    from_version: consent.from_version.clone(),
                                    to_version: consent.to_version.clone(),
                                    fingerprint: consent.fingerprint.clone(),
                                    changelog: consent.changelog.clone(),
                                    consent: Some(*consent),
                                })));
                            }
                        }
                        Err(message) => self.error = Some(message),
                    }
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Closed) => {
                    self.error = Some("Update check failed.".to_string());
                    self.pending = None;
                    self.loading = None;
                    true
                }
            },
            Pending::Apply(rx) => match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    self.loading = None;
                    if result.is_ok() {
                        if let Some(id) = self.pending_plugin.take() {
                            self.updates.remove(&id);
                        }
                    }
                    self.finish_operation(result);
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Closed) => {
                    self.pending = None;
                    self.loading = None;
                    self.finish_operation(Err("Update failed.".to_string()));
                    true
                }
            },
            Pending::Toggle(rx) => match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    self.loading = None;
                    match result {
                        Ok(message) => {
                            self.info = Some(message);
                            self.error = None;
                            self.reload();
                        }
                        Err(message) => self.error = Some(message),
                    }
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Closed) => {
                    self.error = Some("Toggle failed.".to_string());
                    self.pending = None;
                    self.loading = None;
                    true
                }
            },
            Pending::InstallPreview(rx) => match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    self.loading = None;
                    match result {
                        Ok(consent) => self.open_popup(Popup::Install(Box::new(consent))),
                        Err(message) => self.error = Some(message),
                    }
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Closed) => {
                    self.error = Some("Install preview failed.".to_string());
                    self.pending = None;
                    self.loading = None;
                    true
                }
            },
            Pending::InstallApply(rx) => match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    self.loading = None;
                    self.pending_plugin = None;
                    self.finish_operation(result);
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Closed) => {
                    self.pending = None;
                    self.loading = None;
                    self.finish_operation(Err("Install failed.".to_string()));
                    true
                }
            },
            Pending::Uninstall(rx) => match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    self.loading = None;
                    self.pending_plugin = None;
                    match result {
                        Ok(message) => {
                            self.info = Some(message);
                            self.error = None;
                            self.reload();
                        }
                        Err(message) => self.error = Some(message),
                    }
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Closed) => {
                    self.error = Some("Uninstall failed.".to_string());
                    self.pending = None;
                    self.loading = None;
                    true
                }
            },
        }
    }

    /// The currently selected plugin row, if any. Lets an embedding surface
    /// (the settings Plugins tab) read the selection.
    pub fn selected(&self) -> Option<&crate::plugin::PluginView> {
        self.rows.get(self.selected)
    }

    /// Reflect a staged enable/disable in the displayed list without touching
    /// disk or the registry. The settings host stages the change in its own
    /// config and persists it on save, so the row shows the pending state
    /// immediately while still following the normal save flow.
    pub fn set_row_enabled(&mut self, id: &str, enabled: bool) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
            row.enabled = enabled;
        }
    }

    /// Render as a centered modal (the command-palette surface): clears a
    /// clamped sub-rect and draws into it.
    pub fn render(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width.clamp(40, 100);
        let height = area.height.clamp(12, 28);
        let rect = centered_rect(area, width, height);
        f.render_widget(Clear, rect);
        // A modal always owns the keyboard, so its border is always accent.
        self.render_into(f, rect, theme, true);
    }

    /// Render directly into the given rect, no centering or clearing, for
    /// embedding in the settings screen's Plugins category. Same manager, same
    /// state, same key handler; only the framing differs. `focused` mirrors the
    /// settings fields-pane focus so the border matches every other pane.
    pub fn render_inline(&self, f: &mut Frame, area: Rect, theme: &Theme, focused: bool) {
        self.render_into(f, area, theme, focused);
    }

    fn render_into(&self, f: &mut Frame, rect: Rect, theme: &Theme, focused: bool) {
        // Focus-aware border, matching the settings fields pane: accent when
        // the pane holds the keyboard, dim border otherwise.
        let border_color = if focused { theme.accent } else { theme.border };
        let block = Block::default()
            .title(" Plugins ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.render_browse(f, inner, theme);
        // The popup floats over the list, centered on the dialog rect.
        match &self.popup {
            Some(Popup::Review(review)) => self.render_review(f, rect, theme, review),
            Some(Popup::Install(consent)) => self.render_install_consent(f, rect, theme, consent),
            Some(Popup::Reapprove(consent)) => self.render_reapprove(f, rect, theme, consent),
            Some(Popup::ConfirmUninstall { id }) => {
                self.render_confirm_uninstall(f, rect, theme, id)
            }
            Some(Popup::Details(details)) => self.render_details(f, rect, theme, details),
            Some(Popup::Progress(progress)) => self.render_progress(f, rect, theme, progress),
            None => {}
        }
    }

    fn render_review(&self, f: &mut Frame, area: Rect, theme: &Theme, review: &Review) {
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!(
                "Update {}? v{} -> v{}",
                review.id, review.from_version, review.to_version
            ),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))];
        if review.consent.is_some() {
            lines.push(Line::from(Span::styled(
                "This update expands what the plugin can do.",
                Style::default().fg(theme.dimmed),
            )));
        }
        lines.push(Line::from(""));

        // Changelog, shown for every update.
        push_changelog_lines(&mut lines, &review.changelog, theme);
        lines.push(Line::from(""));

        let Some(consent) = &review.consent else {
            // Safe update: changelog only, with an update/cancel hint.
            let footer = vec![Line::from(Span::styled(
                "enter update · esc cancel · j/k scroll",
                Style::default().fg(theme.dimmed),
            ))];
            self.draw_popup(
                f,
                area,
                theme,
                PopupContent {
                    body: lines,
                    footer,
                    follow_tail: false,
                    title: " Update plugin ",
                },
            );
            return;
        };

        if !consent.added_capabilities.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "New capabilities: {}",
                    consent.added_capabilities.join(", ")
                ),
                Style::default().fg(theme.waiting),
            )));
        }
        if !consent.removed_capabilities.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("Removed: {}", consent.removed_capabilities.join(", ")),
                Style::default().fg(theme.dimmed),
            )));
        }
        if let Some(change) = &consent.runtime_change {
            lines.push(Line::from(Span::styled(
                format!("Runtime: {change}"),
                Style::default().fg(theme.waiting),
            )));
        }
        if consent.trust_downgrade {
            lines.push(Line::from(Span::styled(
                "No longer a verified featured plugin (community trust).",
                Style::default().fg(theme.waiting),
            )));
        }
        if !consent.build_steps.is_empty() {
            lines.push(Line::from(Span::styled(
                "Build commands (run as you, unsandboxed):",
                Style::default().fg(theme.waiting),
            )));
            for step in &consent.build_steps {
                lines.push(Line::from(Span::styled(
                    format!("  $ {step}"),
                    Style::default().fg(theme.dimmed),
                )));
            }
        }
        if !consent.ui.is_empty() {
            let mut slots: Vec<&str> = Vec::new();
            for u in &consent.ui {
                if !slots.contains(&u.slot.as_str()) {
                    slots.push(u.slot.as_str());
                }
            }
            lines.push(Line::from(Span::styled(
                format!("UI slots: {}", slots.join(", ")),
                Style::default().fg(theme.dimmed),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Approving trusts this plugin; a worker and build steps run without OS sandboxing.",
            Style::default().fg(theme.dimmed),
        )));
        let footer = vec![Line::from(Span::styled(
            "y approve · n decline · esc close · j/k scroll",
            Style::default().fg(theme.dimmed),
        ))];
        self.draw_popup(
            f,
            area,
            theme,
            PopupContent {
                body: lines,
                footer,
                follow_tail: false,
                title: " Approve update ",
            },
        );
    }

    fn render_install_consent(
        &self,
        f: &mut Frame,
        area: Rect,
        theme: &Theme,
        consent: &InstallConsent,
    ) {
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!("Install {} v{}?", consent.id, consent.version),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))];
        lines.push(Line::from(Span::styled(
            consent.notice.clone(),
            Style::default().fg(theme.dimmed),
        )));
        lines.push(Line::from(Span::styled(
            format!("Source: {} ({})", consent.source, consent.validation),
            Style::default().fg(theme.dimmed),
        )));
        if consent.unverified {
            lines.push(Line::from(Span::styled(
                "Unverified source: not an audited release (explicit ref or default branch).",
                Style::default().fg(theme.waiting),
            )));
        }
        lines.push(Line::from(""));
        if consent.capabilities.is_empty() {
            lines.push(Line::from(Span::styled(
                "No capabilities requested.",
                Style::default().fg(theme.dimmed),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "Capabilities:",
                Style::default().fg(theme.waiting),
            )));
            for cap in &consent.capabilities {
                lines.push(Line::from(Span::styled(
                    format!("  {cap}"),
                    Style::default().fg(theme.text),
                )));
            }
        }
        if !consent.build_steps.is_empty() {
            lines.push(Line::from(Span::styled(
                "Build commands (run as you, unsandboxed):",
                Style::default().fg(theme.waiting),
            )));
            for step in &consent.build_steps {
                lines.push(Line::from(Span::styled(
                    format!("  $ {step}"),
                    Style::default().fg(theme.dimmed),
                )));
            }
        }
        if !consent.ui.is_empty() {
            let mut slots: Vec<&str> = Vec::new();
            for u in &consent.ui {
                if !slots.contains(&u.slot.as_str()) {
                    slots.push(u.slot.as_str());
                }
            }
            lines.push(Line::from(Span::styled(
                format!("UI slots: {}", slots.join(", ")),
                Style::default().fg(theme.dimmed),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Approving trusts this plugin; a worker and build steps run without OS sandboxing.",
            Style::default().fg(theme.dimmed),
        )));
        let footer = vec![Line::from(Span::styled(
            "y install · n cancel · j/k scroll",
            Style::default().fg(theme.dimmed),
        ))];
        self.draw_popup(
            f,
            area,
            theme,
            PopupContent {
                body: lines,
                footer,
                follow_tail: false,
                title: " Approve install ",
            },
        );
    }

    fn render_reapprove(
        &self,
        f: &mut Frame,
        area: Rect,
        theme: &Theme,
        consent: &ReapproveConsent,
    ) {
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!("Re-approve {} v{}?", consent.id, consent.version),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))];
        lines.push(Line::from(Span::styled(
            "Its manifest changed since the last approval; it stays inactive until re-approved.",
            Style::default().fg(theme.dimmed),
        )));
        lines.push(Line::from(Span::styled(
            format!("Validation: {}", consent.validation),
            Style::default().fg(theme.dimmed),
        )));
        lines.push(Line::from(""));
        if consent.capabilities.is_empty() {
            lines.push(Line::from(Span::styled(
                "No capabilities requested.",
                Style::default().fg(theme.dimmed),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "Capabilities:",
                Style::default().fg(theme.waiting),
            )));
            for cap in &consent.capabilities {
                lines.push(Line::from(Span::styled(
                    format!("  {cap}"),
                    Style::default().fg(theme.text),
                )));
            }
        }
        if !consent.ui.is_empty() {
            let mut slots: Vec<&str> = Vec::new();
            for u in &consent.ui {
                if !slots.contains(&u.slot.as_str()) {
                    slots.push(u.slot.as_str());
                }
            }
            lines.push(Line::from(Span::styled(
                format!("UI slots: {}", slots.join(", ")),
                Style::default().fg(theme.dimmed),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "No build steps run; this only re-grants the already-installed version.",
            Style::default().fg(theme.dimmed),
        )));
        let footer = vec![Line::from(Span::styled(
            "y approve · esc cancel · j/k scroll",
            Style::default().fg(theme.dimmed),
        ))];
        self.draw_popup(
            f,
            area,
            theme,
            PopupContent {
                body: lines,
                footer,
                follow_tail: false,
                title: " Approve plugin ",
            },
        );
    }

    fn render_confirm_uninstall(&self, f: &mut Frame, area: Rect, theme: &Theme, id: &str) {
        let lines: Vec<Line> = vec![
            Line::from(Span::styled(
                format!("Uninstall {id}?"),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Removes its files, configuration, and lockfile entry. Per-session plugin data is kept.",
                Style::default().fg(theme.dimmed),
            )),
        ];
        let footer = vec![Line::from(Span::styled(
            "y uninstall · esc cancel",
            Style::default().fg(theme.dimmed),
        ))];
        self.draw_popup(
            f,
            area,
            theme,
            PopupContent {
                body: lines,
                footer,
                follow_tail: false,
                title: " Uninstall plugin ",
            },
        );
    }

    fn render_details(&self, f: &mut Frame, area: Rect, theme: &Theme, details: &Details) {
        let view = &details.view;
        let state = if !view.enabled {
            "disabled"
        } else if view.needs_reapproval {
            "needs approval"
        } else {
            "enabled"
        };
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!("{} v{} ({})", view.name, view.version, view.id),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))];
        if !view.description.is_empty() {
            lines.push(Line::from(Span::styled(
                view.description.clone(),
                Style::default().fg(theme.dimmed),
            )));
        }
        lines.push(Line::from(Span::styled(
            format!("Validation: {} · State: {state}", view.validation),
            Style::default().fg(theme.dimmed),
        )));
        match &view.source {
            Some(source) => lines.push(Line::from(Span::styled(
                format!("Source: {source}"),
                Style::default().fg(theme.dimmed),
            ))),
            None => lines.push(Line::from(Span::styled(
                "Builtin plugin (compiled into aoe).",
                Style::default().fg(theme.dimmed),
            ))),
        }
        if let Some(dir) = &details.dir {
            lines.push(Line::from(Span::styled(
                format!("Install dir: {dir}"),
                Style::default().fg(theme.dimmed),
            )));
        }
        lines.push(Line::from(""));
        if view.capabilities.is_empty() {
            lines.push(Line::from(Span::styled(
                "No capabilities requested.",
                Style::default().fg(theme.dimmed),
            )));
        } else {
            let granted = if view.granted {
                "Capabilities (granted):"
            } else {
                "Capabilities (NOT granted):"
            };
            lines.push(Line::from(Span::styled(
                granted,
                Style::default().fg(if view.granted {
                    theme.running
                } else {
                    theme.waiting
                }),
            )));
            for cap in &view.capabilities {
                lines.push(Line::from(Span::styled(
                    format!("  {cap}"),
                    Style::default().fg(theme.text),
                )));
            }
        }
        if !view.ui_contributions.is_empty() {
            lines.push(Line::from(Span::styled(
                "UI slots:",
                Style::default().fg(theme.dimmed),
            )));
            for u in &view.ui_contributions {
                lines.push(Line::from(Span::styled(
                    format!("  {} ({})", u.slot, u.id),
                    Style::default().fg(theme.text),
                )));
            }
        }
        if !details.commands.is_empty() {
            lines.push(Line::from(Span::styled(
                "Commands:",
                Style::default().fg(theme.dimmed),
            )));
            for command in &details.commands {
                lines.push(Line::from(Span::styled(
                    format!("  {command}"),
                    Style::default().fg(theme.text),
                )));
            }
        }
        if !details.keybinds.is_empty() {
            lines.push(Line::from(Span::styled(
                "Keybinds:",
                Style::default().fg(theme.dimmed),
            )));
            for keybind in &details.keybinds {
                lines.push(Line::from(Span::styled(
                    format!("  {keybind}"),
                    Style::default().fg(theme.text),
                )));
            }
        }
        match &details.runtime {
            Some(runtime) => lines.push(Line::from(Span::styled(
                format!("Runtime: {runtime}"),
                Style::default().fg(theme.dimmed),
            ))),
            None => lines.push(Line::from(Span::styled(
                "Runtime: none (no worker).",
                Style::default().fg(theme.dimmed),
            ))),
        }
        if !details.settings.is_empty() {
            lines.push(Line::from(Span::styled(
                "Settings:",
                Style::default().fg(theme.dimmed),
            )));
            for setting in &details.settings {
                lines.push(Line::from(Span::styled(
                    format!("  {setting}"),
                    Style::default().fg(theme.text),
                )));
            }
        }
        let footer = vec![Line::from(Span::styled(
            "j/k scroll · esc close",
            Style::default().fg(theme.dimmed),
        ))];
        self.draw_popup(
            f,
            area,
            theme,
            PopupContent {
                body: lines,
                footer,
                follow_tail: false,
                title: " Plugin details ",
            },
        );
    }

    fn render_progress(&self, f: &mut Frame, area: Rect, theme: &Theme, progress: &Progress) {
        let mut lines: Vec<Line> = Vec::new();
        for line in read_log_tail(&progress.log_path, PROGRESS_TAIL_LINES) {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(theme.dimmed),
            )));
        }
        // Status and keys live in the pinned footer so a long log tail (or a
        // long error) can never push them off screen.
        let mut footer: Vec<Line> = Vec::new();
        match &progress.done {
            None => {
                footer.push(Line::from(Span::styled(
                    "Working…",
                    Style::default().fg(theme.waiting),
                )));
                footer.push(Line::from(Span::styled(
                    format!(
                        "esc hide (keeps running) · log: {}",
                        progress.log_path.display()
                    ),
                    Style::default().fg(theme.dimmed),
                )));
            }
            Some(Ok(message)) => {
                footer.push(Line::from(Span::styled(
                    message.clone(),
                    Style::default().fg(theme.running),
                )));
                footer.push(Line::from(Span::styled(
                    "esc close",
                    Style::default().fg(theme.dimmed),
                )));
            }
            Some(Err(message)) => {
                footer.push(Line::from(Span::styled(
                    message.clone(),
                    Style::default().fg(theme.error),
                )));
                footer.push(Line::from(Span::styled(
                    format!("Full log: {}", progress.log_path.display()),
                    Style::default().fg(theme.dimmed),
                )));
                footer.push(Line::from(Span::styled(
                    "esc close",
                    Style::default().fg(theme.dimmed),
                )));
            }
        }
        // While the operation runs, follow the newest log rows (unless the
        // user scrolled away themselves); once done, the last position keeps
        // the outcome context in view.
        let follow = progress.done.is_none() && !self.popup_user_scrolled;
        self.draw_popup(
            f,
            area,
            theme,
            PopupContent {
                body: lines,
                footer,
                follow_tail: follow,
                title: &progress.title,
            },
        );
    }

    /// Draw a popup as a scrollable body above a pinned footer (the decision
    /// keys / status), both word-wrapped, in a clamped centered sub-rect.
    /// Sizing and the scroll bound are computed from wrapped (visual) rows,
    /// not logical line counts, so a wrapping body can never push the footer
    /// off screen and the last body row is always reachable by scrolling.
    fn draw_popup(&self, f: &mut Frame, area: Rect, theme: &Theme, content: PopupContent) {
        let PopupContent {
            body,
            footer,
            follow_tail,
            title,
        } = content;
        // A tiny terminal can be narrower/shorter than our preferred size;
        // never pass clamp/centered_rect a max below the min (it panics).
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width.clamp(1, 72);
        let inner_width = width.saturating_sub(2).max(1);
        let body_rows = wrapped_rows_total(&body, inner_width);
        // The footer is pinned in full, but a pathological one (a long error
        // chain) may never starve the body of its half of the popup.
        let footer_rows = wrapped_rows_total(&footer, inner_width)
            .min((area.height.saturating_sub(2) / 2).max(1));
        let height = body_rows
            .saturating_add(footer_rows)
            .saturating_add(2)
            .clamp(1, area.height);
        let rect = centered_rect(area, width, height);
        f.render_widget(Clear, rect);
        let block = Block::default()
            .title(title.to_string())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let footer_rows = if footer.is_empty() {
            0
        } else {
            footer_rows.min(inner.height)
        };
        let body_height = inner.height.saturating_sub(footer_rows);
        // Clamp the scroll so the last body row can always be brought into
        // view but never scrolled far past; `Cell` because render is `&self`.
        // One slack row absorbs any wrap-estimation drift, erring toward
        // reachable rather than clipped.
        let max_scroll = if body_rows > body_height {
            (body_rows - body_height).saturating_add(1)
        } else {
            0
        };
        // A following popup (the running progress log) pins the view to the
        // newest rows; writing it back to the Cell means a later manual `k`
        // scrolls up from the bottom, not from wherever the offset last was.
        if follow_tail {
            self.popup_scroll.set(max_scroll);
        }
        if self.popup_scroll.get() > max_scroll {
            self.popup_scroll.set(max_scroll);
        }
        if body_height > 0 {
            let body_area = Rect {
                height: body_height,
                ..inner
            };
            let body = Paragraph::new(body)
                .wrap(Wrap { trim: true })
                .scroll((self.popup_scroll.get(), 0));
            f.render_widget(body, body_area);
        }
        if footer_rows > 0 {
            let footer_area = Rect {
                y: inner.y + body_height,
                height: footer_rows,
                ..inner
            };
            f.render_widget(
                Paragraph::new(footer).wrap(Wrap { trim: true }),
                footer_area,
            );
        }
    }

    fn render_browse(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(if self.load_errors.is_empty() { 0 } else { 2 }),
                Constraint::Length(2),
            ])
            .split(area);

        if self.mode == Mode::Discover {
            self.render_discover_list(f, chunks[0], theme);
            self.render_footer(f, chunks[2], theme);
            return;
        }

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| {
                let state = if !row.enabled {
                    ("disabled", theme.dimmed)
                } else if row.needs_reapproval {
                    // Waiting on the user to re-approve, not failed: use the
                    // attention-needed color, not the error color.
                    ("needs approval", theme.waiting)
                } else {
                    ("enabled", theme.running)
                };
                let mut spans = vec![
                    Span::styled(
                        format!("{:<28}", format!("{} v{}", row.name, row.version)),
                        Style::default().fg(theme.text),
                    ),
                    Span::styled(
                        format!("{:<10}", row.validation),
                        Style::default().fg(theme.dimmed),
                    ),
                    Span::styled(format!("{:<14}", state.0), Style::default().fg(state.1)),
                ];
                // Mark a row whose last `c` check found a newer version.
                if self.updates.get(&row.id).is_some_and(|u| u.needs_update) {
                    spans.push(Span::styled("update! ", Style::default().fg(theme.accent)));
                }
                // Disclose the dashboard UI slots the plugin renders into, so the
                // manager shows that a plugin modifies the UI (#2366). Distinct
                // slot names only; ids are in the details popup (Enter).
                if !row.ui_contributions.is_empty() {
                    let mut slots: Vec<&str> = Vec::new();
                    for u in &row.ui_contributions {
                        if !slots.contains(&u.slot.as_str()) {
                            slots.push(u.slot.as_str());
                        }
                    }
                    spans.push(Span::styled(
                        format!("ui: {}", slots.join(", ")),
                        Style::default().fg(theme.dimmed),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        let mut state = ListState::default();
        state.select(if self.rows.is_empty() {
            None
        } else {
            Some(self.selected)
        });
        f.render_stateful_widget(list, chunks[0], &mut state);

        if !self.load_errors.is_empty() {
            let errors = Paragraph::new(self.load_errors.join("; "))
                .style(Style::default().fg(theme.error))
                .wrap(Wrap { trim: true });
            f.render_widget(errors, chunks[1]);
        }

        self.render_footer(f, chunks[2], theme);
    }

    fn render_discover_list(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        // The search line renders whenever a query exists or is being edited,
        // so the list always shows what filtered it.
        let show_query = self.query_editing || !self.discover_query.is_empty();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(if show_query { 1 } else { 0 }),
                Constraint::Min(1),
            ])
            .split(area);
        if show_query {
            let (text, color) = if self.query_editing {
                (format!("Search: {}▌", self.discover_query), theme.accent)
            } else {
                (format!("Search: {}", self.discover_query), theme.dimmed)
            };
            f.render_widget(
                Paragraph::new(text).style(Style::default().fg(color)),
                chunks[0],
            );
        }
        let list_area = chunks[1];
        if self.discover_rows.is_empty() {
            let empty = Paragraph::new("No plugins found on the aoe-plugin topic.")
                .style(Style::default().fg(theme.dimmed));
            f.render_widget(empty, list_area);
            return;
        }
        let items: Vec<ListItem> = self
            .discover_rows
            .iter()
            .map(|r| {
                let spans = vec![
                    Span::styled(
                        format!("{:<10}", r.badge.as_str()),
                        Style::default().fg(theme.accent),
                    ),
                    Span::styled(
                        format!("{:<6}", format!("★{}", r.stars)),
                        Style::default().fg(theme.dimmed),
                    ),
                    Span::styled(format!("{:<30}", r.slug), Style::default().fg(theme.text)),
                    Span::styled(
                        r.description.clone().unwrap_or_default(),
                        Style::default().fg(theme.dimmed),
                    ),
                ];
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        let mut state = ListState::default();
        state.select(Some(self.discover_selected));
        f.render_stateful_widget(list, list_area, &mut state);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        // A running task wins the footer; then a transient error/info; then the
        // mode-appropriate key hints.
        let (text, color) = if let Some(loading) = self.loading {
            (loading.to_string(), theme.waiting)
        } else if let Some(e) = self.error.as_deref() {
            (e.to_string(), theme.error)
        } else if let Some(i) = self.info.as_deref() {
            (i.to_string(), theme.running)
        } else if self.mode == Mode::Discover {
            if self.query_editing {
                (
                    "type query · enter search · esc cancel".to_string(),
                    theme.dimmed,
                )
            } else {
                (
                    "enter install · / search · d re-search · esc back".to_string(),
                    theme.dimmed,
                )
            }
        } else {
            let back = if self.embedded {
                "esc back"
            } else {
                "esc close"
            };
            // Contextual hints for the selected row keep the footer short:
            // update / approve / uninstall only apply to some rows.
            let mut hints = vec![
                "space toggle",
                "enter details",
                "d discover",
                "c updates",
                "r refresh",
            ];
            if let Some(row) = self.rows.get(self.selected) {
                if self.updates.get(&row.id).is_some_and(|u| u.needs_update) {
                    hints.push("u update");
                }
                if row.needs_reapproval {
                    hints.push("a approve");
                }
                if !row.builtin {
                    hints.push("x uninstall");
                }
            }
            if self.embedded && self.has_settings_pane {
                hints.push("tab settings");
            }
            hints.push(back);
            (hints.join(" · "), theme.dimmed)
        };
        let footer = Paragraph::new(text)
            .style(Style::default().fg(color))
            .wrap(Wrap { trim: true });
        f.render_widget(footer, area);
    }
}

#[cfg(test)]
mod wrapped_rows_tests {
    use super::{wrapped_rows, wrapped_rows_total};
    use ratatui::prelude::*;

    fn line(s: &str) -> Line<'static> {
        Line::from(s.to_string())
    }

    #[test]
    fn short_and_empty_lines_take_one_row() {
        assert_eq!(wrapped_rows(&line(""), 20), 1);
        assert_eq!(wrapped_rows(&line("hello"), 20), 1);
        assert_eq!(wrapped_rows(&line("fits the row width"), 18), 1);
    }

    #[test]
    fn word_wrap_counts_continuation_rows() {
        // "alpha bravo" (11) into width 7: "alpha" / "bravo".
        assert_eq!(wrapped_rows(&line("alpha bravo"), 7), 2);
        assert_eq!(wrapped_rows(&line("alpha bravo charlie"), 7), 3);
    }

    #[test]
    fn indentation_counts_toward_the_first_row() {
        // Two leading spaces + "abcdef" into width 6 needs a wrap that the
        // unindented text would not.
        assert_eq!(wrapped_rows(&line("abcdef"), 6), 1);
        assert_eq!(wrapped_rows(&line("  abcdef"), 6), 2);
    }

    #[test]
    fn over_wide_word_splits_across_rows() {
        // A 20-wide token into width 8 needs three rows on its own.
        assert_eq!(wrapped_rows(&line("aaaaaaaaaaaaaaaaaaaa"), 8), 3);
        // After a word already on the row, the split starts on a fresh row.
        assert_eq!(wrapped_rows(&line("hi aaaaaaaaaaaaaaaaaaaa"), 8), 4);
    }

    #[test]
    fn totals_sum_per_line() {
        let lines = [line("alpha bravo"), line(""), line("x")];
        assert_eq!(wrapped_rows_total(&lines, 7), 4);
    }
}

//! Plugin manager: list plugins with their trust and enabled state, toggle
//! them (reconciling a running daemon's workers live), inspect a plugin's full
//! disclosure, and run the external-plugin lifecycle in-TUI (install from
//! GitHub discovery, update, re-approve a stale grant, uninstall), each behind
//! the consent popup the CLI and web modals render. The twin of `aoe plugin`.

use std::cell::{Cell, RefCell};
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
use crate::tui::components::hint_buttons::ListMouse;
use crate::tui::styles::Theme;

/// An open update review popup. Every update shows its changelog; `consent` is
/// `Some` only when it also expands access, adding the disclosures and Decline.
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

/// A lifecycle operation whose log file the popup tails.
struct Progress {
    title: String,
    log_path: PathBuf,
    daemon_job: Option<String>,
    daemon_log: Option<String>,
    /// `None` while running; the final outcome line once done.
    done: Option<Result<String, String>>,
}

/// A scrollable body over a pinned footer, and whether the body follows its
/// tail as it grows.
struct PopupContent<'a> {
    body: Vec<Line<'a>>,
    footer: Vec<Line<'a>>,
    follow_tail: bool,
    title: &'a str,
}

/// The floating popup owning the keyboard; at most one at a time.
enum Popup {
    Review(Box<Review>),
    Install(Box<InstallConsent>),
    Reapprove(ReapproveConsent),
    ConfirmUninstall { id: String },
    Details(Box<Details>),
    Progress(Progress),
}

/// The installed list, or GitHub discovery results.
#[derive(PartialEq, Eq)]
enum Mode {
    Browse,
    Discover,
}

/// A network task polled by [`PluginManagerDialog::tick`], spawned so a dead
/// remote cannot freeze the UI.
enum Pending {
    Updates(oneshot::Receiver<Vec<UpdateStatus>>),
    Discover(oneshot::Receiver<Result<Vec<DiscoveryResult>, String>>),
    Preview(oneshot::Receiver<Result<(crate::acp::client::HttpClient, UpdatePreview), String>>),
    Apply(tokio::sync::mpsc::UnboundedReceiver<UpdateEvent>),
    /// An enable/disable; the Ok string says whether the daemon reconciled.
    Toggle(oneshot::Receiver<Result<String, String>>),
    InstallPreview(oneshot::Receiver<Result<InstallConsent, String>>),
    InstallApply(oneshot::Receiver<Result<String, String>>),
    Uninstall(oneshot::Receiver<Result<String, String>>),
}

enum UpdateEvent {
    Accepted(String),
    Log(String),
    Finished(Result<String, String>),
}

/// Submit exactly once, then observe on the client that produced consent.
async fn observe_update(
    client: &crate::acp::client::HttpClient,
    id: &str,
    fingerprint: Option<String>,
    events: &tokio::sync::mpsc::UnboundedSender<UpdateEvent>,
) -> Result<String, String> {
    use crate::acp::client::HttpError;
    let accepted = match client
        .update_plugin(
            id,
            &crate::daemon::ApplyUpdateBody {
                expected_fingerprint: fingerprint,
            },
        )
        .await
    {
        Ok(accepted) => accepted,
        Err(error) => {
            let refused = matches!(
                &error,
                HttpError::Unauthorized
                    | HttpError::ReadOnly
                    | HttpError::LocalPluginSourceRequired
                    | HttpError::PluginJobActive
                    | HttpError::Daemon(crate::daemon::DaemonClientError::InvalidPathSegment)
            ) || matches!(&error, HttpError::Server { status, .. }
                if status.is_client_error() && *status != reqwest::StatusCode::REQUEST_TIMEOUT);
            return Err(if refused {
                format!("Update refused: {error}")
            } else {
                format!("Update outcome unknown: {error}. Inspect the same local Daemon; no operation was resubmitted.")
            });
        }
    };
    let job = accepted.job_id;
    let _ = events.send(UpdateEvent::Accepted(job.clone()));
    loop {
        let response = client.plugin_job(&job).await.map_err(|error| format!(
            "Job {job} outcome unknown: {error}. Run `env -u AOE_DAEMON_URL aoe plugin job {job}` on the same local Daemon host; no operation was resubmitted."
        ))?;
        let _ = events.send(UpdateEvent::Log(response.log.tail));
        match response.job.status {
            crate::daemon::PluginJobStatus::Running => {}
            crate::daemon::PluginJobStatus::Succeeded => return Ok(format!(
                "Updated {id}; Daemon job {job} finished. Worker restart requested; readiness is not implied."
            )),
            crate::daemon::PluginJobStatus::Failed { error } =>
                return Err(format!("Daemon job {job} failed: {error}")),
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

pub struct PluginManagerDialog {
    /// The shared view-model the web dashboard also renders from, built off
    /// the registry so the TUI re-derives no plugin fields.
    rows: Vec<crate::plugin::PluginView>,
    load_errors: Vec<String>,
    selected: usize,
    error: Option<String>,
    info: Option<String>,
    /// Set when on-disk plugin config changes, for an embedding surface to
    /// pick up with [`Self::take_mutated`].
    mutated: bool,
    /// Hosted inside the settings screen, so Esc returns to the categories.
    embedded: bool,
    /// Set when a plugin-settings pane renders beneath, so the footer can
    /// advertise the Tab sub-focus.
    has_settings_pane: bool,
    mode: Mode,
    pending: Option<Pending>,
    loading: Option<&'static str>,
    /// Update statuses from the last `c` check, driving the per-row marker.
    updates: HashMap<String, UpdateStatus>,
    discover_rows: Vec<DiscoveryResult>,
    discover_selected: usize,
    discover_query: String,
    query_editing: bool,
    /// The plugin a preview or apply is running for, so `tick` can place it.
    pending_plugin: Option<String>,
    popup: Option<Popup>,
    update_client: Option<crate::acp::client::HttpClient>,
    /// Scroll offset into the popup body. A `Cell` so render can clamp it to
    /// the content height only render knows.
    popup_scroll: Cell<u16>,
    /// Once the user scrolls, a following popup stops chasing its tail.
    popup_user_scrolled: bool,
    /// Recorded through a `RefCell`: the settings screen renders by `&self`.
    mouse: RefCell<ListMouse>,
}

impl Default for PluginManagerDialog {
    fn default() -> Self {
        Self::new()
    }
}

/// Changelog lines the review popup renders before linking out for the rest.
const MAX_CHANGELOG_LINES: usize = 60;

/// How many trailing log lines the progress popup tails.
const PROGRESS_TAIL_LINES: usize = 30;

/// How far back the tail reads; build output can grow to megabytes.
const PROGRESS_TAIL_BYTES: u64 = 16 * 1024;

/// Append the changelog: release notes, commit subjects, or a single
/// "unavailable" line. Capped at [`MAX_CHANGELOG_LINES`], with `more_url` for
/// the rest.
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
    // Link the full history when clipped here or upstream.
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

/// Where a TUI-run lifecycle operation writes build output, beside the
/// dashboard's job logs.
fn tui_job_log(op: &str, id: &str) -> anyhow::Result<PathBuf> {
    Ok(crate::plugin::plugins_dir()?
        .join("jobs")
        .join(format!("tui-{op}-{id}.log")))
}

/// Last `max_lines` lines of a log file, reading at most
/// [`PROGRESS_TAIL_BYTES`] from its end; empty while the file is absent.
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

/// Rows `line` occupies under `Wrap { trim: true }` at `width` columns. Popup
/// sizing and scroll bounds use it, so a wrapped line can never push the
/// decision-key footer off the bottom edge.
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
        aoe_plugin_api::SettingType::DynamicSelect => "dynamic_select",
        aoe_plugin_api::SettingType::ObjectList => "object_list",
        aoe_plugin_api::SettingType::Cron => "cron",
        aoe_plugin_api::SettingType::StringList => "string_list",
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
            update_client: None,
            popup_scroll: Cell::new(0),
            popup_user_scrolled: false,
            mouse: RefCell::default(),
        };
        dialog.reload();
        dialog.mutated = false; // Initial load is not a user mutation.
        dialog
    }

    /// A manager hosted inside the settings screen; only the footer differs.
    pub fn embedded() -> Self {
        let mut dialog = Self::new();
        dialog.embedded = true;
        dialog
    }

    /// Take and clear the "config mutated" flag.
    pub fn take_mutated(&mut self) -> bool {
        std::mem::take(&mut self.mutated)
    }

    /// Whether the dialog owns every key (an open popup, or discover mode).
    /// The settings host checks it before intercepting Space.
    pub fn captures_input(&self) -> bool {
        self.popup.is_some() || self.mode == Mode::Discover
    }

    pub fn set_has_settings_pane(&mut self, has: bool) {
        self.has_settings_pane = has;
    }

    /// Height the embedded manager wants, so the settings host can size the
    /// master-detail split to the rows rather than to half the pane.
    pub fn preferred_inline_height(&self) -> u16 {
        let errors: u16 = if self.load_errors.is_empty() { 0 } else { 2 };
        (self.rows.len().max(1) as u16)
            .saturating_add(2) // borders
            .saturating_add(2) // footer
            .saturating_add(errors)
    }

    /// Select the row owning a `plugin:<id>.<field>` ident, so a settings
    /// search jump lands on the right plugin. True when a row matched.
    pub fn select_plugin_owning_ident(&mut self, ident: &str) -> bool {
        let Some(rest) =
            ident.strip_prefix(crate::session::config::settings_schema::PLUGIN_SECTION_PREFIX)
        else {
            return false;
        };
        // Plugin ids are dotted, so match "<id>." as a prefix.
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
        // Only a config-mutating action reloads, so flag the mutation here.
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

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<KeyEvent> {
        if let Some(key) = self.mouse.get_mut().hint_at(col, row) {
            return Some(key);
        }
        if self.popup.is_some() || self.query_editing {
            return None;
        }
        let len = self.list_len();
        let selected = if self.mode == Mode::Discover {
            &mut self.discover_selected
        } else {
            &mut self.selected
        };
        self.mouse.get_mut().click_row(col, row, len, selected)
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let (len, rows_live) = (self.list_len(), self.popup.is_none());
        self.mouse.get_mut().handle_hover(col, row, len, rows_live)
    }

    fn list_len(&self) -> usize {
        if self.mode == Mode::Discover {
            self.discover_rows.len()
        } else {
            self.rows.len()
        }
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
            // On-demand network actions; a second press while one is in
            // flight is ignored.
            KeyCode::Char('c') => {
                self.start_update_check();
                DialogResult::Continue
            }
            KeyCode::Char('d') => {
                self.start_discover();
                DialogResult::Continue
            }
            // Only when the last `c` check found an update available.
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
            // An external `aoe plugin` may have changed it meanwhile.
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
                // Once done any decision key dismisses it. While it runs, Esc
                // hides the popup without cancelling, so a hung fetch cannot
                // trap the keyboard; the result then lands in the footer.
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

    /// Keys for the update review popup. The caller took it out of
    /// `self.popup`; put it back unless the key decided it.
    fn handle_review_key(&mut self, key: KeyEvent, review: Review) -> DialogResult<()> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.start_apply(review.id, Some(review.fingerprint));
                DialogResult::Continue
            }
            // Decline records the dismissal and keeps the active version. A
            // safe update has nothing to dismiss, so `n` just closes.
            KeyCode::Char('n') => {
                if review.consent.is_some() {
                    match crate::plugin::install::dismiss_update(&review.id, &review.fingerprint) {
                        Ok(()) => {
                            // Flag the config write so an embedding surface
                            // resyncs and cannot clobber the dismissal.
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

    /// Keep displayed enable-state unchanged until the operation succeeds.
    /// An RPC error can leave the remote outcome unknown.
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

    /// Open the full disclosure for the selected row.
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

    /// Re-approval consent for a grant that no longer covers the manifest.
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
            let result = async {
                let endpoint = crate::acp::client::daemon_manager::require_local_read_daemon()
                    .await
                    .map_err(|e| format!("Update refused: {e}"))?;
                let client = crate::acp::client::HttpClient::new(endpoint)
                    .map_err(|e| format!("Update refused: {e}"))?;
                let preview = client
                    .preview_plugin_update(&preview_id)
                    .await
                    .map_err(|e| format!("Update preview refused: {e}"))?;
                Ok((client, preview))
            }
            .await;
            let _ = tx.send(result);
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
        let Some(client) = self.update_client.clone() else {
            self.error = Some("Update refused: review it again on the local Daemon.".into());
            return;
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let apply_id = id.clone();
        tokio::spawn(async move {
            let result = observe_update(&client, &apply_id, fingerprint, &tx).await;
            let _ = tx.send(UpdateEvent::Finished(result));
        });
        self.pending_plugin = Some(id.clone());
        self.pending = Some(Pending::Apply(rx));
        self.open_popup(Popup::Progress(Progress {
            title: format!(" Updating {id} "),
            log_path: PathBuf::new(),
            daemon_job: None,
            daemon_log: Some(String::new()),
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

    /// Probe the selected discovery result for its consent disclosure.
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
                crate::plugin::install::preview_install(&source, false)
                    .await
                    .map_err(|e| format!("{e:#}")),
            );
        });
        self.pending = Some(Pending::InstallPreview(rx));
        self.loading = Some("Fetching plugin…");
        self.error = None;
    }

    /// Apply an approved install, pinned to the fingerprint the popup showed.
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
                crate::plugin::install::apply_install(&source, &fingerprint, false, log)
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
            daemon_job: None,
            daemon_log: None,
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

    /// Land a finished task in the open progress popup, or the footer.
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

    /// Poll an in-flight task; true when the result landed.
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
                    // outdated() skips builtins, so an empty result means no
                    // external plugins; "all up to date" would read as if the
                    // builtin rows had been checked.
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
                    let result = result.map(|(client, preview)| {
                        self.update_client = Some(client);
                        preview
                    });
                    match result {
                        Ok(UpdatePreview::NoUpdate { .. }) => {
                            self.info = Some("Already up to date.".to_string());
                        }
                        Ok(UpdatePreview::SafeUpdate {
                            to_version,
                            fingerprint,
                            changelog,
                            ..
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
                        // A dismissed version resurfaces only when a newer
                        // one appears.
                        Ok(UpdatePreview::ConsentRequired {
                            consent, dismissed, ..
                        }) => {
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
                Ok(UpdateEvent::Finished(result)) => {
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
                Ok(UpdateEvent::Accepted(id)) => {
                    self.info = Some(format!("Accepted job {id}; observe with `env -u AOE_DAEMON_URL aoe plugin job {id}` on the local host."));
                    if let Some(Popup::Progress(progress)) = &mut self.popup {
                        progress.daemon_job = Some(id);
                    }
                    true
                }
                Ok(UpdateEvent::Log(tail)) => {
                    if let Some(Popup::Progress(progress)) = &mut self.popup {
                        progress.daemon_log = Some(tail);
                    }
                    true
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => false,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.loading = None;
                    self.finish_operation(Err("Update outcome unknown; inspect jobs on the same local Daemon. No operation was resubmitted.".to_string()));
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

    /// The selected plugin row, for an embedding surface to read.
    pub fn selected(&self) -> Option<&crate::plugin::PluginView> {
        self.rows.get(self.selected)
    }

    /// Show a staged enable/disable without touching disk: the settings host
    /// persists it on save, so the row can reflect it at once.
    pub fn set_row_enabled(&mut self, id: &str, enabled: bool) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
            row.enabled = enabled;
        }
    }

    /// Render as a centered modal into a cleared, clamped sub-rect.
    pub fn render(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width.clamp(40, 100);
        let height = area.height.clamp(12, 28);
        let rect = centered_rect(area, width, height);
        f.render_widget(Clear, rect);
        self.render_into(f, rect, theme, true);
    }

    /// Render into `area` without centering or clearing, for the settings
    /// Plugins category. `focused` mirrors the fields-pane focus.
    pub fn render_inline(&self, f: &mut Frame, area: Rect, theme: &Theme, focused: bool) {
        self.render_into(f, area, theme, focused);
    }

    fn render_into(&self, f: &mut Frame, rect: Rect, theme: &Theme, focused: bool) {
        let border_color = if focused { theme.accent } else { theme.border };
        let block = Block::default()
            .title(" Plugins ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .padding(Padding::horizontal(1));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.mouse.borrow_mut().reset();
        self.render_browse(f, inner, theme);
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
        self.mouse
            .borrow()
            .paint_hover(f, theme, self.list_len(), self.popup.is_none());
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
        if !consent.agent_integrations.is_empty() {
            lines.push(Line::from(Span::styled(
                "Agent launch commands (run as you when launched):",
                Style::default().fg(theme.waiting),
            )));
            for agent in &consent.agent_integrations {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}: {}",
                        agent.tool,
                        agent
                            .command
                            .as_deref()
                            .map(crate::plugin::agent_integration::display_command)
                            .unwrap_or_else(|| "No launch command declared".to_string())
                    ),
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
            "Approving trusts this plugin; workers, agent commands and build steps run without OS sandboxing.",
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
        if !consent.agent_integrations.is_empty() {
            lines.push(Line::from(Span::styled(
                "Agent launch commands (run as you when launched):",
                Style::default().fg(theme.waiting),
            )));
            for agent in &consent.agent_integrations {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}: {}",
                        agent.tool,
                        agent
                            .command
                            .as_deref()
                            .map(crate::plugin::agent_integration::display_command)
                            .unwrap_or_else(|| "No launch command declared".to_string())
                    ),
                    Style::default().fg(theme.dimmed),
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Installing trusts this plugin; workers, agent commands and build steps run without OS sandboxing.",
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
        if !consent.agent_integrations.is_empty() {
            lines.push(Line::from(Span::styled(
                "Agent launch commands (run as you when launched):",
                Style::default().fg(theme.waiting),
            )));
            for agent in &consent.agent_integrations {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}: {}",
                        agent.tool,
                        agent
                            .command
                            .as_deref()
                            .map(crate::plugin::agent_integration::display_command)
                            .unwrap_or_else(|| "No launch command declared".to_string())
                    ),
                    Style::default().fg(theme.dimmed),
                )));
            }
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
        if let Some(id) = &progress.daemon_job {
            lines.push(Line::from(format!(
                "Accepted job {id}; `env -u AOE_DAEMON_URL aoe plugin job {id}`"
            )));
        }
        let tail = match &progress.daemon_log {
            Some(tail) => tail.lines().map(str::to_owned).collect(),
            None => read_log_tail(&progress.log_path, PROGRESS_TAIL_LINES),
        };
        for line in tail {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(theme.dimmed),
            )));
        }
        // Pinned, so a long log tail cannot push them off screen.
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
                        progress
                            .daemon_job
                            .as_deref()
                            .map(|_| "local Daemon job".to_owned())
                            .unwrap_or_else(|| progress.log_path.display().to_string())
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
                    format!(
                        "Full log: {}",
                        progress
                            .daemon_job
                            .as_deref()
                            .map(|id| format!(
                                "env -u AOE_DAEMON_URL aoe plugin job {id} (same local host)"
                            ))
                            .unwrap_or_else(|| progress.log_path.display().to_string())
                    ),
                    Style::default().fg(theme.dimmed),
                )));
                footer.push(Line::from(Span::styled(
                    "esc close",
                    Style::default().fg(theme.dimmed),
                )));
            }
        }
        // Follow the newest rows while it runs, unless the user scrolled away.
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

    /// Draw a scrollable body above a pinned footer, both word-wrapped, in a
    /// clamped centered sub-rect. Sizing and the scroll bound count wrapped
    /// rows, so the footer stays on screen and every body row is reachable.
    fn draw_popup(&self, f: &mut Frame, area: Rect, theme: &Theme, content: PopupContent) {
        let PopupContent {
            body,
            footer,
            follow_tail,
            title,
        } = content;
        // A tiny terminal can be smaller than the preferred size, and a max
        // below the min panics.
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width.clamp(1, 72);
        let inner_width = width.saturating_sub(2).max(1);
        let body_rows = wrapped_rows_total(&body, inner_width);
        // The footer is pinned in full but never starves the body.
        let footer_rows = wrapped_rows_total(&footer, inner_width)
            .min((area.height.saturating_sub(2) / 2).max(1));
        let height = body_rows
            .saturating_add(footer_rows)
            .saturating_add(2)
            .clamp(1, area.height);
        let rect = centered_rect(area, width, height);
        f.render_widget(Clear, rect);
        // The popup owns the keyboard, so only its own hints are clickable.
        self.mouse.borrow_mut().clear_hints();
        let block = Block::default()
            .title(title.to_string())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .padding(Padding::horizontal(1));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let footer_rows = if footer.is_empty() {
            0
        } else {
            footer_rows.min(inner.height)
        };
        let body_height = inner.height.saturating_sub(footer_rows);
        // Clamp so the last body row is reachable but not far overscrolled,
        // with one slack row for wrap-estimation drift.
        let max_scroll = if body_rows > body_height {
            (body_rows - body_height).saturating_add(1)
        } else {
            0
        };
        // A following popup pins to the newest rows, and writing that back
        // means a later `k` scrolls up from the bottom.
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
            self.mouse
                .borrow_mut()
                .record_hints(f.buffer_mut(), footer_area);
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
                    // Waiting on re-approval, not failed.
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
                        Style::default().fg(if row.validation == "dev" {
                            theme.waiting
                        } else {
                            theme.dimmed
                        }),
                    ),
                    Span::styled(format!("{:<14}", state.0), Style::default().fg(state.1)),
                ];
                // Mark a row whose last `c` check found a newer version.
                if self.updates.get(&row.id).is_some_and(|u| u.needs_update) {
                    spans.push(Span::styled("update! ", Style::default().fg(theme.accent)));
                }
                // Distinct slot names only; ids are in the details popup.
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
        self.record_list(chunks[0], state.offset());

        if !self.load_errors.is_empty() {
            let errors = Paragraph::new(self.load_errors.join("; "))
                .style(Style::default().fg(theme.error))
                .wrap(Wrap { trim: true });
            f.render_widget(errors, chunks[1]);
        }

        self.render_footer(f, chunks[2], theme);
    }

    fn render_discover_list(&self, f: &mut Frame, area: Rect, theme: &Theme) {
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
        self.record_list(list_area, state.offset());
    }

    fn record_list(&self, area: Rect, offset: usize) {
        self.mouse.borrow_mut().record_list(area, offset);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        // A running task wins, then a transient message, then the key hints.
        let showing_hints = self.loading.is_none() && self.error.is_none() && self.info.is_none();
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
            // Update / approve / uninstall only apply to some rows.
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
        if showing_hints {
            self.mouse.borrow_mut().record_hints(f.buffer_mut(), area);
        }
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
    fn wrapped_rows_cases() {
        for (text, width, rows) in [
            ("", 20, 1),
            ("hello", 20, 1),
            ("fits the row width", 18, 1),
            ("alpha bravo", 7, 2),
            ("alpha bravo charlie", 7, 3),
            // Indentation counts toward the first row.
            ("abcdef", 6, 1),
            ("  abcdef", 6, 2),
            // An over-wide word splits; after another word it starts fresh.
            ("aaaaaaaaaaaaaaaaaaaa", 8, 3),
            ("hi aaaaaaaaaaaaaaaaaaaa", 8, 4),
        ] {
            assert_eq!(wrapped_rows(&line(text), width), rows, "{text:?} @ {width}");
        }
        let lines = [line("alpha bravo"), line(""), line("x")];
        assert_eq!(wrapped_rows_total(&lines, 7), 4);
    }
}

#[cfg(test)]
mod mouse_tests {
    use super::*;
    use crate::tui::dialogs::test_render::{draw, find};

    fn result(slug: &str) -> DiscoveryResult {
        DiscoveryResult {
            slug: slug.to_string(),
            html_url: String::new(),
            description: None,
            stars: 0,
            badge: DiscoveryBadge::Unvetted,
            featured: false,
            install_command: String::new(),
            source_avatar_url: String::new(),
        }
    }

    #[test]
    #[serial_test::serial]
    fn rows_select_then_open_and_only_the_open_popup_s_hints_click() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());
        let mut d = PluginManagerDialog::new();
        d.mode = Mode::Discover;
        d.discover_rows = vec![result("owner/alpha"), result("owner/beta")];

        let buf = draw(100, 30, |f, theme| d.render(f, f.area(), theme));
        let (x, y) = find(&buf, "owner/beta");
        assert!(d.handle_hover(x, y));
        assert_eq!(d.discover_selected, 0, "hover only tints");
        assert_eq!(d.handle_click(x, y), None, "the first click selects");
        assert_eq!(d.discover_selected, 1);
        assert_eq!(d.handle_click(x, y).map(|k| k.code), Some(KeyCode::Enter));
        let (x, y) = find(&buf, "/ search");
        assert_eq!(
            d.handle_click(x, y).map(|k| k.code),
            Some(KeyCode::Char('/'))
        );
        let (hint_x, hint_y) = find(&buf, "d re-search");

        d.open_popup(Popup::ConfirmUninstall {
            id: "owner/beta".to_string(),
        });
        let buf = draw(100, 30, |f, theme| d.render(f, f.area(), theme));
        let (x, y) = find(&buf, "y uninstall");
        assert_eq!(
            d.handle_click(x, y).map(|k| k.code),
            Some(KeyCode::Char('y'))
        );
        assert_eq!(
            d.handle_click(hint_x, hint_y),
            None,
            "the list's hints are inert under a popup"
        );
    }

    #[test]
    #[serial_test::serial]
    fn install_consent_discloses_plugin_agent_launch_command() {
        let mut dialog = PluginManagerDialog::new();
        dialog.open_popup(Popup::Install(Box::new(
            crate::plugin::install::InstallConsent {
                id: "acme.widget".into(),
                version: "1.0.0".into(),
                source: "gh:acme/widget".into(),
                notice: String::new(),
                unverified: false,
                validation: "verified".into(),
                capabilities: Vec::new(),
                ui: Vec::new(),
                build_steps: Vec::new(),
                agent_integrations: vec![
                    aoe_plugin_api::agent_integration::AgentIntegrationContribution {
                        tool: "session-helper".into(),
                        provider_versions: Vec::new(),
                        command: Some("TOKEN=local session-helper --flag".into()),
                        replies: false,
                    },
                ],
                fingerprint: "fingerprint".into(),
            },
        )));
        let buffer = draw(100, 30, |f, theme| dialog.render(f, f.area(), theme));
        let _ = find(&buffer, "session-helper: TOKEN=local session-helper --flag");
    }

    #[test]
    #[serial_test::serial]
    fn reapprove_consent_discloses_agent_launch_commands_without_raw_controls() {
        let mut dialog = PluginManagerDialog::new();
        dialog.open_popup(Popup::Reapprove(crate::plugin::install::ReapproveConsent {
            id: "acme.widget".into(),
            version: "1.0.0".into(),
            validation: "verified".into(),
            capabilities: Vec::new(),
            ui: Vec::new(),
            agent_integrations: vec![
                aoe_plugin_api::agent_integration::AgentIntegrationContribution {
                    tool: "session-helper".into(),
                    provider_versions: Vec::new(),
                    command: Some("x\u{1b}[2Jtail".into()),
                    replies: false,
                },
            ],
            manifest_hash: "sha256:hash".into(),
        }));
        let buffer = draw(100, 30, |f, theme| dialog.render(f, f.area(), theme));
        // The command renders with visible escapes and its plain text intact,
        // not with a raw escape sequence the manifest could hide text behind.
        let _ = find(&buffer, "session-helper: x\\u{1b}[2Jtail");
    }
}

#[cfg(all(test, unix))]
mod daemon_update_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    async fn wait_file(path: &Path) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn daemon_update_survives_tui_runtime_shutdown_during_build_and_restart() {
        use futures_util::FutureExt;
        use std::os::unix::fs::PermissionsExt;
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _home = crate::session::test_support::isolate_app_dir();
        let source = tempfile::tempdir().unwrap();
        let effects = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(effects.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let pid_file = effects.path().join("worker-pid");
        let receipt = effects.path().join("worker-receipt");
        let node = crate::plugin::write_fake_node(effects.path(), "node");
        let _node = crate::session::test_support::EnvGuard::set(&[("AOE_ACP_NODE", &node)]);
        let write_manifest = |version: &str, build: &str| {
            let command = serde_json::to_string(&[
                "sh", "-c",
                "printf '%s' $$ > \"$1\"; while IFS= read -r line; do printf '%s' \"$3\" > \"$2\"; done",
                "worker", pid_file.to_str().unwrap(), receipt.to_str().unwrap(), version,
            ]).unwrap();
            std::fs::write(source.path().join("aoe-plugin.toml"), format!(
                "id = \"acme.tui-job\"\nname = \"TUI job\"\nversion = \"{version}\"\napi_version = 2\ncapabilities = [\"runtime.worker\"]\n[runtime]\nkind = \"command\"\ncommand = {command}\nsystem = true\n{build}"
            )).unwrap();
        };
        write_manifest("1.0.0", "");
        crate::plugin::install::install(source.path().to_str().unwrap(), true, false)
            .await
            .unwrap();
        let state = crate::server::test_support::build_test_app_state_with_policy_configured(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
            |state| {
                state.plugin_host = Some(
                    crate::plugin::host::PluginHost::new(
                        &crate::session::get_app_dir().unwrap(),
                        "default",
                        None,
                        state.runtime.work.clone(),
                    )
                    .unwrap(),
                );
            },
        );
        let host = state.plugin_host.as_ref().unwrap();
        host.reconcile(&crate::plugin::registry()).await;
        let socket = effects.path().join("daemon.sock");
        let submissions = Arc::new(AtomicUsize::new(0));
        let server = crate::server::test_support::serve_local_plugin_fixture(
            state.clone(),
            &socket,
            submissions.clone(),
        )
        .await;
        let client = crate::acp::client::HttpClient::new(
            crate::daemon::DaemonEndpoint::local_unix(socket, None),
        )
        .unwrap();
        let outcome = std::panic::AssertUnwindSafe(async {
            for (index, phase) in ["build", "restart"].into_iter().enumerate() {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        host.notify_worker("acme.tui-job", "fixture.ping", serde_json::json!({}))
                            .await;
                        if receipt.exists() {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let old_pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
                assert!(crate::process::worker::is_process_group_alive(old_pid));
                let ready = effects.path().join(format!("build-{index}-ready"));
                let release = effects.path().join(format!("build-{index}-release"));
                let build_command = serde_json::to_string(&[
                    "sh",
                    "-c",
                    "printf building; : > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done",
                    "build",
                    ready.to_str().unwrap(),
                    release.to_str().unwrap(),
                ])
                .unwrap();
                let version = format!("{}.0.0", index + 2);
                write_manifest(
                    &version,
                    &format!("\n[[runtime.build]]\ncommand = {build_command}\n"),
                );
                let preview = client.preview_plugin_update("acme.tui-job").await.unwrap();
                let fingerprint = match preview {
                    UpdatePreview::SafeUpdate { fingerprint, .. } => fingerprint,
                    UpdatePreview::ConsentRequired { consent, .. } => consent.fingerprint,
                    UpdatePreview::NoUpdate { .. } => panic!("fixture must update"),
                };
                let installed = crate::plugin::plugins_dir().unwrap().join("acme.tui-job");
                let mut restart_gate =
                    crate::plugin::fetch::test_gate::install(&installed, "restart");
                let (accepted_tx, accepted_rx) = oneshot::channel();
                let (shutdown_tx, shutdown_rx) = oneshot::channel();
                let (stopped_tx, stopped_rx) = oneshot::channel();
                let pinned = client.clone();
                let observer = std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    runtime.block_on(async move {
                        let mut dialog = PluginManagerDialog::new();
                        dialog.update_client = Some(pinned);
                        dialog.start_apply("acme.tui-job".into(), Some(fingerprint));
                        loop {
                            dialog.tick();
                            if let Some(Popup::Progress(progress)) = &dialog.popup {
                                if let Some(job) = &progress.daemon_job {
                                    let _ = accepted_tx.send(job.clone());
                                    break;
                                }
                                if let Some(Err(error)) = &progress.done {
                                    panic!("{error}");
                                }
                            }
                            tokio::task::yield_now().await;
                        }
                        // Hiding the popup must not cancel its accepted mutation.
                        dialog.popup = None;
                        let _ = shutdown_rx.await;
                        drop(dialog);
                    });
                    drop(runtime);
                    let _ = stopped_tx.send(());
                });
                let job = tokio::time::timeout(Duration::from_secs(10), accepted_rx)
                    .await
                    .unwrap()
                    .unwrap();
                wait_file(&ready).await;
                if phase == "restart" {
                    std::fs::write(&release, b"release").unwrap();
                    tokio::time::timeout(Duration::from_secs(10), &mut restart_gate.entered)
                        .await
                        .unwrap()
                        .unwrap();
                }
                assert!(matches!(
                    client.plugin_job(&job).await.unwrap().job.status,
                    crate::daemon::PluginJobStatus::Running
                ));
                shutdown_tx.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(5), stopped_rx)
                    .await
                    .unwrap()
                    .unwrap();
                observer.join().unwrap();
                assert_eq!(submissions.load(Ordering::SeqCst), index + 1);
                assert!(matches!(
                    client.plugin_job(&job).await.unwrap().job.status,
                    crate::daemon::PluginJobStatus::Running
                ));
                if phase == "build" {
                    std::fs::write(&release, b"release").unwrap();
                    tokio::time::timeout(Duration::from_secs(10), &mut restart_gate.entered)
                        .await
                        .unwrap()
                        .unwrap();
                }
                std::fs::remove_file(&receipt).unwrap();
                restart_gate.release();
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let response = client.plugin_job(&job).await.unwrap();
                        match response.job.status {
                            crate::daemon::PluginJobStatus::Running => {
                                tokio::task::yield_now().await
                            }
                            crate::daemon::PluginJobStatus::Succeeded => {
                                assert!(response.log.tail.contains("building"));
                                break;
                            }
                            crate::daemon::PluginJobStatus::Failed { error } => panic!("{error}"),
                        }
                    }
                })
                .await
                .unwrap();
                assert!(!crate::process::worker::is_process_group_alive(old_pid));
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        host.notify_worker("acme.tui-job", "fixture.ping", serde_json::json!({}))
                            .await;
                        if std::fs::read_to_string(&receipt).ok().as_deref()
                            == Some(version.as_str())
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let new_pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
                assert_ne!(new_pid, old_pid);
                assert!(crate::process::worker::is_process_group_alive(new_pid));
                assert_eq!(
                    crate::plugin::lockfile::Lockfile::load()
                        .unwrap()
                        .get("acme.tui-job")
                        .unwrap()
                        .version,
                    version
                );
            }
        })
        .catch_unwind()
        .await;
        for index in 0..2 {
            std::fs::write(
                effects.path().join(format!("build-{index}-release")),
                b"cleanup",
            )
            .unwrap();
        }
        state.runtime.work.drain().await;
        host.shutdown().await;
        server.abort();
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }
}

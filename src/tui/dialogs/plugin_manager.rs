//! Plugin manager: list plugins with their trust and enabled state, toggle
//! them (reconciling a running daemon's workers live), inspect a plugin's full
//! disclosure, and run the external-plugin lifecycle in-TUI (install from
//! GitHub discovery, update, re-approve a stale grant, uninstall), each behind
//! the consent popup the CLI and web modals render. The twin of `aoe plugin`.

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
    InstallConsent, LiveRestart, LiveToggle, ReapproveConsent, UpdateConsent, UpdatePreview,
};
use crate::plugin::update_check::UpdateStatus;
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
    Preview(oneshot::Receiver<Result<UpdatePreview, String>>),
    Apply(oneshot::Receiver<Result<String, String>>),
    /// An enable/disable; the Ok string says whether the daemon reconciled.
    Toggle(oneshot::Receiver<Result<String, String>>),
    InstallPreview(oneshot::Receiver<Result<InstallConsent, String>>),
    InstallApply(oneshot::Receiver<Result<String, String>>),
    Uninstall(oneshot::Receiver<Result<String, String>>),
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
    /// Scroll offset into the popup body. A `Cell` so render can clamp it to
    /// the content height only render knows.
    popup_scroll: Cell<u16>,
    /// Once the user scrolls, a following popup stops chasing its tail.
    popup_user_scrolled: bool,
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

    /// Toggle live, reconciling daemon workers or falling back to a config write.
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
                let report = crate::plugin::install::apply_update(&apply_id, fingerprint, &log)
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                let updated = format!("Updated {} to {}", report.id, report.version);
                Ok(
                    match crate::plugin::install::restart_worker_live(&apply_id).await {
                        LiveRestart::Daemon => format!("{updated}; the daemon reloaded it."),
                        LiveRestart::NoDaemon => format!("{updated}."),
                        LiveRestart::DaemonStale { reason } => format!(
                            "{updated}. Daemon not reloaded ({reason}); restart it to run the new build."
                        ),
                    },
                )
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
                crate::plugin::install::preview_install(&source)
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
                    match result {
                        Ok(UpdatePreview::NoUpdate) => {
                            self.info = Some("Already up to date.".to_string());
                        }
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
                        // A dismissed version resurfaces only when a newer
                        // one appears.
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
                        Style::default().fg(theme.dimmed),
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
    }

    fn render_footer(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        // A running task wins, then a transient message, then the key hints.
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

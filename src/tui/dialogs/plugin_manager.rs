//! Plugin manager: list plugins (builtin and external) with their trust and
//! enabled/approval state, enable/disable them, and update an external plugin
//! through an in-TUI review popup that shows the changelog (and the access
//! disclosure when the new version expands what the plugin can do). The TUI
//! twin of `aoe plugin list` and the web Plugins tab. Installing a new plugin is
//! still CLI-driven (`aoe plugin install`); the TUI shows the resulting state.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tokio::sync::oneshot;

use super::{centered_rect, DialogResult};
use crate::plugin::changelog::{ChangelogEntry, UpdateChangelog};
use crate::plugin::discover::DiscoveryResult;
use crate::plugin::install::{UpdateConsent, UpdatePreview};
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
    /// Applying an approved update.
    Apply(oneshot::Receiver<Result<(), String>>),
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
    /// Set whenever the on-disk plugin config changed (enable/disable). An
    /// embedding surface drains it via [`take_mutated`] to re-sync its own
    /// config view; the standalone modal ignores it.
    mutated: bool,
    /// True when hosted inside the settings screen (vs the command-palette
    /// modal). Only changes the footer hint: Esc returns to the category list.
    embedded: bool,
    mode: Mode,
    /// An in-flight discovery / update-check task; `None` when idle.
    pending: Option<Pending>,
    /// A transient status line shown while a task runs ("Checking for updates…").
    loading: Option<&'static str>,
    /// Update statuses from the last `c` check, keyed by plugin id; drives the
    /// per-row "update!" marker.
    updates: HashMap<String, UpdateStatus>,
    /// Discovery results from the last `d` search, plus the cursor into them.
    discover_rows: Vec<DiscoveryResult>,
    discover_selected: usize,
    /// The plugin id a preview/apply is running for, so `tick` knows which row
    /// the result belongs to.
    pending_plugin: Option<String>,
    /// An open update review popup: the changelog plus, when access expands, the
    /// consent disclosure to render and approve / decline.
    review: Option<Review>,
}

impl Default for PluginManagerDialog {
    fn default() -> Self {
        Self::new()
    }
}

/// Most changelog lines the non-scrollable popup renders before linking out to
/// GitHub for the rest, so a long release body can never push the consent
/// disclosure and the approve/decline hint off screen.
const MAX_CHANGELOG_LINES: usize = 12;

/// Append the changelog to a review popup's lines: release notes or commit
/// subjects, or a single "unavailable" / "none" line. The popup `Paragraph` is
/// not scrollable, so the rendered body is hard-capped at [`MAX_CHANGELOG_LINES`]
/// and the full history is linked via `more_url`; the entry counts are already
/// capped by the backend, this bounds multi-line release bodies too.
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
            mode: Mode::Browse,
            pending: None,
            loading: None,
            updates: HashMap::new(),
            discover_rows: Vec::new(),
            discover_selected: 0,
            pending_plugin: None,
            review: None,
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

    /// Take and clear the "config mutated" flag (enable/disable wrote to disk
    /// and reloaded the registry).
    pub fn take_mutated(&mut self) -> bool {
        std::mem::take(&mut self.mutated)
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

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        self.info = None;
        // An open review popup owns the keyboard until the user decides.
        if self.review.is_some() {
            return self.handle_review_key(key);
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
            KeyCode::Char(' ') | KeyCode::Enter => {
                if let Some(row) = self.rows.get(self.selected) {
                    let (id, enabled) = (row.id.clone(), row.enabled);
                    match crate::plugin::install::set_enabled(&id, !enabled) {
                        Ok(()) => {
                            self.info = Some(format!(
                                "{} {id}",
                                if enabled { "Disabled" } else { "Enabled" }
                            ));
                            self.error = None;
                            self.reload();
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
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
            _ => DialogResult::Continue,
        }
    }

    /// Keys while the update review popup is open: approve/update, decline (only
    /// when access expands), or close.
    fn handle_review_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if let Some(review) = self.review.take() {
                    self.start_apply(review.id, Some(review.fingerprint));
                }
                DialogResult::Continue
            }
            // Decline only applies to a consent-expanding update: record the
            // dismissal so it stops nagging, keep the active version. A safe
            // update has nothing to dismiss, so `n` just closes it.
            KeyCode::Char('n') => {
                if let Some(review) = self.review.take() {
                    if review.consent.is_some() {
                        match crate::plugin::install::dismiss_update(
                            &review.id,
                            &review.fingerprint,
                        ) {
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
                }
                DialogResult::Continue
            }
            // Close without deciding.
            KeyCode::Esc | KeyCode::Char('q') => {
                self.review = None;
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn handle_discover_key(&mut self, key: KeyEvent) -> DialogResult<()> {
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
            // The dashboard/TUI have no install path (capability approval needs a
            // terminal prompt); show the command to run instead.
            KeyCode::Enter => {
                if let Some(r) = self.discover_rows.get(self.discover_selected) {
                    self.info = Some(format!("Install with: {}", r.install_command));
                }
                DialogResult::Continue
            }
            KeyCode::Char('d') => {
                self.start_discover();
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
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
        let (tx, rx) = oneshot::channel();
        let apply_id = id.clone();
        tokio::spawn(async move {
            let _ = tx.send(
                crate::plugin::install::apply_update(
                    &apply_id,
                    fingerprint,
                    &crate::plugin::install::OperationLog::Inherit,
                )
                .await
                .map(|_| ())
                .map_err(|e| format!("{e:#}")),
            );
        });
        self.pending_plugin = Some(id);
        self.pending = Some(Pending::Apply(rx));
        self.loading = Some("Updating…");
        self.error = None;
    }

    fn start_discover(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = crate::plugin::discover::discover(None)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.pending = Some(Pending::Discover(rx));
        self.loading = Some("Searching GitHub…");
        self.error = None;
    }

    /// Poll an in-flight discovery / update-check task. Returns true when the
    /// result landed (the host should redraw). Called from the event-loop tick.
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
                                self.review = Some(Review {
                                    id,
                                    from_version,
                                    to_version,
                                    fingerprint,
                                    changelog,
                                    consent: None,
                                });
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
                                self.review = Some(Review {
                                    id: consent.id.clone(),
                                    from_version: consent.from_version.clone(),
                                    to_version: consent.to_version.clone(),
                                    fingerprint: consent.fingerprint.clone(),
                                    changelog: consent.changelog.clone(),
                                    consent: Some(*consent),
                                });
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
                    match result {
                        Ok(()) => {
                            if let Some(id) = self.pending_plugin.take() {
                                self.updates.remove(&id);
                                self.info = Some(format!("Updated {id}."));
                            }
                            self.reload();
                        }
                        Err(message) => self.error = Some(message),
                    }
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Closed) => {
                    self.error = Some("Update failed.".to_string());
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
        // The review popup floats over the list, centered on the dialog rect.
        if let Some(review) = &self.review {
            self.render_review(f, rect, theme, review);
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
            lines.push(Line::from(Span::styled(
                "enter update · esc cancel",
                Style::default().fg(theme.dimmed),
            )));
            self.draw_review_popup(f, area, theme, lines, " Update plugin ");
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
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "y approve · n decline · esc close",
            Style::default().fg(theme.dimmed),
        )));
        self.draw_review_popup(f, area, theme, lines, " Approve update ");
    }

    /// Draw the review popup body into a clamped, centered sub-rect.
    fn draw_review_popup(
        &self,
        f: &mut Frame,
        area: Rect,
        theme: &Theme,
        lines: Vec<Line>,
        title: &str,
    ) {
        // A tiny terminal can be narrower/shorter than our preferred size;
        // never pass clamp/centered_rect a max below the min (it panics).
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width.clamp(1, 72);
        let height = (lines.len() as u16).saturating_add(2).clamp(1, area.height);
        let rect = centered_rect(area, width, height);
        f.render_widget(Clear, rect);
        let block = Block::default()
            .title(title.to_string())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let body = Paragraph::new(lines).wrap(Wrap { trim: true });
        f.render_widget(body, inner);
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
                // slot names only; ids are in `aoe plugin info`.
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
        if self.discover_rows.is_empty() {
            let empty = Paragraph::new("No plugins found on the aoe-plugin topic.")
                .style(Style::default().fg(theme.dimmed));
            f.render_widget(empty, area);
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
        f.render_stateful_widget(list, area, &mut state);
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
            (
                "enter: install command · d: re-search · esc: back".to_string(),
                theme.dimmed,
            )
        } else {
            let back = if self.embedded {
                "esc back"
            } else {
                "esc close"
            };
            (
                format!("space toggle · c check updates · u update · d discover · {back}"),
                theme.dimmed,
            )
        };
        let footer = Paragraph::new(text)
            .style(Style::default().fg(color))
            .wrap(Wrap { trim: true });
        f.render_widget(footer, area);
    }
}

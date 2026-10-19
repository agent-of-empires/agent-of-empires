//! Native browser state for daemon-provided suggested sessions.
//! Suggestions are opaque, untrusted provider content; session creation goes through the normal daemon API.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Suggestion {
    pub id: String,
    pub title: String,
    pub reason: String,
    pub source_url: String,
    pub source_ref: String,
    pub preview: String,
    pub project: Option<String>,
    pub criterion: String,
    pub draft: SuggestionDraft,
    pub provider_id: String,
    pub provider_name: String,
    pub revision: u64,
    #[serde(default)]
    pub dismissed: bool,
    pub session_id: Option<String>,
    pub project_path: Option<String>,
    #[serde(default)]
    pub can_start: bool,
    pub start_blocked_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SuggestionDraft {
    pub title: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SuggestionProvider {
    pub provider_id: String,
    pub provider_name: String,
    pub status: String,
    pub error: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SuggestionPolicy {
    pub enabled: bool,
    pub rules: Vec<SuggestionRule>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct SuggestionRule {
    pub provider_id: String,
    pub criterion: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SuggestionsResponse {
    pub items: Vec<Suggestion>,
    pub providers: Vec<SuggestionProvider>,
    pub auto_create: SuggestionPolicy,
}

type SuggestionActionResult = Result<(Option<SuggestionsResponse>, Option<String>), String>;

#[derive(Default)]
pub struct SuggestionsView {
    pub snapshot: Option<SuggestionsResponse>,
    pub error: Option<String>,
    pub show_dismissed: bool,
    pub query: String,
    pub filtering: bool,
    reviewed_rule: Option<SuggestionRule>,
    pub selected: usize,
    pub preview_scroll: usize,
    pending_refresh: Option<std::sync::mpsc::Receiver<Result<SuggestionsResponse, String>>>,
    pending_action: Option<std::sync::mpsc::Receiver<SuggestionActionResult>>,
    pub completed_session_id: Option<String>,
    pending_prepare:
        Option<std::sync::mpsc::Receiver<Result<(PreparedSuggestion, String), String>>>,
    pub prepared: Option<(PreparedSuggestion, String)>,
    pending_policy: Option<std::sync::mpsc::Receiver<Result<SuggestionPolicy, String>>>,
}

impl SuggestionsView {
    pub fn refresh(&mut self, profile: String) {
        use std::sync::mpsc;
        if self.is_busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.pending_refresh = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|runtime| {
                    runtime.block_on(async move {
                        let endpoint = crate::acp::client::require_daemon()
                            .await
                            .map_err(|e| e.to_string())?;
                        let client =
                            SuggestionsClient::new(endpoint, profile).map_err(|e| e.to_string())?;
                        client.refresh().await.map_err(|e| e.to_string())
                    })
                });
            let _ = tx.send(result);
        });
    }

    pub fn action(
        &mut self,
        profile: String,
        identity: SuggestionIdentity,
        action: SuggestionActionKind,
        create_another: bool,
    ) {
        use std::sync::mpsc;
        if self.is_busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.pending_action = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|runtime| {
                    runtime.block_on(async move {
                        let endpoint = crate::acp::client::require_daemon()
                            .await
                            .map_err(|e| e.to_string())?;
                        let client =
                            SuggestionsClient::new(endpoint, profile).map_err(|e| e.to_string())?;
                        let result = client
                            .action(identity, action, create_another)
                            .await
                            .map_err(|e| e.to_string())?;
                        let session_id = result
                            .get("session_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                        let snapshot = match client.get().await {
                            Ok(snapshot) => Some(snapshot),
                            Err(error) if session_id.is_none() => {
                                return Err(error.to_string());
                            }
                            // The create request already succeeded. A failed
                            // follow-up read must not hide its session id or
                            // make the user retry a completed action.
                            Err(_) => None,
                        };
                        Ok((snapshot, session_id))
                    })
                });
            let _ = tx.send(result);
        });
    }

    pub fn prepare(&mut self, profile: String, identity: SuggestionIdentity) {
        use std::sync::mpsc;
        if self.is_busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.pending_prepare = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|runtime| {
                    runtime.block_on(async move {
                        let endpoint = crate::acp::client::require_daemon()
                            .await
                            .map_err(|e| e.to_string())?;
                        let client =
                            SuggestionsClient::new(endpoint, profile).map_err(|e| e.to_string())?;
                        client
                            .prepare(identity)
                            .await
                            .map(|prepared| {
                                let source_ref = prepared.source_ref.clone();
                                (prepared, source_ref)
                            })
                            .map_err(|e| e.to_string())
                    })
                });
            let _ = tx.send(result);
        });
    }

    pub fn toggle_selected_policy(&mut self, profile: String) {
        if self.is_busy() {
            return;
        }
        let selected_rule = self.selected().map(|item| SuggestionRule {
            provider_id: item.provider_id.clone(),
            criterion: item.criterion.clone(),
        });
        if selected_rule.is_none() || self.reviewed_rule != selected_rule {
            self.error = Some(
                "Press p to review this exact provider and criterion before changing automation."
                    .into(),
            );
            return;
        }
        use std::sync::mpsc;
        if self.pending_policy.is_some() {
            return;
        }
        let Some(item) = self.selected().cloned() else {
            self.error = Some("Select a suggestion to review its automation rule.".to_string());
            return;
        };
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let mut policy = snapshot.auto_create.clone();
        let rule = SuggestionRule {
            provider_id: item.provider_id,
            criterion: item.criterion,
        };
        if let Some(index) = policy.rules.iter().position(|existing| existing == &rule) {
            policy.rules.remove(index);
            if policy.rules.is_empty() {
                policy.enabled = false;
            }
        } else {
            policy.rules.push(rule);
            policy.enabled = true;
        }
        let (tx, rx) = mpsc::channel();
        self.pending_policy = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|runtime| {
                    runtime.block_on(async move {
                        let endpoint = crate::acp::client::require_daemon()
                            .await
                            .map_err(|e| e.to_string())?;
                        let client =
                            SuggestionsClient::new(endpoint, profile).map_err(|e| e.to_string())?;
                        client.save_policy(&policy).await.map_err(|e| e.to_string())
                    })
                });
            let _ = tx.send(result);
        });
    }

    pub fn is_busy(&self) -> bool {
        self.pending_refresh.is_some()
            || self.pending_action.is_some()
            || self.pending_prepare.is_some()
            || self.pending_policy.is_some()
    }

    pub fn review_selected_policy(&mut self) -> Option<String> {
        let item = self.selected()?;
        let rule = SuggestionRule {
            provider_id: item.provider_id.clone(),
            criterion: item.criterion.clone(),
        };
        let review = format!("Provider: {} ({})\nCriterion: {}\n\nCtrl+e adds/removes only this exact rule. Automation defaults off. Source text remains untrusted; ordinary defaults, project trust and permission checks still apply.", item.provider_name, rule.provider_id, rule.criterion);
        self.reviewed_rule = Some(rule);
        Some(review)
    }

    pub fn poll_refresh(&mut self) {
        if let Some(receiver) = &self.pending_prepare {
            match receiver.try_recv() {
                Ok(Ok(prepared)) => {
                    self.prepared = Some(prepared);
                    self.pending_prepare = None;
                    self.error = None;
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.pending_prepare = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Suggestion preparation ended without a result.".to_string());
                    self.pending_prepare = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(receiver) = &self.pending_action {
            match receiver.try_recv() {
                Ok(Ok((snapshot, session_id))) => {
                    if let Some(snapshot) = snapshot {
                        self.snapshot = Some(snapshot);
                        self.selected = self
                            .selected
                            .min(self.visible_items().len().saturating_sub(1));
                        self.preview_scroll = 0;
                    }
                    self.completed_session_id = session_id;
                    self.pending_action = None;
                    self.error = None;
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.pending_action = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Suggestion action ended without a result.".to_string());
                    self.pending_action = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(receiver) = &self.pending_policy {
            match receiver.try_recv() {
                Ok(Ok(policy)) => {
                    if let Some(snapshot) = &mut self.snapshot {
                        snapshot.auto_create = policy;
                    }
                    self.pending_policy = None;
                    self.error = None;
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.pending_policy = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.error =
                        Some("Automation policy update ended without a result.".to_string());
                    self.pending_policy = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let Some(receiver) = &self.pending_refresh else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(snapshot)) => {
                self.snapshot = Some(snapshot);
                self.error = None;
                self.pending_refresh = None;
                self.selected = self
                    .selected
                    .min(self.visible_items().len().saturating_sub(1));
            }
            Ok(Err(error)) => {
                self.error = Some(error);
                self.pending_refresh = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.error = Some("Suggestion refresh ended without a result.".to_string());
                self.pending_refresh = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }
}

impl SuggestionsView {
    pub fn visible_items(&self) -> Vec<&Suggestion> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let query = self.query.to_lowercase();
        let mut items: Vec<_> = snapshot
            .items
            .iter()
            .filter(|item| {
                (self.show_dismissed || !item.dismissed)
                    && (query.is_empty()
                        || [
                            item.title.as_str(),
                            item.reason.as_str(),
                            item.project.as_deref().unwrap_or(""),
                            item.provider_name.as_str(),
                            item.source_url.as_str(),
                            item.source_ref.as_str(),
                            item.criterion.as_str(),
                            item.preview.as_str(),
                        ]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&query)))
            })
            .collect();
        items.sort_by(|left, right| {
            left.project
                .cmp(&right.project)
                .then_with(|| left.source_ref.cmp(&right.source_ref))
        });
        items
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.visible_items().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(len - 1);
        self.preview_scroll = 0;
    }

    pub fn selected(&self) -> Option<&Suggestion> {
        self.visible_items().get(self.selected).copied()
    }
}

#[derive(Debug, Clone, serde::Serialize, Deserialize, PartialEq, Eq)]
pub struct SuggestionIdentity {
    pub provider_id: String,
    pub id: String,
    pub revision: u64,
}

fn identity(item: &Suggestion) -> SuggestionIdentity {
    SuggestionIdentity {
        provider_id: item.provider_id.clone(),
        id: item.id.clone(),
        revision: item.revision,
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionsCommand {
    None,
    Close,
    Refresh,
    Start(SuggestionIdentity),
    Customize(SuggestionIdentity),
    Dismiss(SuggestionIdentity),
    Restore(SuggestionIdentity),
    CreateAnother(SuggestionIdentity),
    ToggleDismissed,
    ReviewPolicy,
    TogglePolicy,
    OpenLinked(String),
    Blocked(String),
}

impl SuggestionsView {
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> SuggestionsCommand {
        use crossterm::event::{KeyCode, KeyModifiers};
        if self.filtering {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.query.push(ch)
                }
                _ => {}
            }
            self.selected = 0;
            self.preview_scroll = 0;
            return SuggestionsCommand::None;
        }
        if self.is_busy() {
            return SuggestionsCommand::None;
        }
        match key.code {
            KeyCode::Esc => SuggestionsCommand::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                SuggestionsCommand::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                SuggestionsCommand::None
            }
            KeyCode::PageUp => {
                self.preview_scroll = self.preview_scroll.saturating_sub(8);
                SuggestionsCommand::None
            }
            KeyCode::PageDown => {
                self.preview_scroll = self.preview_scroll.saturating_add(8);
                SuggestionsCommand::None
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.query.clear();
                SuggestionsCommand::None
            }
            KeyCode::Char('r') => SuggestionsCommand::Refresh,
            KeyCode::Char('d') => SuggestionsCommand::ToggleDismissed,
            KeyCode::Char('p') => SuggestionsCommand::ReviewPolicy,
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                SuggestionsCommand::TogglePolicy
            }
            KeyCode::Char('x') => self.selected().map_or(SuggestionsCommand::None, |item| {
                let id = identity(item);
                if item.dismissed {
                    SuggestionsCommand::Restore(id)
                } else {
                    SuggestionsCommand::Dismiss(id)
                }
            }),
            KeyCode::Char('a') => self.selected().map_or(SuggestionsCommand::None, |item| {
                if item.can_start {
                    SuggestionsCommand::CreateAnother(identity(item))
                } else {
                    SuggestionsCommand::Blocked(
                        item.start_blocked_reason
                            .clone()
                            .unwrap_or_else(|| "Suggestion has no safe project mapping.".into()),
                    )
                }
            }),
            KeyCode::Char('c') => self.selected().map_or(SuggestionsCommand::None, |item| {
                SuggestionsCommand::Customize(identity(item))
            }),
            KeyCode::Enter => self.selected().map_or(SuggestionsCommand::None, |item| {
                if let Some(session_id) = &item.session_id {
                    SuggestionsCommand::OpenLinked(session_id.clone())
                } else if item.can_start {
                    SuggestionsCommand::Start(identity(item))
                } else {
                    SuggestionsCommand::Blocked(
                        item.start_blocked_reason
                            .clone()
                            .unwrap_or_else(|| "Suggestion has no safe project mapping.".into()),
                    )
                }
            }),
            KeyCode::Char(ch) if !ch.is_control() => {
                self.query.push(ch);
                self.selected = 0;
                self.preview_scroll = 0;
                SuggestionsCommand::None
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.preview_scroll = 0;
                SuggestionsCommand::None
            }
            _ => SuggestionsCommand::None,
        }
    }

    pub fn render(
        &self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        theme: &crate::tui::styles::Theme,
    ) {
        use ratatui::layout::{Constraint, Direction, Layout};
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line, Span, Text};
        use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

        frame.render_widget(ratatui::widgets::Clear, area);
        let outer = Block::default()
            .title(" Suggested sessions ")
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .style(Style::default().bg(theme.background).fg(theme.text))
            .border_style(Style::default().fg(theme.border));
        let inner = outer.inner(area);
        frame.render_widget(outer, area);
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(3)])
            .split(inner);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(sections[0]);
        let items = self.visible_items();
        let mut rows = Vec::new();
        let mut selected_row = 0usize;
        let mut group: Option<(&str, &str)> = None;
        for (index, item) in items.iter().enumerate() {
            let name = item.project.as_deref().unwrap_or("Unassigned project");
            let source = item.source_ref.as_str();
            if group != Some((name, source)) {
                rows.push(Line::from(Span::styled(
                    format!("▸ {name} / {source}"),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )));
                group = Some((name, source));
            }
            if index == self.selected {
                selected_row = rows.len();
            }
            let marker = if index == self.selected { "› " } else { "  " };
            let state = if item.dismissed {
                " [dismissed]"
            } else if item.session_id.is_some() {
                " [linked]"
            } else {
                ""
            };
            rows.push(Line::from(Span::styled(
                format!("{marker}{}{state}", item.title),
                Style::default().fg(if index == self.selected {
                    theme.text
                } else {
                    theme.dimmed
                }),
            )));
        }
        if rows.is_empty() {
            let message = if self.is_busy() && self.snapshot.is_none() {
                "Loading suggested sessions..."
            } else if self.snapshot.is_none() {
                self.error
                    .as_deref()
                    .unwrap_or("No suggestions loaded. Press r to refresh.")
            } else if self.query.is_empty() {
                "No suggestions. Refresh to check providers."
            } else {
                "No suggestions match this filter."
            };
            rows.push(Line::from(Span::styled(
                message,
                Style::default().fg(theme.dimmed),
            )));
        }
        let list_scroll = selected_row.saturating_sub(columns[0].height.saturating_sub(3) as usize);
        let list = Paragraph::new(Text::from(rows))
            .scroll((list_scroll.min(u16::MAX as usize) as u16, 0))
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM | Borders::LEFT)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .padding(ratatui::widgets::Padding::horizontal(1))
                    .title(format!(" Projects  /{} ", self.query)),
            );
        frame.render_widget(list, columns[0]);
        frame.render_widget(
            Paragraph::new(
                "j/k select · / filter · PgUp/PgDn preview · Esc back\nEnter start/open · c customize · a create another · x dismiss/restore\nr refresh · d dismissed · p review policy · Ctrl+e toggle reviewed rule",
            )
            .style(Style::default().fg(theme.dimmed))
            .wrap(Wrap { trim: true }),
            sections[1],
        );
        let mut detail = Vec::new();
        if let Some(error) = &self.error {
            let context = if self.snapshot.is_some() {
                "Request failed; showing last snapshot"
            } else {
                "Suggestions unavailable"
            };
            detail.push(Line::from(Span::styled(
                format!("{context}: {error}"),
                Style::default().fg(theme.accent),
            )));
            detail.push(Line::from(""));
        }
        if let Some(item) = self.selected() {
            detail.push(Line::from(Span::styled(
                &item.title,
                Style::default().add_modifier(Modifier::BOLD),
            )));
            detail.push(Line::from(format!(
                "Provider: {} · Criterion: {}",
                item.provider_name, item.criterion
            )));
            detail.push(Line::from(format!(
                "Source: {} ({})",
                item.source_url, item.source_ref
            )));
            detail.push(Line::from(format!(
                "Project: {}",
                item.project.as_deref().unwrap_or("not registered")
            )));
            detail.push(Line::from(""));
            detail.push(Line::from(item.reason.as_str()));
            detail.extend(item.preview.lines().map(Line::from));
            detail.push(Line::from(""));
            detail.push(Line::from(Span::styled(
                "Proposed prompt (untrusted source text):",
                Style::default().fg(theme.accent),
            )));
            detail.extend(item.draft.prompt.lines().map(Line::from));
        }
        if let Some(snapshot) = &self.snapshot {
            detail.push(Line::from(""));
            for provider in &snapshot.providers {
                detail.push(Line::from(format!(
                    "{}: {}{}",
                    provider.provider_name,
                    provider.status,
                    provider
                        .error
                        .as_deref()
                        .map(|e| format!("; {e}"))
                        .unwrap_or_default()
                )));
                if let Some(updated_at) = &provider.updated_at {
                    detail.push(Line::from(format!("Last update: {updated_at}")));
                }
            }
            detail.push(Line::from(format!("Automatic creation: {}. This does not make source text safe or grant unattended permissions.", if snapshot.auto_create.enabled { "enabled" } else { "off" })));
            if let Some(error) = &snapshot.auto_create.last_error {
                detail.push(Line::from(format!("Automatic creation blocked: {error}")));
            }
            if snapshot.auto_create.rules.is_empty() {
                detail.push(Line::from("Reviewed rules: none"));
            } else {
                for rule in &snapshot.auto_create.rules {
                    detail.push(Line::from(format!(
                        "Reviewed rule: {} / {}",
                        rule.provider_id, rule.criterion
                    )));
                }
            }
        }
        let preview = Paragraph::new(Text::from(detail))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .padding(ratatui::widgets::Padding::horizontal(1))
                    .title(" Preview and provider status "),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.preview_scroll.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(preview, columns[1]);
    }
}

/// Native suggestions API using the daemon's shared authenticated transport.
pub struct SuggestionsClient {
    http: crate::acp::client::HttpClient,
    profile: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PreparedSuggestion {
    pub project_path: String,
    pub title: String,
    pub prompt: String,
    pub suggestion: SuggestionIdentity,
    pub source_ref: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionActionKind {
    Dismiss,
    Restore,
    Start,
}

impl SuggestionsClient {
    pub fn new(
        endpoint: crate::acp::client::DaemonEndpoint,
        profile: String,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            http: crate::acp::client::HttpClient::new(endpoint)?,
            profile,
        })
    }

    fn path(&self, suffix: &str) -> String {
        format!(
            "/api/suggestions{suffix}?profile={}",
            percent_encoding::utf8_percent_encode(
                &self.profile,
                percent_encoding::NON_ALPHANUMERIC
            )
        )
    }

    pub async fn get(&self) -> anyhow::Result<SuggestionsResponse> {
        Ok(self
            .http
            .daemon_json(reqwest::Method::GET, &self.path(""), None)
            .await?)
    }

    pub async fn refresh(&self) -> anyhow::Result<SuggestionsResponse> {
        Ok(self
            .http
            .daemon_json(reqwest::Method::POST, &self.path("/refresh"), None)
            .await?)
    }

    pub async fn action(
        &self,
        identity: SuggestionIdentity,
        action: SuggestionActionKind,
        create_another: bool,
    ) -> anyhow::Result<serde_json::Value> {
        let body = serde_json::json!({"provider_id": identity.provider_id, "id": identity.id, "revision": identity.revision, "action": action, "create_another": create_another});
        Ok(self
            .http
            .daemon_json(reqwest::Method::POST, &self.path("/action"), Some(&body))
            .await?)
    }

    pub async fn prepare(
        &self,
        identity: SuggestionIdentity,
    ) -> anyhow::Result<PreparedSuggestion> {
        let body = serde_json::to_value(identity)?;
        Ok(self
            .http
            .daemon_json(reqwest::Method::POST, &self.path("/prepare"), Some(&body))
            .await?)
    }

    pub async fn create_session(
        &self,
        body: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(self
            .http
            .daemon_json(reqwest::Method::POST, "/api/sessions", Some(&body))
            .await?)
    }

    pub fn create_customized_session_body(
        data: &crate::tui::dialogs::NewSessionData,
        prepared: &PreparedSuggestion,
    ) -> anyhow::Result<serde_json::Value> {
        if prepared.source_ref.is_empty() || prepared.prompt.is_empty() {
            anyhow::bail!("missing suggestion source reference or prepared prompt");
        }
        Ok(serde_json::json!({
            "profile": data.profile, "title": data.title, "title_typed": data.title_typed,
            "path": data.path, "group": data.group, "tool": data.tool,
            "worktree_enabled": data.worktree_enabled, "worktree_branch": data.worktree_branch,
            "create_new_branch": data.create_new_branch, "base_branch": data.base_branch,
            "extra_repo_paths": data.extra_repo_paths, "sandbox": data.sandbox,
            "sandbox_image": data.sandbox_image, "yolo_mode": data.yolo_mode,
            "extra_env": data.extra_env, "extra_args": data.extra_args,
            "command_override": data.command_override, "scratch": data.scratch,
            "view": if data.structured { "structured" } else { "terminal" },
            "prompt": prepared.prompt, "source_refs": [prepared.source_ref],
            "suggestion": prepared.suggestion, "create_another": false
        }))
    }

    pub async fn save_policy(&self, policy: &SuggestionPolicy) -> anyhow::Result<SuggestionPolicy> {
        let body = serde_json::json!({"enabled": policy.enabled, "rules": policy.rules});
        Ok(self
            .http
            .daemon_json(reqwest::Method::PATCH, &self.path("/policy"), Some(&body))
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suggestion(title: &str, project: &str) -> Suggestion {
        Suggestion {
            id: title.into(),
            title: title.into(),
            reason: String::new(),
            source_url: String::new(),
            source_ref: format!("source-{title}"),
            preview: String::new(),
            project: Some(project.into()),
            criterion: String::new(),
            draft: SuggestionDraft {
                title: title.into(),
                prompt: String::new(),
            },
            provider_id: "p".into(),
            provider_name: "provider".into(),
            revision: 1,
            dismissed: false,
            session_id: None,
            project_path: None,
            can_start: true,
            start_blocked_reason: None,
        }
    }

    #[test]
    fn filters_groups_by_project_and_excludes_dismissed_by_default() {
        let mut view = SuggestionsView::default();
        let mut hidden = suggestion("Closed", "repo");
        hidden.dismissed = true;
        view.snapshot = Some(SuggestionsResponse {
            items: vec![
                suggestion("Fix docs", "repo"),
                suggestion("Fix tests", "other"),
                hidden,
            ],
            providers: vec![],
            auto_create: SuggestionPolicy {
                enabled: false,
                rules: vec![],
                last_error: None,
            },
        });
        assert_eq!(view.visible_items().len(), 2);
        view.query = "other".into();
        assert_eq!(view.selected().unwrap().title, "Fix tests");
        view.show_dismissed = true;
        view.query.clear();
        assert_eq!(view.visible_items().len(), 3);
        view.move_selection(1);
        assert_eq!(view.selected().unwrap().title, "Closed");
        view.query = "source-Closed".into();
        view.selected = 0;
        assert_eq!(view.selected().unwrap().title, "Closed");
    }
    #[test]
    fn policy_review_and_toggle_have_explicit_keyboard_commands() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut view = SuggestionsView {
            snapshot: Some(SuggestionsResponse {
                items: vec![suggestion("Fix docs", "repo")],
                providers: vec![],
                auto_create: SuggestionPolicy {
                    enabled: false,
                    rules: vec![],
                    last_error: None,
                },
            }),
            ..Default::default()
        };
        assert_eq!(
            view.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            SuggestionsCommand::ReviewPolicy
        );
        assert_eq!(
            view.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL,)),
            SuggestionsCommand::TogglePolicy
        );
        assert!(matches!(
            view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            SuggestionsCommand::Start(_)
        ));
    }
    #[test]
    fn start_actions_require_a_resolved_project_mapping() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut blocked = suggestion("Fix docs", "missing");
        blocked.can_start = false;
        blocked.start_blocked_reason = Some("Project is not registered".into());
        let mut view = SuggestionsView {
            snapshot: Some(SuggestionsResponse {
                items: vec![blocked],
                providers: vec![],
                auto_create: SuggestionPolicy {
                    enabled: false,
                    rules: vec![],
                    last_error: None,
                },
            }),
            ..Default::default()
        };

        let expected = SuggestionsCommand::Blocked("Project is not registered".into());
        assert_eq!(
            view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            expected
        );
        assert_eq!(
            view.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            expected
        );
    }

    #[test]
    fn provider_status_decodes_host_timestamp() {
        let provider: SuggestionProvider = serde_json::from_value(serde_json::json!({
            "provider_id": "github",
            "provider_name": "GitHub",
            "status": "ok",
            "error": null,
            "updated_at": "2026-10-18T12:00:00+00:00"
        }))
        .unwrap();

        assert_eq!(
            provider.updated_at.as_deref(),
            Some("2026-10-18T12:00:00+00:00")
        );
    }

    fn loaded_view() -> SuggestionsView {
        let mut item = suggestion("Review change", "repo");
        item.reason = "Review requested".into();
        item.criterion = "review_requested".into();
        item.draft.prompt = "Inspect the actual diff".into();
        SuggestionsView {
            snapshot: Some(SuggestionsResponse {
                items: vec![item],
                providers: vec![SuggestionProvider {
                    provider_id: "p".into(),
                    provider_name: "provider".into(),
                    status: "stale".into(),
                    error: Some("Provider unavailable".into()),
                    updated_at: Some("2026-10-18T12:00:00Z".into()),
                }],
                auto_create: SuggestionPolicy {
                    enabled: false,
                    rules: vec![],
                    last_error: None,
                },
            }),
            ..SuggestionsView::default()
        }
    }

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn suggestion_keys_cover_linked_create_another_dismiss_restore_and_filter() {
        use crossterm::event::KeyCode;
        let mut view = loaded_view();
        let id = identity(view.selected().unwrap());
        assert_eq!(
            view.handle_key(key(KeyCode::Enter)),
            SuggestionsCommand::Start(id.clone())
        );
        assert_eq!(
            view.handle_key(key(KeyCode::Char('c'))),
            SuggestionsCommand::Customize(id.clone())
        );
        assert_eq!(
            view.handle_key(key(KeyCode::Char('x'))),
            SuggestionsCommand::Dismiss(id.clone())
        );
        view.snapshot.as_mut().unwrap().items[0].session_id = Some("linked".into());
        assert_eq!(
            view.handle_key(key(KeyCode::Enter)),
            SuggestionsCommand::OpenLinked("linked".into())
        );
        assert_eq!(
            view.handle_key(key(KeyCode::Char('a'))),
            SuggestionsCommand::CreateAnother(id.clone())
        );
        view.snapshot.as_mut().unwrap().items[0].dismissed = true;
        view.show_dismissed = true;
        assert_eq!(
            view.handle_key(key(KeyCode::Char('x'))),
            SuggestionsCommand::Restore(id)
        );
        assert_eq!(
            view.handle_key(key(KeyCode::Char('d'))),
            SuggestionsCommand::ToggleDismissed
        );
        assert_eq!(
            view.handle_key(key(KeyCode::Char('r'))),
            SuggestionsCommand::Refresh
        );
        view.handle_key(key(KeyCode::Char('/')));
        for ch in "review_requested".chars() {
            assert_eq!(
                view.handle_key(key(KeyCode::Char(ch))),
                SuggestionsCommand::None
            );
        }
        assert_eq!(view.query, "review_requested");
        assert_eq!(view.visible_items().len(), 1);
        view.handle_key(key(KeyCode::Enter));
        assert!(!view.filtering);
        assert_eq!(
            view.handle_key(key(KeyCode::Esc)),
            SuggestionsCommand::Close
        );
    }

    #[test]
    fn failed_async_start_keeps_suggestion_and_allows_retry() {
        let mut view = loaded_view();
        let (tx, rx) = std::sync::mpsc::channel();
        view.pending_action = Some(rx);
        tx.send(Err("creation rejected".into())).unwrap();
        view.poll_refresh();
        assert!(view.pending_action.is_none());
        assert!(view.completed_session_id.is_none());
        assert_eq!(view.error.as_deref(), Some("creation rejected"));
        assert_eq!(view.visible_items().len(), 1);
        assert!(view.selected().unwrap().session_id.is_none());
        assert!(matches!(
            view.handle_key(key(crossterm::event::KeyCode::Enter)),
            SuggestionsCommand::Start(_)
        ));
    }

    #[test]
    fn successful_dismiss_snapshot_retains_restore_and_clamps_selection() {
        let mut view = loaded_view();
        view.snapshot
            .as_mut()
            .unwrap()
            .items
            .push(suggestion("Second", "repo"));
        view.move_selection(1);
        let dismissed_id = identity(view.selected().unwrap());
        let mut snapshot = view.snapshot.clone().unwrap();
        snapshot
            .items
            .iter_mut()
            .find(|item| item.id == dismissed_id.id)
            .unwrap()
            .dismissed = true;
        let (tx, rx) = std::sync::mpsc::channel();
        view.pending_action = Some(rx);
        tx.send(Ok((Some(snapshot), None))).unwrap();
        view.poll_refresh();
        assert_eq!(view.selected, 0);
        assert_eq!(view.visible_items().len(), 1);
        assert_eq!(view.selected().unwrap().title, "Review change");
        view.show_dismissed = true;
        view.move_selection(1);
        assert_eq!(
            view.handle_key(key(crossterm::event::KeyCode::Char('x'))),
            SuggestionsCommand::Restore(dismissed_id)
        );
    }

    #[test]
    fn automation_requires_review_of_the_selected_exact_rule() {
        let mut view = loaded_view();
        view.toggle_selected_policy("test".into());
        assert!(!view.is_busy());
        assert!(!view.snapshot.as_ref().unwrap().auto_create.enabled);
        assert!(view.error.as_deref().unwrap().contains("Press p"));
        let review = view.review_selected_policy().unwrap();
        assert!(review.contains("provider (p)"));
        assert!(review.contains("review_requested"));
        assert!(review.contains("Source text remains untrusted"));
        view.snapshot.as_mut().unwrap().items[0].criterion = "another".into();
        view.toggle_selected_policy("test".into());
        assert!(!view.is_busy());
        assert!(!view.snapshot.as_ref().unwrap().auto_create.enabled);
    }

    #[test]
    fn renders_suggestion_detail_stale_provider_error_empty_and_filtered_states() {
        fn render(view: &SuggestionsView) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 36)).unwrap();
            terminal
                .draw(|frame| {
                    view.render(frame, frame.area(), &crate::tui::styles::Theme::default())
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        }
        let mut view = loaded_view();
        view.error = Some("creation rejected".into());
        let text = render(&view);
        for expected in [
            "Suggested sessions",
            "repo / source-Review change",
            "Review requested",
            "Inspect the actual diff",
            "untrusted source text",
            "provider: stale; Provider unavailable",
            "creation rejected",
            "Automatic creation: off",
            "Last update: 2026-10-18",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        view.query = "no match".into();
        assert!(render(&view).contains("No suggestions match this filter"));
        view.query.clear();
        view.snapshot.as_mut().unwrap().items.clear();
        assert!(render(&view).contains("No suggestions. Refresh to check providers"));
        view.snapshot = None;
        assert!(render(&view).contains("creation rejected"));
    }
}

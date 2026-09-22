//! The card shown once a mode is live: status, pairing code, paired devices,
//! the dashboard URL and its QR, plus the help overlay and error screen.

use std::time::Duration;

use ratatui::prelude::*;
use ratatui::widgets::*;

use super::{
    base_url, centered, client_command, error_mentions_tailscale, truncate_to_width, Exposure,
    PendingConfirm, ServeUrl,
};
use crate::tui::styles::Theme;

/// The scannable block for `url`, as terminal rows. Empty without the
/// dashboard bundle: a phone that scanned it would reach no page.
#[cfg(feature = "web")]
pub(super) fn render_qr(url: &str) -> String {
    use qrcode::render::unicode::Dense1x2;
    use qrcode::QrCode;

    match QrCode::new(url.as_bytes()) {
        Ok(code) => code
            .render::<Dense1x2>()
            .quiet_zone(true)
            .dark_color(Dense1x2::Dark)
            .light_color(Dense1x2::Light)
            .build(),
        Err(_) => String::from("(QR unavailable; use the URL below)"),
    }
}

#[cfg(not(feature = "web"))]
pub(super) fn render_qr(_url: &str) -> String {
    String::new()
}

/// Shown in place of the QR when the dashboard bundle is not embedded.
pub(super) const API_ONLY_NOTICE: &str =
    "This build has no dashboard; the link serves the REST API only.";

/// Narrowest content the card lays out for; below it lines are truncated.
pub(super) const CARD_MIN_WIDTH: usize = 50;
/// Widest a single column grows before lines are truncated. The longest line
/// the card lays out is the token URL indented by two, about 100 columns for
/// `http://<host>:<port>/?token=<64 hex>`, so this keeps it on one line.
pub(super) const CARD_MAX_WIDTH: usize = 112;
/// Columns between the pairing column and the QR column.
pub(super) const COLUMN_GAP: usize = 3;

pub(super) struct ActiveModel<'a> {
    pub(super) mode: Exposure,
    pub(super) urls: &'a [ServeUrl],
    pub(super) url_index: usize,
    pub(super) passphrase: Option<&'a str>,
    pub(super) elapsed: Duration,
    pub(super) pending_confirm: Option<PendingConfirm>,
    pub(super) pairing: Option<&'a crate::tui::dialogs::pairing::PairingPanel>,
    pub(super) show_web: bool,
    pub(super) flash: Option<&'a str>,
    /// The selected URL's QR code, empty when this build draws none.
    pub(super) qr: &'a str,
}

/// A card row and how early it gives way on a short terminal: rows with the
/// highest `yields` go first, and 0 never does.
pub(super) struct Row {
    line: Line<'static>,
    yields: u8,
}

pub(super) const KEEP: u8 = 0;
/// A device other than the selected one.
pub(super) const OTHER_DEVICE: u8 = 1;
pub(super) const EXPOSURE_NOTE: u8 = 2;
pub(super) const GAP: u8 = 3;
pub(super) const EXPLAIN: u8 = 4;

impl Row {
    fn keep(line: impl Into<Line<'static>>) -> Self {
        Self::yields(line, KEEP)
    }

    fn yields(line: impl Into<Line<'static>>, yields: u8) -> Self {
        Self {
            line: line.into(),
            yields,
        }
    }

    fn gap() -> Self {
        Self::yields(Line::from(""), GAP)
    }
}

/// Drop the most expendable rows until `rows` fits `height`, then tidy the
/// gaps the drops left at the edges or doubled up.
pub(super) fn fit_rows(mut rows: Vec<Row>, height: usize, width: usize) -> Vec<Line<'static>> {
    while rows.len() > height {
        let Some(most) = rows
            .iter()
            .map(|row| row.yields)
            .max()
            .filter(|y| *y > KEEP)
        else {
            break;
        };
        let at = rows.iter().rposition(|row| row.yields == most).unwrap_or(0);
        rows.remove(at);
    }
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len());
    for row in rows {
        let blank = row.line.width() == 0;
        if blank && lines.last().is_none_or(|last| last.width() == 0) {
            continue;
        }
        lines.push(clip(row.line, width));
    }
    while lines.last().is_some_and(|last| last.width() == 0) {
        lines.pop();
    }
    lines
}

/// `text` word-wrapped to `width` after `indent`, every row yielding as an
/// explanation.
pub(super) fn explain(theme: &Theme, indent: &str, text: &str, width: usize) -> Vec<Row> {
    let room = width.saturating_sub(indent.len()).max(1);
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= room => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines
        .into_iter()
        .map(|line| {
            Row::yields(
                Line::styled(format!("{indent}{line}"), Style::default().fg(theme.dimmed)),
                EXPLAIN,
            )
        })
        .collect()
}

/// `line` cut to `width` columns, keeping each span's style.
pub(super) fn clip(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let mut room = width.saturating_sub(1);
    let mut spans = Vec::new();
    for span in line.spans {
        if room == 0 {
            break;
        }
        let text: String = span.content.chars().take(room).collect();
        room -= text.chars().count();
        spans.push(Span::styled(text, span.style));
    }
    spans.push(Span::raw("…"));
    Line::from(spans)
}

pub(super) fn heading(theme: &Theme, text: &str) -> Row {
    Row::keep(Line::styled(
        text.to_string(),
        Style::default().fg(theme.accent).bold(),
    ))
}

/// `text` broken into lines of at most `width` characters, for values like a
/// tokenized URL that are useless truncated.
pub(super) fn chunked(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Status first: where this machine is shared, since when, and what that
/// lets other machines do.
pub(super) fn status_rows(
    theme: &Theme,
    model: &ActiveModel,
    url: &ServeUrl,
    width: usize,
) -> Vec<Row> {
    let (place, meaning) = match model.mode {
        Exposure::Tunnel => (
            "Sharing over the internet",
            "Anyone with the link and passphrase, or a paired aoe, can connect.",
        ),
        Exposure::Network => (
            "Sharing on local network",
            "Other aoe clients on this network can connect once paired.",
        ),
        Exposure::Localhost => (
            "Sharing on this machine only",
            "Nothing else can connect until you change the exposure.",
        ),
    };
    let address = base_url(&url.url)
        .map(|base| crate::daemon::remotes::short_remote_address(base.as_str()))
        .unwrap_or_default();
    let mut spans = vec![
        Span::styled("● ", Style::default().fg(theme.running)),
        Span::styled(place, Style::default().fg(theme.text).bold()),
        Span::styled(format!(" · {address}"), Style::default().fg(theme.text)),
        Span::styled(
            format!(" · up {}", format_elapsed(model.elapsed)),
            Style::default().fg(theme.dimmed),
        ),
    ];
    if model.elapsed >= Duration::from_secs(8 * 3600) {
        spans.push(Span::styled(
            " · still need it?",
            Style::default().fg(theme.waiting),
        ));
    }
    let mut rows = vec![Row::keep(Line::from(spans))];
    rows.extend(explain(theme, "", meaning, width));
    rows
}

/// The code and the two steps that use it, led by any lockout, since a
/// locked-out machine cannot pair at all.
pub(super) fn pair_rows(
    theme: &Theme,
    model: &ActiveModel,
    command: Option<&str>,
    width: usize,
) -> Vec<Row> {
    let dimmed = Style::default().fg(theme.dimmed);
    let hint = Style::default().fg(theme.hint);
    let text = Style::default().fg(theme.text);
    let mut rows = vec![heading(theme, "Pair a device")];
    let Some(panel) = model.pairing else {
        return rows;
    };
    for (ip, left) in panel.lockouts() {
        rows.push(Row::keep(Line::from(vec![
            Span::styled(format!("  {ip}"), Style::default().fg(theme.waiting)),
            Span::styled(
                format!(
                    " locked out · {} left  ",
                    crate::tui::dialogs::pairing::ago(left)
                ),
                dimmed,
            ),
            Span::styled("u", hint),
            Span::styled(" unblock", dimmed),
        ])));
    }
    match panel.code() {
        crate::tui::dialogs::pairing::CodeView::Minting => {
            rows.push(Row::keep(Line::styled("  creating a code…", dimmed)));
        }
        crate::tui::dialogs::pairing::CodeView::Ready { spaced, expires_in } => {
            let code = Span::styled(
                format!("  {spaced}"),
                Style::default().fg(theme.accent).bold(),
            );
            let expiry = format!("single use · new code in {expires_in}");
            if code.width() + 3 + expiry.chars().count() <= width {
                rows.push(Row::keep(Line::from(vec![
                    code,
                    Span::styled(format!("   {expiry}"), dimmed),
                ])));
            } else {
                rows.push(Row::keep(Line::from(code)));
                rows.push(Row::keep(Line::styled(format!("  {expiry}"), dimmed)));
            }
        }
        crate::tui::dialogs::pairing::CodeView::Failed(error) => {
            rows.push(Row::keep(Line::styled(
                truncate_to_width(&format!("  No code: {error}"), width),
                Style::default().fg(theme.error),
            )));
        }
    }
    let Some(command) = command else {
        rows.push(Row::keep(Line::styled(
            "  Other machines cannot reach this one; press e to share it.",
            text,
        )));
        return rows;
    };
    let lead = "  1. On the other machine run  ";
    let command_style = text.bold();
    if lead.len() + command.len() <= width {
        rows.push(Row::keep(Line::from(vec![
            Span::styled(lead, text),
            Span::styled(command.to_string(), command_style),
        ])));
    } else {
        rows.push(Row::keep(Line::styled(
            "  1. On the other machine run",
            text,
        )));
        rows.push(Row::keep(Line::styled(
            truncate_to_width(&format!("     {command}"), width),
            command_style,
        )));
    }
    rows.push(Row::keep(Line::styled(
        "  2. Enter the code above when it asks",
        text,
    )));
    rows
}

pub(super) fn device_rows(theme: &Theme, model: &ActiveModel, width: usize) -> Vec<Row> {
    let dimmed = Style::default().fg(theme.dimmed);
    let mut rows = vec![heading(theme, "Paired devices")];
    let devices = match model.pairing.map(|panel| panel.devices()) {
        None | Some(Err(None)) => {
            rows.push(Row::keep(Line::styled("  loading…", dimmed)));
            return rows;
        }
        Some(Err(Some(error))) => {
            rows.push(Row::keep(Line::styled(
                truncate_to_width(&format!("  {error}"), width),
                Style::default().fg(theme.error),
            )));
            return rows;
        }
        Some(Ok(devices)) if devices.is_empty() => {
            rows.push(Row::keep(Line::styled("  No devices paired yet.", dimmed)));
            return rows;
        }
        Some(Ok(devices)) => devices,
    };
    let name_width = devices
        .iter()
        .map(|d| d.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(20);
    for device in devices {
        let name_style = if device.selected {
            Style::default().fg(theme.text).bold()
        } else {
            Style::default().fg(theme.text)
        };
        let mut spans = vec![
            Span::styled(
                if device.selected { "▸ " } else { "  " },
                Style::default().fg(theme.accent),
            ),
            Span::styled(
                format!("{:name_width$}", truncate_to_width(device.name, name_width)),
                name_style,
            ),
            Span::styled(
                format!("  {} · seen {}", device.address, device.seen),
                dimmed,
            ),
        ];
        if device.armed {
            spans.push(Span::styled(
                format!("  press x again to revoke {}", device.name),
                Style::default().fg(theme.waiting).bold(),
            ));
        } else if device.selected {
            spans.push(Span::styled("  x", Style::default().fg(theme.hint)));
            spans.push(Span::styled(format!(" revoke {}", device.name), dimmed));
        }
        let line = Line::from(spans);
        let line = if line.width() > width {
            Line::styled(truncate_to_width(&line.to_string(), width), name_style)
        } else {
            line
        };
        rows.push(Row::yields(
            line,
            if device.selected { KEEP } else { OTHER_DEVICE },
        ));
    }
    rows
}

/// The dashboard link, what it is for, and the passphrase a tunnel adds.
pub(super) fn web_rows(
    theme: &Theme,
    model: &ActiveModel,
    url: &ServeUrl,
    width: usize,
) -> Vec<Row> {
    let dimmed = Style::default().fg(theme.dimmed);
    let mut rows = vec![heading(theme, "Browser or phone")];
    if cfg!(feature = "web") {
        rows.extend(explain(
            theme,
            "  ",
            "Open the dashboard in a browser or scan the QR with a phone. The link \
             carries the token, which grants full access.",
            width,
        ));
    } else {
        for row in explain(theme, "  ", API_ONLY_NOTICE, width) {
            rows.push(Row::keep(row.line));
        }
    }
    if let Some(label) = &url.label {
        rows.push(Row::yields(
            Line::styled(format!("  via {label}"), dimmed.italic()),
            EXPLAIN,
        ));
    }
    for chunk in chunked(&url.url, width.saturating_sub(2)) {
        rows.push(Row::keep(Line::styled(
            format!("  {chunk}"),
            Style::default().fg(theme.accent),
        )));
    }
    if model.mode == Exposure::Tunnel {
        rows.push(Row::keep(match model.passphrase {
            Some(passphrase) => Line::from(vec![
                Span::styled("  Passphrase ", dimmed),
                Span::styled(
                    passphrase.to_string(),
                    Style::default().fg(theme.accent).bold(),
                ),
            ]),
            None => Line::styled("  Passphrase: see the shell that ran `aoe serve`", dimmed),
        }));
    }
    rows
}

pub(super) fn exposure_row(theme: &Theme, width: usize) -> Row {
    let full = " change exposure: localhost only, local network, or internet";
    let label = if full.len() < width {
        full
    } else {
        " change exposure"
    };
    Row::yields(
        Line::from(vec![
            Span::styled("e", Style::default().fg(theme.hint)),
            Span::styled(label, Style::default().fg(theme.dimmed)),
        ]),
        EXPOSURE_NOTE,
    )
}

pub(super) fn qr_lines(theme: &Theme, qr: &str) -> Vec<Line<'static>> {
    qr.lines()
        .map(|line| Line::styled(line.to_string(), Style::default().fg(theme.text)))
        .collect()
}

/// How the card arranges itself for the space it has.
#[derive(Debug, PartialEq)]
pub(super) enum CardLayout {
    /// Pairing, devices and the link in one column.
    Single,
    /// The QR beside everything else.
    Beside,
    /// The link and QR alone, toggled with `w` where they do not fit beside.
    Web,
}

pub(super) fn card_layout(model: &ActiveModel, area: Rect, qr: (usize, usize)) -> CardLayout {
    let (qr_width, qr_height) = qr;
    if qr_width == 0 {
        return CardLayout::Single;
    }
    let beside = CARD_MIN_WIDTH + COLUMN_GAP + qr_width.max(CARD_MIN_WIDTH / 2) + 4;
    if area.width as usize >= beside && area.height as usize >= qr_height + 8 {
        CardLayout::Beside
    } else if model.show_web {
        CardLayout::Web
    } else {
        CardLayout::Single
    }
}

/// The exposed daemon as a centered card sized to its content: status, then
/// pairing, paired devices and browser access, with the key hints last.
pub(super) fn render_active(frame: &mut Frame, area: Rect, theme: &Theme, model: &ActiveModel) {
    let title = if cfg!(feature = "web") {
        " Remote Access "
    } else {
        " Remote API Access "
    };
    let Some(url) = model
        .urls
        .get(model.url_index)
        .or_else(|| model.urls.first())
    else {
        let card = centered(area, 60, 3);
        frame.render_widget(Clear, card);
        frame.render_widget(
            Paragraph::new("The daemon started but has not published a URL yet.")
                .style(Style::default().fg(theme.dimmed))
                .block(card_block(theme, title)),
            card,
        );
        return;
    };
    let command = client_command(&url.url);
    let qr = qr_lines(theme, model.qr);
    let qr_size = (qr.iter().map(Line::width).max().unwrap_or(0), qr.len());
    let layout = card_layout(model, area, qr_size);

    // Borders take two columns and two rows, the padding two columns, and the
    // footer one row.
    let max_width = (area.width as usize).saturating_sub(4).max(1);
    let max_rows = (area.height as usize).saturating_sub(3).max(1);
    let (width, left, right, right_width) = match layout {
        CardLayout::Beside => {
            let right_width = qr_size.0.max(CARD_MIN_WIDTH / 2);
            let left_width = (max_width - COLUMN_GAP - right_width).min(CARD_MAX_WIDTH);
            let mut left = status_rows(theme, model, url, left_width);
            left.push(Row::gap());
            left.extend(pair_rows(theme, model, command.as_deref(), left_width));
            left.push(Row::gap());
            left.extend(device_rows(theme, model, left_width));
            left.push(Row::gap());
            left.extend(web_rows(theme, model, url, left_width));
            left.push(Row::gap());
            left.push(exposure_row(theme, left_width));
            let mut right: Vec<Row> = qr.into_iter().map(Row::keep).collect();
            right.push(Row::keep(Line::styled(
                "Scan with a phone to open the link",
                Style::default().fg(theme.dimmed),
            )));
            let left = fit_rows(left, max_rows, left_width);
            let right = fit_rows(right, max_rows, right_width);
            let used = left.iter().map(Line::width).max().unwrap_or(0);
            (
                used + COLUMN_GAP + right_width,
                left,
                Some(right),
                right_width,
            )
        }
        CardLayout::Web => {
            let width = max_width.min(CARD_MAX_WIDTH);
            let mut rows = status_rows(theme, model, url, width);
            rows.push(Row::gap());
            rows.extend(web_rows(theme, model, url, width));
            if qr_size.0 <= width && rows.len() + 1 + qr_size.1 <= max_rows {
                rows.push(Row::gap());
                rows.extend(qr.into_iter().map(Row::keep));
            } else {
                rows.push(Row::keep(Line::styled(
                    "  The terminal is too small for the QR.",
                    Style::default().fg(theme.dimmed),
                )));
            }
            (width, fit_rows(rows, max_rows, width), None, 0)
        }
        CardLayout::Single => {
            let width = max_width.min(CARD_MAX_WIDTH);
            let mut rows = status_rows(theme, model, url, width);
            rows.push(Row::gap());
            rows.extend(pair_rows(theme, model, command.as_deref(), width));
            rows.push(Row::gap());
            rows.extend(device_rows(theme, model, width));
            rows.push(Row::gap());
            rows.extend(web_rows(theme, model, url, width));
            rows.push(Row::gap());
            rows.push(exposure_row(theme, width));
            let lines = fit_rows(rows, max_rows, width);
            let used = lines.iter().map(Line::width).max().unwrap_or(0);
            (used.clamp(CARD_MIN_WIDTH.min(width), width), lines, None, 0)
        }
    };
    let footer = active_footer(theme, model, &layout, qr_size.0 > 0, width);
    let body_rows = left.len().max(right.as_ref().map_or(0, Vec::len));
    let card = centered(area, width as u16 + 4, body_rows as u16 + 3);
    frame.render_widget(Clear, card);
    let block = card_block(theme, title);
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let [body, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    match right {
        Some(right) => {
            let [left_area, _, right_area] = Layout::horizontal([
                Constraint::Min(1),
                Constraint::Length(COLUMN_GAP as u16),
                Constraint::Length(right_width as u16),
            ])
            .areas(body);
            frame.render_widget(Paragraph::new(left), left_area);
            frame.render_widget(Paragraph::new(right), right_area);
        }
        None => frame.render_widget(Paragraph::new(left), body),
    }
    frame.render_widget(Paragraph::new(footer), foot);
}

pub(super) fn card_block(theme: &Theme, title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            title,
            Style::default().fg(theme.accent).bold(),
        ))
}

/// Key hints in reading order, at most `width` wide: the least needed go
/// first, and `?` lists every key with what it does.
pub(super) fn active_footer(
    theme: &Theme,
    model: &ActiveModel,
    layout: &CardLayout,
    has_qr: bool,
    width: usize,
) -> Line<'static> {
    if let Some(confirm) = model.pending_confirm {
        return Line::styled(
            match confirm {
                PendingConfirm::NewPassphrase => {
                    "Press g again for a new passphrase (clients need it); any other key cancels"
                }
                PendingConfirm::Restart => {
                    "Press r again to restart (clears all sessions); any other key cancels"
                }
            },
            Style::default().fg(theme.waiting).bold(),
        );
    }
    if let Some(flash) = model.flash {
        return Line::styled(flash.to_string(), Style::default().fg(theme.accent));
    }
    let panel = model.pairing;
    let has_devices = panel.is_some_and(|p| p.devices().is_ok_and(|d| !d.is_empty()));
    // (key, label, keep rank): higher ranks survive a narrow footer.
    let mut keys: Vec<(&str, &str, u8)> = Vec::new();
    if *layout == CardLayout::Web {
        keys.push(("w", "back to pairing", 7));
    } else {
        if has_devices {
            keys.push(("↑↓", "select device", 1));
            keys.push(("x", "revoke device", 5));
        }
        if panel.is_some_and(|p| !p.lockouts().is_empty()) {
            keys.push(("u", "unblock IPs", 6));
        }
        if has_qr && *layout == CardLayout::Single {
            keys.push(("w", "show QR", 4));
        }
    }
    if model.urls.len() > 1 {
        keys.push(("Tab", "next URL", 2));
    }
    keys.extend([
        ("e", "change exposure", 3),
        ("?", "help", 8),
        ("Esc", "back to sessions", 9),
    ]);
    let render = |keys: &[(&str, &str, u8)]| {
        let mut spans = Vec::new();
        for (i, (key, label, _)) in keys.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                key.to_string(),
                Style::default().fg(theme.hint),
            ));
            spans.push(Span::styled(
                format!(" {label}"),
                Style::default().fg(theme.dimmed),
            ));
        }
        Line::from(spans)
    };
    let mut line = render(&keys);
    while line.width() > width && keys.len() > 1 {
        let lowest = keys
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, _, rank))| *rank)
            .map_or(0, |(at, _)| at);
        keys.remove(lowest);
        line = render(&keys);
    }
    line
}

/// Every key the exposed view takes, with what it does.
pub(super) fn help_shortcuts(is_tunnel: bool) -> Vec<(&'static str, &'static str)> {
    let mut shortcuts = vec![
        ("↑↓  j k", "Select a paired device."),
        ("x", "Revoke the selected device (press twice)."),
        ("u", "Let locked-out IPs try pairing again."),
        ("w", "Show the browser link and QR code."),
        ("Tab", "Show the next URL, when there are several."),
        ("e", "Change exposure: this machine, LAN, internet."),
        ("r", "Restart the server, ending sessions (twice)."),
    ];
    if is_tunnel {
        shortcuts.push(("g", "New passphrase and restart (press twice)."));
    }
    shortcuts.extend([
        ("?", "Show or hide this help."),
        ("Esc  q", "Back to sessions; the server keeps running."),
    ]);
    shortcuts
}

pub(super) fn render_help_overlay(frame: &mut Frame, area: Rect, theme: &Theme, mode: Exposure) {
    let is_tunnel = mode == Exposure::Tunnel;
    let key_width = 9;
    let mut lines: Vec<Line> = help_shortcuts(is_tunnel)
        .into_iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!("{key:key_width$}"), Style::default().fg(theme.hint)),
                Span::styled(desc, Style::default().fg(theme.text)),
            ])
        })
        .collect();
    if is_tunnel {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "The passphrase is a second factor for the tunnel",
            Style::default().fg(theme.dimmed),
        ));
        lines.push(Line::styled(
            "and persists across restarts.",
            Style::default().fg(theme.dimmed),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Press any key to close",
        Style::default().fg(theme.dimmed),
    ));
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4;
    let dialog_area = centered(area, width, lines.len() as u16 + 2);
    frame.render_widget(Clear, dialog_area);
    let block = Block::default()
        .style(Style::default().bg(theme.background))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            " Remote Access Keys ",
            Style::default().fg(theme.accent).bold(),
        ));
    frame.render_widget(Paragraph::new(lines).block(block), dialog_area);
}

pub(super) fn render_error(frame: &mut Frame, area: Rect, theme: &Theme, msg: &str) {
    // Error copy can be long (multi-line tailscale output, stacked log
    // tail, hints, plus remediation steps). Keep it wide + tall enough
    // that the whole message fits without clipping the bottom. Wrap is
    // still on for individual long lines.
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.error))
        .title(Line::styled(
            " Serve failed ",
            Style::default().fg(theme.error).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(msg)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.text)),
        chunks[0],
    );
    let keybinds = if error_mentions_tailscale(msg) {
        "[R] Reset tailscale funnel    [Enter] Close"
    } else {
        "[Enter] Close"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            keybinds,
            Style::default().fg(theme.dimmed),
        )))
        .alignment(Alignment::Center),
        chunks[1],
    );
}

pub(super) fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{}h {:02}m", h, m)
    } else if m > 0 {
        format!("{}m {:02}s", m, s)
    } else {
        format!("{}s", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::serve::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const TOKEN_URL: &str = "http://192.168.1.42:8081/?token=abc123def456";

    struct Setup {
        width: u16,
        height: u16,
        url: String,
        show_web: bool,
        qr: String,
        panel: crate::tui::dialogs::pairing::PairingPanel,
        mode: Exposure,
    }

    fn setup(width: u16, height: u16) -> Setup {
        Setup {
            width,
            height,
            url: TOKEN_URL.to_string(),
            show_web: false,
            qr: String::new(),
            panel: crate::tui::dialogs::pairing::PairingPanel::ready("K7F-3QX", &["laptop"]),
            mode: Exposure::Network,
        }
    }

    /// A stand-in the size of the QR a tokenized LAN URL draws.
    fn fake_qr() -> String {
        vec!["█".repeat(45); 23].join("\n")
    }

    impl Setup {
        fn draw(&self) -> (String, Rect) {
            let urls = vec![ServeUrl {
                label: Some("lan".to_string()),
                url: self.url.clone(),
            }];
            let mut term =
                Terminal::new(TestBackend::new(self.width, self.height)).expect("terminal");
            term.draw(|f| {
                render_active(
                    f,
                    f.area(),
                    &Theme::default(),
                    &ActiveModel {
                        mode: self.mode,
                        urls: &urls,
                        url_index: 0,
                        passphrase: Some("amber copper navy teal"),
                        elapsed: Duration::from_secs(42),
                        pending_confirm: None,
                        pairing: Some(&self.panel),
                        show_web: self.show_web,
                        flash: None,
                        qr: &self.qr,
                    },
                )
            })
            .expect("draw");
            let buf = term.backend().buffer().clone();
            let rows: Vec<String> = (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect();
            let top = rows.iter().position(|r| r.contains('╭')).expect("card");
            let bottom = rows.iter().rposition(|r| r.contains('╰')).expect("card");
            let left = rows[top].chars().position(|c| c == '╭').unwrap_or(0);
            let right = rows[top].chars().position(|c| c == '╮').unwrap_or(0);
            let card = Rect::new(
                left as u16,
                top as u16,
                (right - left + 1) as u16,
                (bottom - top + 1) as u16,
            );
            (rows.join("\n"), card)
        }
    }

    fn position(screen: &str, needle: &str) -> usize {
        screen
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing:\n{screen}"))
    }

    fn inner_rows(screen: &str, card: Rect) -> Vec<String> {
        screen
            .lines()
            .skip(card.y as usize + 1)
            .take(card.height.saturating_sub(2) as usize)
            .map(|row| {
                row.chars()
                    .skip(card.x as usize + 1)
                    .take(card.width.saturating_sub(2) as usize)
                    .collect()
            })
            .collect()
    }

    /// At every size the card reads status, then the code and its steps,
    /// then devices, with described hints; nothing is clipped and it carries
    /// no stretch of empty rows.
    #[test]
    fn the_card_reads_in_order_at_every_size_without_dead_space() {
        for (width, height) in [(60, 20), (100, 30), (140, 40)] {
            let (screen, card) = setup(width, height).draw();
            let order = [
                "● Sharing on local network · 192.168.1.42:8081",
                "Pair a device",
                "K 7 F - 3 Q X",
                "1. On the other machine run",
                "aoe remote add 192.168.1.42:8081",
                "2. Enter the code above when it asks",
                "Paired devices",
                "laptop  192.168.1.9 · seen 2m ago",
            ];
            let positions: Vec<usize> = order.iter().map(|n| position(&screen, n)).collect();
            assert!(
                positions.windows(2).all(|w| w[0] < w[1]),
                "{width}x{height}:\n{screen}"
            );
            for hint in ["? help", "Esc back to sessions", "x revoke laptop"] {
                position(&screen, hint);
            }
            assert!(
                !screen.contains('…'),
                "clipped at {width}x{height}:\n{screen}"
            );
            let rows = inner_rows(&screen, card);
            let blank = rows.iter().filter(|r| r.trim().is_empty()).count();
            assert!(
                blank <= 4,
                "{blank} blank rows at {width}x{height}:\n{screen}"
            );
            assert!(
                !rows
                    .windows(2)
                    .any(|w| w[0].trim().is_empty() && w[1].trim().is_empty()),
                "{width}x{height}:\n{screen}"
            );
            assert!(card.width <= width && card.height <= height);
        }
        let roomy = setup(100, 30).draw().0;
        position(
            &roomy,
            "Other aoe clients on this network can connect once paired.",
        );
        position(
            &roomy,
            "e change exposure: localhost only, local network, or internet",
        );
    }

    #[test]
    fn devices_show_an_empty_state_an_armed_revoke_and_lockouts() {
        let mut empty = setup(100, 30);
        empty.panel = crate::tui::dialogs::pairing::PairingPanel::ready("K7F-3QX", &[]);
        let screen = empty.draw().0;
        position(&screen, "No devices paired yet.");
        assert!(!screen.contains("revoke"), "{screen}");

        let mut armed = setup(100, 30);
        armed
            .panel
            .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        position(&armed.draw().0, "press x again to revoke laptop");

        let mut locked = setup(100, 30);
        locked.panel = crate::tui::dialogs::pairing::PairingPanel::ready("K7F-3QX", &["laptop"])
            .with_lockout("100.89.98.53", 725);
        let screen = locked.draw().0;
        let lockout = position(&screen, "100.89.98.53 locked out · 12m left  u unblock");
        assert!(lockout < position(&screen, "K 7 F - 3 Q X"), "{screen}");
        position(&screen, "u unblock IPs");
    }

    /// The QR sits beside pairing when both fit; otherwise it waits behind `w`,
    /// which swaps the card to the link and QR.
    #[test]
    fn the_qr_sits_beside_pairing_when_it_fits_and_behind_w_when_not() {
        let mut wide = setup(140, 40);
        wide.qr = fake_qr();
        let (screen, card) = wide.draw();
        let row = screen
            .lines()
            .find(|row| row.contains("Pair a device"))
            .expect("pairing row");
        assert!(row.contains('█'), "beside:\n{screen}");
        assert!(card.width < 140, "{screen}");

        let mut narrow = setup(100, 30);
        narrow.qr = fake_qr();
        let screen = narrow.draw().0;
        assert!(!screen.contains('█'), "{screen}");
        position(&screen, "w show QR");
        narrow.show_web = true;
        let screen = narrow.draw().0;
        position(&screen, "Browser or phone");
        position(&screen, "w back to pairing");
        assert!(!screen.contains("Pair a device"), "{screen}");
    }

    /// The card is wide enough for a real token URL to stand on one line,
    /// and still folds it when the terminal is narrow.
    #[test]
    fn a_token_url_stands_on_one_line_when_the_terminal_has_room() {
        let url = format!("http://192.168.1.42:8081/?token={}", "a1b2c3d4".repeat(8));
        let mut wide = setup(140, 40);
        wide.url.clone_from(&url);
        wide.show_web = true;
        let (screen, card) = wide.draw();
        position(&screen, &url);
        assert!(card.width <= 140, "{screen}");

        let mut narrow = setup(100, 30);
        narrow.url.clone_from(&url);
        narrow.show_web = true;
        let (screen, card) = narrow.draw();
        assert!(!screen.contains(&url), "folded when narrow:\n{screen}");
        assert!(card.width <= 100, "{screen}");
    }

    /// A tunnel adds its passphrase to the link section.
    #[test]
    fn a_tunnel_shows_its_passphrase_with_the_link() {
        let mut tunnel = setup(100, 30);
        tunnel.mode = Exposure::Tunnel;
        let screen = tunnel.draw().0;
        position(&screen, "Passphrase amber copper navy teal");
        position(&screen, "Sharing over the internet");
    }

    /// Without the bundle the title and the link section say the endpoint is
    /// API-only, since a browser following the link gets a 404.
    #[test]
    fn active_screen_says_api_only_without_web() {
        let screen = setup(120, 40).draw().0;
        for needle in ["Remote API Access", "This build has no dashboard"] {
            assert_eq!(
                screen.contains(needle),
                !cfg!(feature = "web"),
                "{needle:?}:\n{screen}"
            );
        }
    }

    #[test]
    fn the_client_command_is_the_short_address_and_never_loopback() {
        for (url, expected) in [
            (
                "http://192.168.1.20:54321/?token=abc123",
                Some("aoe remote add 192.168.1.20:54321"),
            ),
            (
                "https://aoe-mini.tailnet.ts.net/?token=abc123",
                Some("aoe remote add aoe-mini.tailnet.ts.net"),
            ),
            ("http://127.0.0.1:54321/?token=abc123", None),
            ("http://[::1]:54321/", None),
        ] {
            assert_eq!(client_command(url).as_deref(), expected, "{url}");
        }
    }

    #[test]
    fn format_elapsed_shows_units() {
        assert_eq!(format_elapsed(Duration::from_secs(5)), "5s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_elapsed(Duration::from_secs(3600 + 120)), "1h 02m");
    }

    /// Help names every key the exposed view takes and still fits the
    /// smallest supported terminal.
    #[test]
    fn help_lists_every_key_and_fits_a_small_terminal() {
        for tunnel in [false, true] {
            let keys: Vec<&str> = help_shortcuts(tunnel).iter().map(|(k, _)| *k).collect();
            for key in ["↑↓  j k", "x", "u", "w", "Tab", "e", "r", "?", "Esc  q"] {
                assert!(keys.contains(&key), "{key} missing: {keys:?}");
            }
            assert_eq!(keys.contains(&"g"), tunnel);
        }
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        term.draw(|f| render_help_overlay(f, f.area(), &Theme::default(), Exposure::Tunnel))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let screen: String = (0..20)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        for (_, desc) in help_shortcuts(true) {
            assert!(screen.contains(desc), "{desc:?} clipped:\n{screen}");
        }
    }
}

//! Full-page screens for each serve view state.

use std::time::Duration;

use ratatui::prelude::*;
use ratatui::widgets::*;

use crossterm::event::{KeyCode, KeyEvent};

use super::daemon::error_mentions_tailscale;
use super::{
    Flash, PendingConfirm, ServeMode, ServeMouse, ServeView, ServeViewState, TransportStatus,
    TunnelTransport, FLASH_TTL,
};
use crate::tui::components::hint_buttons::scan_spaced_hints;
use crate::tui::components::hover::paint_hover_bg;
use crate::tui::dialogs::centered_rect;
use crate::tui::styles::Theme;

/// Shown in place of the QR when the dashboard bundle is not embedded.
const API_ONLY_NOTICE: &str =
    "No dashboard bundle in this build: the URL serves the REST API only, and a browser gets a 404.";
const API_ONLY_ROWS: u16 = 2;

pub(super) fn render(view: &ServeView, frame: &mut Frame, area: Rect, theme: &Theme) {
    let mut mouse = view.mouse.borrow_mut();
    mouse.keys.clear();
    mouse.copies.clear();
    match &view.state {
        ServeViewState::ModePicker {
            selected,
            tunnel_available,
            local_available,
            flash,
        } => render_mode_picker(
            frame,
            area,
            theme,
            &mut mouse,
            *selected,
            (*local_available, *tunnel_available),
            flash_text(flash),
        ),
        ServeViewState::Confirm {
            selected,
            tailscale,
            cloudflare,
            flash,
        } => render_confirm(
            frame,
            area,
            theme,
            &mut mouse,
            *selected,
            (*tailscale, *cloudflare),
            flash_text(flash),
        ),
        ServeViewState::Starting {
            mode, started_at, ..
        } => render_starting(frame, area, theme, &mut mouse, *mode, started_at.elapsed()),
        ServeViewState::Active { mode, .. } => {
            render_active(view, frame, area, theme, &mut mouse);
            if view.show_help {
                // Any click closes the overlay, so nothing beneath is a target.
                mouse.keys.clear();
                mouse.copies.clear();
                render_help_overlay(frame, area, theme, *mode);
            }
        }
        ServeViewState::Error(msg) => render_error(frame, area, theme, &mut mouse, msg),
    }
    if let Some(rect) = mouse.hover.current_in(&mouse.rects()) {
        paint_hover_bg(frame, rect, theme.selection);
    }
}

/// Record the hints drawn in `area` as click targets.
fn record_hints(frame: &mut Frame, mouse: &mut ServeMouse, area: Rect) {
    mouse
        .keys
        .extend(scan_spaced_hints(frame.buffer_mut(), area));
}

/// A card click selects it, and a second click on the selected card confirms.
fn card_key(selected: bool, select: KeyCode) -> KeyEvent {
    KeyEvent::from(if selected { KeyCode::Enter } else { select })
}

fn flash_text(flash: &Flash) -> &str {
    flash.as_ref().map_or("", |(m, _)| m.as_str())
}

/// Clear `area`, draw a titled rounded border, and return the inner area.
fn render_page(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    border: Color,
    title_color: Color,
) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Line::styled(title, Style::default().fg(title_color).bold()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn centered_line(frame: &mut Frame, area: Rect, text: &str, style: Style) {
    frame.render_widget(
        Paragraph::new(Line::styled(text, style)).alignment(Alignment::Center),
        area,
    );
}

/// Draw a selectable card border and return its inner area and body text color.
fn render_card(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    selected: bool,
    available: bool,
) -> (Rect, Color) {
    let (border, title_color, body) = match (selected, available) {
        (_, false) => (theme.dimmed, theme.dimmed, theme.dimmed),
        (true, true) => (theme.accent, theme.accent, theme.text),
        (false, true) => (theme.border, theme.title, theme.text),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            format!(" {title} "),
            Style::default().fg(title_color).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    (inner, body)
}

/// Rows of a centered `width` x `height` card page: head, two cards, flash and keybinds.
fn card_page(
    inner: Rect,
    (width, height): (u16, u16),
    head: u16,
    min_cards: u16,
) -> (Rect, [Rect; 2], Rect, Rect) {
    let content = centered_rect(inner, width, height);
    let [head_area, cards, flash, keys] = Layout::vertical([
        Constraint::Length(head),
        Constraint::Min(min_cards),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(content);
    let [left, _, right] = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Length(1),
        Constraint::Percentage(50),
    ])
    .areas(cards);
    (head_area, [left, right], flash, keys)
}

fn render_mode_picker(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    mouse: &mut ServeMouse,
    selected: ServeMode,
    (local_available, tunnel_available): (bool, bool),
    flash: &str,
) {
    let inner = render_page(frame, area, " Remote Access ", theme.accent, theme.accent);
    // The head is the question plus a spacer row.
    let (question, [local, tunnel], flash_area, keys) = card_page(inner, (72, 13), 2, 7);
    centered_line(
        frame,
        question,
        "How should this be reachable?",
        Style::default().fg(theme.title).bold(),
    );

    let local_primary = crate::server::discover_tagged_ips()
        .into_iter()
        .next()
        .map(|(kind, ip)| match kind {
            crate::server::IpKind::Tailscale => format!("{} (Tailscale)", ip),
            crate::server::IpKind::Lan => format!("{} (LAN)", ip),
            crate::server::IpKind::Loopback => format!("{} (loopback)", ip),
        })
        .unwrap_or_else(|| "only localhost available".to_string());
    let tunnel_status = if tunnel_available {
        "reachable from your phone"
    } else {
        "no tunnel tool installed"
    };
    let cards = [
        (
            local,
            "Local network",
            ServeMode::Local,
            local_available,
            local_primary,
            ["Token auth, no passphrase.", "LAN + Tailscale. Instant."],
            "  (no non-loopback interface)",
        ),
        (
            tunnel,
            "Internet (HTTPS)",
            ServeMode::Tunnel,
            tunnel_available,
            tunnel_status.to_string(),
            [
                "Token + passphrase (2FA).",
                "Pick transport on next screen.",
            ],
            "  (brew install tailscale or cloudflared)",
        ),
    ];
    for (card_area, title, mode, available, headline, body_lines, unavailable_note) in cards {
        let select = match mode {
            ServeMode::Local => KeyCode::Char('h'),
            ServeMode::Tunnel => KeyCode::Char('l'),
        };
        mouse
            .keys
            .push((card_key(selected == mode, select), card_area));
        let (card_inner, body) =
            render_card(frame, card_area, theme, title, selected == mode, available);
        let headline_color = if available {
            theme.accent
        } else {
            theme.dimmed
        };
        let mut lines = vec![
            Line::from(""),
            Line::styled(headline, Style::default().fg(headline_color)),
            Line::from(""),
        ];
        lines.extend(body_lines.map(|l| Line::styled(l, Style::default().fg(body))));
        lines.push(if available {
            Line::from("")
        } else {
            Line::styled(unavailable_note, Style::default().fg(theme.dimmed))
        });
        frame.render_widget(Paragraph::new(lines), card_inner);
    }

    centered_line(
        frame,
        flash_area,
        flash,
        Style::default().fg(theme.error).bold(),
    );
    centered_line(
        frame,
        keys,
        "[←/→] choose   [L] Local   [T] Tunnel   [Enter] confirm   [Esc] cancel",
        Style::default().fg(theme.dimmed),
    );
    record_hints(frame, mouse, keys);
}

fn render_confirm(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    mouse: &mut ServeMouse,
    selected: TunnelTransport,
    (tailscale, cloudflare): (TransportStatus, TransportStatus),
    flash: &str,
) {
    let inner = render_page(
        frame,
        area,
        " Expose to Internet? ",
        theme.accent,
        theme.accent,
    );
    let (head, [ts_area, cf_area], flash_area, keys) = card_page(inner, (88, 19), 7, 8);
    let [risk_area, pick_area] =
        Layout::vertical([Constraint::Length(6), Constraint::Length(1)]).areas(head);

    let text = Style::default().fg(theme.text);
    let heading = Style::default().fg(theme.title).bold();
    let bullet = Style::default().fg(theme.running);
    let risk = vec![
        Line::styled(
            "Your sessions become reachable from anywhere over HTTPS.",
            text,
        ),
        Line::from(""),
        Line::styled("Two factors required to log in:", heading),
        Line::from(vec![
            Span::styled("  \u{2022} ", bullet),
            Span::styled("token (in the URL / QR code)", text),
        ]),
        Line::from(vec![
            Span::styled("  \u{2022} ", bullet),
            Span::styled("passphrase (typed on the login page)", text),
        ]),
        Line::styled(
            "Don't share screenshots with BOTH. Stop with [S] when done.",
            Style::default().fg(theme.dimmed),
        ),
    ];
    frame.render_widget(Paragraph::new(risk), risk_area);
    frame.render_widget(
        Paragraph::new(Line::styled("Pick a transport:", heading)),
        pick_area,
    );

    let cards = [
        (
            ts_area,
            "Tailscale Funnel",
            TunnelTransport::Tailscale,
            tailscale,
            [
                "Stable URL across restarts",
                "PWA-friendly on phones",
                "https://<host>.<tailnet>.ts.net",
            ],
            "Not installed (tailscale up)",
        ),
        (
            cf_area,
            "Cloudflare Tunnel",
            TunnelTransport::Cloudflare,
            cloudflare,
            [
                "Works anywhere",
                "URL rotates each restart",
                "Not PWA-friendly",
            ],
            "Not installed (brew install cloudflared)",
        ),
    ];
    for (card_area, title, transport, status, body_lines, not_installed) in cards {
        let select = match transport {
            TunnelTransport::Tailscale => KeyCode::Char('h'),
            TunnelTransport::Cloudflare => KeyCode::Char('l'),
        };
        mouse
            .keys
            .push((card_key(selected == transport, select), card_area));
        let ready = status == TransportStatus::Ready;
        let (card_inner, body) =
            render_card(frame, card_area, theme, title, selected == transport, ready);
        let mut lines = vec![Line::from("")];
        lines.extend(body_lines.map(|l| Line::styled(l, Style::default().fg(body))));
        lines.push(Line::from(""));
        let (icon, status_text, status_style) = match status {
            TransportStatus::Ready => (
                "\u{2713}",
                "Ready",
                Style::default().fg(theme.running).bold(),
            ),
            TransportStatus::NotInstalled => {
                ("\u{26A0}", not_installed, Style::default().fg(theme.dimmed))
            }
            TransportStatus::FunnelNotEnabled => (
                "\u{26A0}",
                "Funnel not enabled for this node",
                Style::default().fg(theme.error).bold(),
            ),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{icon} "), status_style),
            Span::styled(status_text, status_style),
        ]));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), card_inner);
    }

    centered_line(
        frame,
        flash_area,
        flash,
        Style::default().fg(theme.error).bold(),
    );
    centered_line(
        frame,
        keys,
        "[←/→] select  [T] Tailscale  [C] Cloudflare  [R] refresh  [Enter] confirm  [Esc] cancel",
        Style::default().fg(theme.dimmed),
    );
    record_hints(frame, mouse, keys);
}

fn render_starting(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    mouse: &mut ServeMouse,
    mode: ServeMode,
    elapsed: Duration,
) {
    let (title, wait_line1, wait_line2) = match mode {
        ServeMode::Tunnel => (
            " Starting HTTPS tunnel... ",
            "Waiting for the daemon to bring the tunnel up",
            "(first-time Tailscale cert provisioning can take 30\u{2013}60s).",
        ),
        ServeMode::Local => (
            " Starting local server... ",
            "Binding on 0.0.0.0 and discovering interfaces",
            "(usually under a second).",
        ),
    };
    let inner = render_page(frame, area, title, theme.border, theme.title);
    let text = Style::default().fg(theme.text);
    let banner = vec![
        Line::from(""),
        Line::styled(wait_line1, text),
        Line::styled(wait_line2, text),
        Line::from(""),
        Line::styled(
            format!("Elapsed: {}s    [Esc close]  [S stop]", elapsed.as_secs()),
            Style::default().fg(theme.dimmed),
        ),
    ];
    let banner_area = centered_rect(inner, inner.width, 5);
    frame.render_widget(
        Paragraph::new(banner).alignment(Alignment::Center),
        banner_area,
    );
    let keys_row = Rect {
        y: banner_area.bottom().saturating_sub(1),
        height: 1,
        ..banner_area
    };
    record_hints(frame, mouse, keys_row);
}

/// The scannable block for `url`. Empty without the dashboard bundle: a scan would reach no page.
#[cfg(feature = "web")]
fn render_qr(url: &str) -> String {
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
fn render_qr(_url: &str) -> String {
    String::new()
}

fn render_active(
    view: &ServeView,
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    mouse: &mut ServeMouse,
) {
    let ServeViewState::Active {
        mode,
        urls,
        url_index,
        passphrase,
        opened_at,
        ..
    } = &view.state
    else {
        return;
    };
    let Some(active_url) = urls.get(*url_index).or_else(|| urls.first()) else {
        render_error(
            frame,
            area,
            theme,
            mouse,
            "Daemon started but no URL available yet.",
        );
        return;
    };
    let elapsed = opened_at.elapsed();
    let is_tunnel = *mode == ServeMode::Tunnel;
    let dimmed = Style::default().fg(theme.dimmed);
    let accent = Style::default().fg(theme.accent);

    frame.render_widget(Clear, area);
    let [header, content, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(10),
        Constraint::Length(3),
    ])
    .areas(area);

    let header_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(theme.border));
    let header_inner = header_block.inner(header);
    frame.render_widget(header_block, header);
    let long_running = elapsed >= Duration::from_secs(8 * 3600);
    let title_color = if long_running {
        theme.waiting
    } else {
        theme.title
    };
    let mode_label = if is_tunnel { "tunnel" } else { "local" };
    let title = if cfg!(feature = "web") {
        format!(" Remote Access ({mode_label})")
    } else {
        format!(" Remote API Access ({mode_label})")
    };
    let mut header_spans = vec![
        Span::styled(title, Style::default().fg(title_color).bold()),
        Span::styled(format!("  open {}", format_elapsed(elapsed)), dimmed),
    ];
    if long_running {
        header_spans.push(Span::styled(
            "  still need it?",
            Style::default().fg(theme.waiting),
        ));
    }
    if let Some((label, _)) = view.copied.filter(|(_, at)| at.elapsed() < FLASH_TTL) {
        header_spans.push(Span::styled(
            format!("  copied {label}"),
            Style::default().fg(theme.running),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), header_inner);

    let url = active_url.url.as_str();
    let qr_text = render_qr(url);
    let qr_lines: Vec<Line> = qr_text
        .lines()
        .map(|l| Line::styled(l, Style::default().fg(theme.text)))
        .collect();
    let center = |line: Line<'static>| Paragraph::new(line).alignment(Alignment::Center);
    let labeled = |label: &'static str, value: String, style: Style| {
        center(Line::from(vec![
            Span::styled(label, dimmed),
            Span::styled(value, style),
        ]))
    };

    // Each row may carry a `(label, value)` whose value a click copies.
    type Copy = Option<(&'static str, String)>;
    let mut rows: Vec<(u16, Paragraph, Copy)> = vec![(
        qr_lines.len() as u16,
        Paragraph::new(qr_lines).alignment(Alignment::Center),
        None,
    )];
    if !cfg!(feature = "web") {
        rows.push((
            API_ONLY_ROWS,
            Paragraph::new(API_ONLY_NOTICE)
                .style(dimmed)
                .wrap(Wrap { trim: true })
                .alignment(Alignment::Center),
            None,
        ));
    }
    rows.push((1, Paragraph::new(""), None));
    if let Some(label) = &active_url.label {
        rows.push((
            1,
            center(Line::styled(format!("via {}", label), dimmed.italic())),
            None,
        ));
    }
    let url_fits =
        "URL: ".len() + url.chars().count() <= content.width.saturating_sub(2).max(1) as usize;
    let (base_url, token) = split_url_and_token(url);
    // The full URL is copied even when it wraps onto a token row.
    if url_fits {
        rows.push((
            1,
            labeled("URL: ", url.to_string(), accent),
            Some(("URL: ", url.to_string())),
        ));
    } else {
        rows.push((
            1,
            labeled("URL: ", base_url.clone(), accent),
            Some(("URL: ", base_url)),
        ));
        if let Some(token) = token {
            rows.push((
                1,
                labeled("Token: ", token.to_string(), accent),
                Some(("Token: ", token.to_string())),
            ));
        }
    }
    if is_tunnel {
        let (value, style, copy) = match passphrase {
            Some(pp) => (
                pp.clone(),
                accent.bold(),
                Some(("Passphrase: ", pp.clone())),
            ),
            None => (
                "(set when the daemon started; check the shell that ran `aoe serve`)".to_string(),
                dimmed,
                None,
            ),
        };
        rows.push((1, labeled("Passphrase: ", value, style), copy));
    }

    let total: u16 = rows.iter().map(|(h, _, _)| h).sum();
    let top_pad = Constraint::Length(content.height.saturating_sub(total) / 2);
    let constraints = std::iter::once(top_pad)
        .chain(rows.iter().map(|(h, _, _)| Constraint::Length(*h)))
        .chain(std::iter::once(Constraint::Min(0)));
    let chunks = Layout::vertical(constraints)
        .horizontal_margin(1)
        .split(content);
    for ((_, row, copy), chunk) in rows.into_iter().zip(chunks.iter().skip(1)) {
        frame.render_widget(row, *chunk);
        if let Some((label, value)) = copy {
            let label_w = label.len() as u16;
            let value_w = value.chars().count() as u16;
            let x = crate::tui::dialogs::centered_x(*chunk, label_w + value_w) + label_w;
            let width = value_w.min(chunk.right().saturating_sub(x));
            let what = label.trim_end_matches([':', ' ']);
            mouse
                .copies
                .push(((what, value), Rect::new(x, chunk.y, width, 1)));
        }
    }

    let footer_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.border));
    let footer_inner = footer_block.inner(footer);
    frame.render_widget(footer_block, footer);
    let footer_line = match view.pending_confirm.map(|(action, _)| action) {
        Some(action) => {
            let text = match action {
                PendingConfirm::NewPassphrase => "Press G again to confirm new passphrase (clients will need it). Any other key cancels.",
                PendingConfirm::Restart => "Press R again to confirm restart (clears all sessions). Any other key cancels.",
            };
            Line::styled(text, Style::default().fg(theme.waiting).bold())
        }
        None => {
            let mut keys = Vec::new();
            if urls.len() > 1 {
                keys.push(("Tab", ": URL  "));
            }
            if is_tunnel {
                keys.push(("G", ": new pass  "));
            }
            keys.extend([
                ("R", ": restart  "),
                ("S", ": stop  "),
                ("?", ": help  "),
                ("Esc", ": close"),
            ]);
            Line::from(
                keys.into_iter()
                    .flat_map(|(key, desc)| [Span::styled(key, accent), Span::styled(desc, dimmed)])
                    .collect::<Vec<_>>(),
            )
        }
    };
    frame.render_widget(
        Paragraph::new(footer_line).alignment(Alignment::Center),
        footer_inner,
    );
    record_hints(frame, mouse, footer_inner);
}

fn render_help_overlay(frame: &mut Frame, area: Rect, theme: &Theme, mode: ServeMode) {
    let is_tunnel = mode == ServeMode::Tunnel;
    let width = 72.min(area.width.saturating_sub(4));
    let height = (if is_tunnel { 20 } else { 14 }).min(area.height.saturating_sub(4));
    let dialog_area = centered_rect(area, width, height);

    let heading = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(Clear, dialog_area);
    let block = Block::default()
        .style(Style::default().bg(theme.background))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(" Remote Access Help ")
        .title_style(heading);
    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let text = Style::default().fg(theme.text);
    let mut shortcuts = Vec::new();
    if is_tunnel {
        shortcuts.push(("G", "New random passphrase and restart server"));
    }
    shortcuts.extend([
        ("R", "Restart server (clears all client sessions)"),
        ("S", "Stop server, return to mode picker"),
        ("Tab", "Cycle URLs (when multiple available)"),
        ("Esc / q", "Close this view (server keeps running)"),
        ("?", "Toggle this help"),
    ]);

    let mut lines = vec![
        Line::from(""),
        Line::styled("Keyboard Shortcuts", heading),
        Line::from(""),
    ];
    lines.extend(shortcuts.into_iter().map(|(key, desc)| {
        Line::from(vec![
            Span::styled(format!("  {:14}", key), Style::default().fg(theme.waiting)),
            Span::styled(desc, text),
        ])
    }));
    lines.push(Line::from(""));
    if is_tunnel {
        lines.extend([
            Line::styled("About the passphrase", heading),
            Line::styled("  Second factor for internet-exposed tunnels.", text),
            Line::styled("  Persists across stop/start. Press G to rotate.", text),
        ]);
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "  Press any key to close",
        Style::default().fg(theme.dimmed),
    ));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Split `https://host/?token=XYZ` into base URL and token for narrow screens.
fn split_url_and_token(url: &str) -> (String, Option<&str>) {
    if let Some(q_start) = url.find("?token=") {
        let token = url[q_start + "?token=".len()..]
            .split('&')
            .next()
            .unwrap_or("");
        if !token.is_empty() {
            return (
                url[..q_start].trim_end_matches('?').to_string(),
                Some(token),
            );
        }
    }
    (url.to_string(), None)
}

fn render_error(frame: &mut Frame, area: Rect, theme: &Theme, mouse: &mut ServeMouse, msg: &str) {
    let inner = render_page(frame, area, " Serve failed ", theme.error, theme.error);
    let [body, keys] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)])
        .margin(1)
        .areas(inner);
    frame.render_widget(
        Paragraph::new(msg)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.text)),
        body,
    );
    let keybinds = if error_mentions_tailscale(msg) {
        "[S] Force-stop daemon    [R] Reset tailscale funnel    [Enter] Close"
    } else {
        "[S] Force-stop daemon    [Enter] Close"
    };
    centered_line(frame, keys, keybinds, Style::default().fg(theme.dimmed));
    record_hints(frame, mouse, keys);
}

fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
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
    use crate::cli::serve::ServeUrl;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::Instant;

    #[test]
    fn format_elapsed_shows_units() {
        assert_eq!(format_elapsed(Duration::from_secs(5)), "5s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_elapsed(Duration::from_secs(3600 + 120)), "1h 02m");
    }

    #[test]
    fn split_url_and_token_extracts_token() {
        let base = "https://foo.trycloudflare.com/";
        for (url, token) in [
            (
                "https://foo.trycloudflare.com/?token=abc123def456",
                Some("abc123def456"),
            ),
            ("https://foo.trycloudflare.com/", None),
            (
                "https://foo.trycloudflare.com/?token=abc123&foo=bar",
                Some("abc123"),
            ),
        ] {
            assert_eq!(split_url_and_token(url), (base.to_string(), token));
        }
    }

    /// The URL must survive the QR being compiled out, and a build without the dashboard
    /// bundle must say the endpoint is API-only instead of drawing a code nothing answers.
    #[test]
    fn active_screen_matches_the_dashboard_feature() {
        let view = ServeView {
            state: ServeViewState::Active {
                mode: ServeMode::Local,
                transport: None,
                urls: vec![ServeUrl {
                    label: Some("lan".to_string()),
                    url: "http://192.168.1.42:8080/?t=abc123def456".to_string(),
                }],
                url_index: 0,
                passphrase: None,
                opened_at: Instant::now(),
                log_offset: 0,
            },
            pending_passphrase: String::new(),
            pending_confirm: None,
            show_help: false,
            mouse: Default::default(),
            copied: None,
        };
        let mut term = Terminal::new(TestBackend::new(72, 26)).unwrap();
        term.draw(|f| render(&view, f, f.area(), &Theme::default()))
            .unwrap();
        let buf = term.backend().buffer();
        let screen: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .map(|(x, y)| {
                let sym = buf[(x, y)].symbol();
                if x + 1 == buf.area.width {
                    format!("{sym}\n")
                } else {
                    sym.to_string()
                }
            })
            .collect();

        assert!(
            screen.contains("http://192.168.1.42:8080/?t=abc123def456"),
            "{screen}"
        );
        let qr_drawn = screen
            .chars()
            .any(|c| ['\u{2588}', '\u{2580}', '\u{2584}'].contains(&c));
        assert_eq!(qr_drawn, cfg!(feature = "web"), "{screen}");
        for needle in ["Remote API Access", "No dashboard bundle in this build:"] {
            assert_eq!(
                screen.contains(needle),
                !cfg!(feature = "web"),
                "{needle:?}\n{screen}"
            );
        }
    }

    fn view(state: ServeViewState) -> ServeView {
        ServeView {
            state,
            pending_passphrase: String::new(),
            pending_confirm: None,
            show_help: false,
            mouse: Default::default(),
            copied: None,
        }
    }

    fn draw(view: &ServeView) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(100, 40)).unwrap();
        term.draw(|f| render(view, f, f.area(), &Theme::default()))
            .unwrap();
        term.backend().buffer().clone()
    }

    #[test]
    fn active_screen_hints_click_and_values_are_copy_targets() {
        use crate::tui::dialogs::test_render::find;
        let url = "https://host.example.ts.net/?token=abc123";
        let mut view = view(ServeViewState::Active {
            mode: ServeMode::Tunnel,
            transport: None,
            urls: vec![ServeUrl {
                label: None,
                url: url.to_string(),
            }],
            url_index: 0,
            passphrase: Some("correct-horse".to_string()),
            opened_at: Instant::now(),
            log_offset: 0,
        });
        let buf = draw(&view);
        let copies: Vec<(String, String)> = view
            .mouse
            .borrow()
            .copies
            .iter()
            .map(|((label, value), r)| {
                let drawn: String = (r.x..r.right()).map(|x| buf[(x, r.y)].symbol()).collect();
                assert_eq!(&drawn, value, "{label} rect covers its drawn value");
                (label.to_string(), value.clone())
            })
            .collect();
        assert_eq!(
            copies,
            [("URL", url), ("Passphrase", "correct-horse")].map(|(l, v)| (l.into(), v.into()))
        );

        let (x, y) = find(&buf, "R: restart");
        assert!(view.handle_hover(x, y));
        assert_eq!(
            view.handle_click(x, y).map(|k| k.code),
            Some(KeyCode::Char('R'))
        );

        view.show_help = true;
        draw(&view);
        assert_eq!(view.handle_click(0, 0).map(|k| k.code), Some(KeyCode::Esc));
    }

    #[test]
    fn a_card_click_selects_it_then_confirms_it() {
        let mut view = view(ServeViewState::ModePicker {
            selected: ServeMode::Local,
            tunnel_available: true,
            local_available: true,
            flash: None,
        });
        let buf = draw(&view);
        let (x, y) = crate::tui::dialogs::test_render::find(&buf, "Internet (HTTPS)");
        assert_eq!(
            view.handle_click(x, y).map(|k| k.code),
            Some(KeyCode::Char('l'))
        );
        view.handle_key(KeyEvent::from(KeyCode::Char('l')));
        draw(&view);
        assert_eq!(
            view.handle_click(x, y).map(|k| k.code),
            Some(KeyCode::Enter)
        );
        let buf = draw(&view);
        let (x, y) = crate::tui::dialogs::test_render::find(&buf, "[Esc] cancel");
        assert_eq!(view.handle_click(x, y).map(|k| k.code), Some(KeyCode::Esc));
    }
}

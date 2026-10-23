//! Full-page screens for each serve view state.

use std::time::Duration;

use ratatui::prelude::*;
use ratatui::widgets::*;

use crossterm::event::{KeyCode, KeyEvent};

use super::{
    funnel_reset_applies, Flash, LinkChange, Logins, PendingConfirm, RestartEffect, ServeMode,
    ServeMouse, ServeView, ServeViewState, TransportStatus, TunnelTransport, FLASH_TTL,
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
    if view.stop_requested {
        render_starting(
            frame,
            area,
            theme,
            &mut mouse,
            ServeMode::Local,
            Duration::ZERO,
            true,
        );
        return;
    }
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
        ServeViewState::Checking { .. } => render_pending(
            frame,
            area,
            theme,
            " Checking... ",
            "Looking for a running daemon",
        ),
        ServeViewState::Starting {
            mode, started_at, ..
        } => render_starting(
            frame,
            area,
            theme,
            &mut mouse,
            *mode,
            started_at.elapsed(),
            false,
        ),
        ServeViewState::Active { mode, urls, .. } => {
            render_active(view, frame, area, theme, &mut mouse);
            if view.show_help {
                // Any click closes the overlay, so nothing beneath is a target.
                mouse.keys.clear();
                mouse.copies.clear();
                let effect =
                    RestartEffect::of(*mode, urls, &view.restart_probe, view.running_persists);
                render_help_overlay(frame, area, theme, *mode, effect);
            }
        }
        ServeViewState::Error {
            message,
            mode,
            transport,
        } => render_error(
            frame,
            area,
            theme,
            &mut mouse,
            message,
            &error_recovery_keys(message, *mode, *transport),
        ),
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

/// The view's name, matching the footer's web entry. A build without the dashboard bundle
/// serves the API only.
fn view_title() -> &'static str {
    if cfg!(feature = "web") {
        "Web dashboard"
    } else {
        "Remote API access"
    }
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
    let title = format!(" {} ", view_title());
    let inner = render_page(frame, area, &title, theme.accent, theme.accent);
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

fn render_pending(frame: &mut Frame, area: Rect, theme: &Theme, title: &str, line: &str) {
    let inner = render_page(frame, area, title, theme.border, theme.title);
    frame.render_widget(
        Paragraph::new(Line::styled(line, Style::default().fg(theme.text)))
            .alignment(Alignment::Center),
        centered_rect(inner, inner.width, 1),
    );
}

fn render_starting(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    mouse: &mut ServeMouse,
    mode: ServeMode,
    elapsed: Duration,
    stopping: bool,
) {
    let (title, wait_line1, wait_line2) = if stopping {
        (
            " Stopping daemon... ",
            "Waiting for the owned launch to settle, then stop",
            "No reconnect or mutation will be replayed.",
        )
    } else {
        match mode {
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
        }
    };
    let inner = render_page(frame, area, title, theme.border, theme.title);
    let text = Style::default().fg(theme.text);
    let banner = vec![
        Line::from(""),
        Line::styled(wait_line1, text),
        Line::styled(wait_line2, text),
        Line::from(""),
        Line::styled(
            if stopping {
                "[Esc close]".to_owned()
            } else {
                format!("Elapsed: {}s    [Esc close]  [S stop]", elapsed.as_secs())
            },
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
            &[],
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
        // A pending confirm can wrap to three rows on a narrow terminal.
        Constraint::Length(if view.pending_confirm.is_some() { 4 } else { 3 }),
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
    let title = format!(" {} ({mode_label})", view_title());
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

    // Each row may carry `(label, drawn value width, value a click copies)`.
    type Copy = Option<(&'static str, u16, String)>;
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
    let width = |s: &str| s.chars().count() as u16;
    // The full URL is copied even when it wraps onto a token row.
    let shown_url = if url_fits { url.to_string() } else { base_url };
    rows.push((
        1,
        labeled("URL: ", shown_url.clone(), accent),
        Some(("URL: ", width(&shown_url), url.to_string())),
    ));
    if let Some(token) = token.filter(|_| !url_fits) {
        rows.push((
            1,
            labeled("Token: ", token.to_string(), accent),
            Some(("Token: ", width(token), token.to_string())),
        ));
    }
    if is_tunnel {
        let (value, style, copy) = match passphrase {
            Some(pp) => (
                pp.clone(),
                accent.bold(),
                Some(("Passphrase: ", width(pp), pp.clone())),
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
        if let Some((label, value_w, value)) = copy {
            let label_w = label.len() as u16;
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
    let footer_text = match view.pending_confirm.map(|(action, _)| action) {
        Some(action) => {
            let effect = RestartEffect::of(*mode, urls, &view.restart_probe, view.running_persists);
            let (first, effect_text) = match action {
                PendingConfirm::NewPassphrase => (
                    "Press G again for a new passphrase.",
                    "Every Device signs out.",
                ),
                PendingConfirm::Restart if effect.link == LinkChange::Replaced => {
                    ("Press R again to restart.", "Agents keep running.")
                }
                PendingConfirm::Restart => (
                    "Press R again to restart.",
                    match effect.logins {
                        Logins::Kept => "Devices stay signed in; agents keep running.",
                        Logins::Lost => "Every Device signs out; agents keep running.",
                        Logins::Unknown => {
                            "Devices may have to sign in again; agents keep running."
                        }
                    },
                ),
            };
            let link = match effect.link {
                LinkChange::Same => None,
                LinkChange::Replaced if effect.passphrase => {
                    Some("The link changes: re-scan and sign in again.")
                }
                LinkChange::Replaced | LinkChange::Moved => Some("The link changes: re-scan it."),
            };
            let rest = [effect_text]
                .into_iter()
                .chain(link)
                .chain(["Any other key cancels."]);
            let mut spans = vec![Span::styled(
                first,
                Style::default().fg(theme.waiting).bold(),
            )];
            spans.extend(rest.map(|s| Span::styled(format!(" {s}"), dimmed)));
            Text::from(Line::from(spans))
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
            Text::from(Line::from(
                keys.into_iter()
                    .flat_map(|(key, desc)| [Span::styled(key, accent), Span::styled(desc, dimmed)])
                    .collect::<Vec<_>>(),
            ))
        }
    };
    // A confirm may wrap onto the footer's second line.
    frame.render_widget(
        Paragraph::new(footer_text)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        footer_inner,
    );
    record_hints(frame, mouse, footer_inner);
}

/// The help row for R, matching the confirm's sentences.
fn restart_help(effect: RestartEffect) -> String {
    let logins = match effect.logins {
        Logins::Kept => "Devices stay signed in",
        Logins::Lost => "every Device signs out",
        Logins::Unknown => "Devices may have to sign in again",
    };
    let what = match effect.link {
        LinkChange::Replaced if effect.passphrase => "new link, sign in again".to_string(),
        LinkChange::Replaced => "new link".to_string(),
        LinkChange::Moved => format!("new link; {logins}"),
        LinkChange::Same => logins.to_string(),
    };
    format!("Restart the Daemon ({what}; agents keep running)")
}

fn render_help_overlay(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    mode: ServeMode,
    effect: RestartEffect,
) {
    let is_tunnel = mode == ServeMode::Tunnel;
    let width = 72.min(area.width.saturating_sub(4));
    let height = (if is_tunnel { 20 } else { 15 }).min(area.height.saturating_sub(4));
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
        .title(format!(" {} help ", view_title()))
        .title_style(heading);
    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let text = Style::default().fg(theme.text);
    let restart = restart_help(effect);
    let mut shortcuts = Vec::new();
    if is_tunnel {
        shortcuts.push(("G", "New passphrase (signs every Device out)"));
    }
    shortcuts.extend([
        ("R", restart.as_str()),
        ("S", "Stop the Daemon, return to mode picker"),
        ("Tab", "Cycle URLs (when multiple available)"),
        ("Esc / q", "Close this view (the Daemon keeps running)"),
        ("?", "Toggle this help"),
    ]);

    let mut lines = vec![
        Line::from(""),
        Line::styled("Keyboard Shortcuts", heading),
        Line::from(""),
    ];
    // A long description continues under itself, not under the key column.
    const KEY_COL: u16 = 16;
    for (key, desc) in shortcuts {
        let mut rows = Vec::new();
        crate::tui::markdown::wrap_line_into(
            Line::styled(desc.to_string(), text),
            inner.width.saturating_sub(KEY_COL),
            &mut rows,
        );
        for (i, row) in rows.into_iter().enumerate() {
            let lead = if i == 0 {
                format!("  {key:14}")
            } else {
                " ".repeat(KEY_COL as usize)
            };
            let mut spans = vec![Span::styled(lead, Style::default().fg(theme.waiting))];
            spans.extend(row.spans);
            lines.push(Line::from(spans));
        }
    }
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

/// The keys `error_key` handles for this failure, shown before the way out.
fn error_recovery_keys(
    message: &str,
    mode: ServeMode,
    transport: Option<TunnelTransport>,
) -> Vec<&'static str> {
    let mut keys = Vec::new();
    if mode == ServeMode::Tunnel {
        keys.push("[P] Pick provider");
    }
    if funnel_reset_applies(message, mode, transport) {
        keys.push("[R] Reset Funnel");
    }
    keys
}

fn render_error(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    mouse: &mut ServeMouse,
    message: &str,
    recovery: &[&str],
) {
    let inner = render_page(frame, area, " Serve failed ", theme.error, theme.error);
    let rest = ["[S] Force stop", "[Enter] Back to sessions"];
    const GAP: &str = "    ";
    let one_line = recovery
        .iter()
        .chain(&rest)
        .copied()
        .collect::<Vec<_>>()
        .join(GAP);
    // Too wide for one row, recovery keys and the way out take a row each.
    let rows: Vec<String> = if one_line.chars().count() <= inner.width.saturating_sub(2) as usize
        || recovery.is_empty()
    {
        vec![one_line]
    } else {
        vec![recovery.join(GAP), rest.join(GAP)]
    };
    let [body, keys] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(rows.len() as u16)])
            .margin(1)
            .areas(inner);
    frame.render_widget(
        Paragraph::new(message)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.text)),
        body,
    );
    let hint = Style::default().fg(theme.dimmed);
    for (i, row) in rows.iter().enumerate() {
        let line = Rect::new(keys.x, keys.y + i as u16, keys.width, 1);
        centered_line(frame, line, row, hint);
    }
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
            running_persists: Some(true),
            restart_probe: Default::default(),
            pending_launch: None,
            launch_in_flight: false,
            stop_requested: false,
            probes: super::super::Probes::new(|_| false),
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
        for needle in ["Remote API access", "No dashboard bundle in this build:"] {
            assert_eq!(
                screen.contains(needle),
                !cfg!(feature = "web"),
                "{needle:?}\n{screen}"
            );
        }
        assert_eq!(
            screen.contains("Web dashboard (local)"),
            cfg!(feature = "web"),
            "{screen}"
        );
    }

    fn view(state: ServeViewState) -> ServeView {
        ServeView {
            state,
            pending_passphrase: String::new(),
            pending_confirm: None,
            show_help: false,
            mouse: Default::default(),
            copied: None,
            running_persists: Some(true),
            restart_probe: Default::default(),
            pending_launch: None,
            launch_in_flight: false,
            stop_requested: false,
            probes: super::super::Probes::new(|_| false),
        }
    }

    fn draw(view: &ServeView) -> ratatui::buffer::Buffer {
        draw_at(view, 100)
    }

    fn draw_at(view: &ServeView, width: u16) -> ratatui::buffer::Buffer {
        crate::tui::dialogs::test_render::draw(width, 40, |f, _| {
            render(view, f, f.area(), &Theme::default())
        })
    }

    #[test]
    fn values_are_copy_targets_and_hints_click() {
        use crate::tui::dialogs::test_render::find;
        let url = "https://host.example.ts.net/?token=abcdefghijklmnopqrstuvwxyz0123456789";
        let token = split_url_and_token(url).1.unwrap();
        // At 60 columns the URL splits onto a Token row, yet its row still
        // copies the whole URL.
        let cases: [(u16, &[(&str, &str)]); 2] = [
            (100, &[("URL", url), ("Passphrase", "correct-horse")]),
            (
                60,
                &[
                    ("URL", url),
                    ("Token", token),
                    ("Passphrase", "correct-horse"),
                ],
            ),
        ];
        for (width, want) in cases {
            let view = view(ServeViewState::Active {
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
            let buf = draw_at(&view, width);
            let mouse = view.mouse.borrow();
            let got: Vec<(&str, &str)> = mouse
                .copies
                .iter()
                .map(|((label, value), r)| {
                    let drawn: String = (r.x..r.right()).map(|x| buf[(x, r.y)].symbol()).collect();
                    assert!(!drawn.is_empty() && value.starts_with(&drawn), "{label}");
                    (*label, value.as_str())
                })
                .collect();
            assert_eq!(got, want, "width {width}");
        }

        let mut view = view(ServeViewState::Active {
            mode: ServeMode::Local,
            transport: None,
            urls: vec![ServeUrl {
                label: None,
                url: url.to_string(),
            }],
            url_index: 0,
            passphrase: None,
            opened_at: Instant::now(),
            log_offset: 0,
        });
        let (x, y) = find(&draw(&view), "R: restart");
        assert!(view.handle_hover(x, y));
        assert_eq!(
            view.handle_click(x, y).map(|k| k.code),
            Some(KeyCode::Char('R'))
        );
        view.show_help = true;
        draw(&view);
        assert_eq!(view.handle_click(0, 0).map(|k| k.code), Some(KeyCode::Esc));
    }

    /// The text drawn in `rect`, rows joined and box borders blanked, so a wrapped sentence matches.
    fn text_in(buf: &ratatui::buffer::Buffer, rect: Rect) -> String {
        let rows: Vec<String> = (rect.y..rect.bottom())
            .map(|y| {
                (rect.x..rect.right())
                    .map(|x| buf[(x, y)].symbol())
                    .map(|sym| {
                        if "│─╭╮╰╯".contains(sym) {
                            " "
                        } else {
                            sym
                        }
                    })
                    .collect()
            })
            .collect();
        rows.join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn restart_and_passphrase_copy_say_what_happens_to_devices() {
        use super::super::RestartProbe;
        use PendingConfirm::{NewPassphrase as G, Restart as R};
        use ServeMode::{Local, Tunnel};
        let lan = "http://192.168.1.42:8080/?token=t";
        let funnel = "https://host.tailnet.ts.net/?token=t";
        let named = "https://aoe.example.com/?token=t";
        let quick = "https://a-b-c.trycloudflare.com/?token=t";
        let kept = "Devices stay signed in; agents keep running";
        let link = "The link changes: re-scan and sign in again.";
        let rescan = "The link changes: re-scan it.";
        let moved = (
            format!("Agents keep running. {link}"),
            "new link, sign in again; agents keep running".to_string(),
        );
        let stays = (format!("{kept}."), kept.to_string());
        let signs_out = (
            "Every Device signs out; agents keep running.".to_string(),
            "every Device signs out; agents keep running".to_string(),
        );
        let unknown = (
            "Devices may have to sign in again; agents keep running.".to_string(),
            "Devices may have to sign in again; agents keep running".to_string(),
        );
        let local_moved = (format!("{kept}. {rescan}"), format!("new link; {kept}"));
        let on = Some(true);
        // What the next start does: (takes the Funnel path, port it reuses (None = random),
        // still has the link's LAN address, inherits AOE_SERVE_PASSPHRASE, persist_sessions,
        // mints a new token).
        let probe =
            |on_funnel, port, same_host: bool, local_passphrase, persists, renews| RestartProbe {
                on_funnel,
                persists,
                port,
                local_hosts: vec![
                    if same_host {
                        "192.168.1.42"
                    } else {
                        "10.0.0.7"
                    }
                    .to_string(),
                    "localhost".to_string(),
                ],
                local_passphrase,
                local_token_renews: renews,
            };
        let tunnel = |on_funnel, persists| probe(on_funnel, None, true, false, persists, false);
        // (mode, url, next start, running daemon's persist_sessions (None = reattached), key
        //  pressed once, confirm after the first sentence, help R)
        let cases = [
            // Without a passphrase, Local signs in with the stable token in the link, so only
            // the link itself can change.
            (
                Local,
                lan,
                probe(false, Some(8080), true, false, false, false),
                None,
                R,
                stays.clone(),
            ),
            // A CLI-started Local daemon on another port, or no saved port, moves the link.
            (
                Local,
                lan,
                probe(false, Some(51234), true, false, false, false),
                None,
                R,
                local_moved.clone(),
            ),
            (
                Local,
                lan,
                probe(false, None, true, false, true, false),
                on,
                R,
                local_moved.clone(),
            ),
            // The interface lost its address: the old host's cookie is never sent again.
            (
                Local,
                lan,
                probe(false, Some(8080), false, false, true, false),
                on,
                R,
                (
                    format!("Agents keep running. {rescan}"),
                    "new link; agents keep running".to_string(),
                ),
            ),
            (
                Local,
                lan,
                probe(false, Some(8080), false, true, true, false),
                on,
                R,
                moved.clone(),
            ),
            // A Local daemon behind AOE_SERVE_PASSPHRASE has LAN Devices sign in, so the same
            // host and port keep them only when the sign-ins persist.
            (
                Local,
                lan,
                probe(false, Some(8080), true, true, false, false),
                on,
                R,
                signs_out.clone(),
            ),
            (
                Local,
                lan,
                probe(false, Some(8080), true, true, true, false),
                Some(false),
                R,
                signs_out.clone(),
            ),
            (
                Local,
                lan,
                probe(false, Some(8080), true, true, true, false),
                None,
                R,
                unknown.clone(),
            ),
            (
                Local,
                lan,
                probe(false, Some(8080), true, true, true, false),
                on,
                R,
                stays.clone(),
            ),
            // The cookie isn't scoped by port, so a new port loses only what persistence loses.
            (
                Local,
                lan,
                probe(false, None, true, true, false, false),
                on,
                R,
                (
                    format!("Every Device signs out; agents keep running. {rescan}"),
                    "new link; every Device signs out; agents keep running".to_string(),
                ),
            ),
            // A Local daemon never refreshes serve.token, so a restart a day on mints a new
            // one: a token-only Device's cookie stops matching, while a passphrase sign-in
            // still lets it in.
            (
                Local,
                lan,
                probe(false, Some(8080), true, false, true, true),
                on,
                R,
                (
                    format!("Agents keep running. {rescan}"),
                    "new link; agents keep running".to_string(),
                ),
            ),
            (
                Local,
                lan,
                probe(false, Some(8080), true, true, true, true),
                on,
                R,
                local_moved,
            ),
            (Tunnel, funnel, tunnel(true, true), on, R, stays.clone()),
            // Tailscale went away (or Cloudflare was picked): the restart opens a quick tunnel.
            (Tunnel, funnel, tunnel(false, true), on, R, moved.clone()),
            // spawn_daemon never re-creates a named tunnel, whether or not Funnel replaces it.
            (Tunnel, named, tunnel(false, true), on, R, moved.clone()),
            (Tunnel, named, tunnel(true, true), on, R, moved.clone()),
            (Tunnel, quick, tunnel(false, true), on, R, moved.clone()),
            (
                Tunnel,
                funnel,
                tunnel(true, false),
                on,
                R,
                signs_out.clone(),
            ),
            // Logins the running daemon held only in memory are gone, whatever the next start reads.
            (
                Tunnel,
                funnel,
                tunnel(true, true),
                Some(false),
                R,
                signs_out,
            ),
            // A reattached daemon's persistence is unknown.
            (Tunnel, funnel, tunnel(true, true), None, R, unknown),
            (
                Tunnel,
                funnel,
                tunnel(true, true),
                on,
                G,
                ("Every Device signs out.".to_string(), kept.to_string()),
            ),
            (
                Tunnel,
                quick,
                tunnel(false, true),
                on,
                G,
                (format!("Every Device signs out. {link}"), String::new()),
            ),
        ];
        let theme = crate::tui::styles::load_theme("empire");
        for (mode, url, probe, running, key, (effects, help_restart)) in cases {
            let case = format!("{mode:?} {url} {probe:?} running={running:?} {key:?}");
            let mut view = view(ServeViewState::Active {
                mode,
                transport: None,
                urls: vec![ServeUrl {
                    label: None,
                    url: url.to_string(),
                }],
                url_index: 0,
                passphrase: (mode == Tunnel).then(|| "correct-horse".to_string()),
                opened_at: Instant::now(),
                log_offset: 0,
            });
            view.restart_probe = probe;
            view.running_persists = running;
            view.pending_confirm = Some((key, Instant::now()));
            let first = match key {
                G => "Press G again for a new passphrase.",
                R => "Press R again to restart.",
            };
            // 60 columns wraps the longest confirm onto a third footer row.
            for width in [60, 80, 120] {
                let buf = crate::tui::dialogs::test_render::draw(width, 24, |f, theme| {
                    render(&view, f, f.area(), theme)
                });
                let footer = text_in(&buf, Rect::new(0, 21, width, 3));
                assert_eq!(
                    footer,
                    format!("{first} {effects} Any other key cancels."),
                    "{case} at {width}"
                );
                let (x, y) = crate::tui::dialogs::test_render::find(&buf, first);
                assert_eq!(buf[(x, y)].fg, theme.waiting, "{case}");
                let (x, y) = crate::tui::dialogs::test_render::find(&buf, "cancels.");
                assert_eq!(buf[(x, y)].fg, theme.dimmed, "{case}");
            }

            if help_restart.is_empty() {
                continue;
            }
            view.pending_confirm = None;
            view.show_help = true;
            let buf = crate::tui::dialogs::test_render::draw(80, 24, |f, theme| {
                render(&view, f, f.area(), theme)
            });
            let help = text_in(
                &buf,
                centered_rect(
                    Rect::new(0, 0, 80, 24),
                    72,
                    if mode == Tunnel { 20 } else { 15 },
                ),
            );
            for want in [
                format!("R Restart the Daemon ({help_restart})"),
                "Esc / q Close this view (the Daemon keeps running)".to_string(),
                "Press any key to close".to_string(),
            ] {
                assert!(help.contains(&want), "{case}: {want:?}\n{help}");
            }
            // A wrapped description continues under its own column, not under the key.
            let (desc_x, desc_y) =
                crate::tui::dialogs::test_render::find(&buf, "Restart the Daemon");
            let cell = |x: u16| buf[(x, desc_y + 1)].symbol().trim().to_string();
            let wrapped =
                crate::tui::dialogs::test_render::find(&buf, "Stop the Daemon").1 > desc_y + 1;
            assert!(
                !wrapped || (cell(desc_x - 1).is_empty() && !cell(desc_x).is_empty()),
                "{case}\n{help}"
            );
            assert_eq!(
                help.contains("G New passphrase (signs every Device out)"),
                mode == Tunnel,
                "{case}\n{help}"
            );
        }
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

    #[test]
    fn the_error_footer_offers_recovery_keys_first_and_only_those_error_key_handles() {
        use ServeMode::{Local, Tunnel};
        use TunnelTransport::{Cloudflare, Tailscale};
        let (p, r) = ("[P] Pick provider", "[R] Reset Funnel");
        let rest = "[S] Force stop    [Enter] Back to sessions";
        let funnel_msg = "Tailscale Funnel setup failed";
        // (mode, transport, message, width, footer rows)
        let cases = [
            (
                Tunnel,
                Some(Tailscale),
                "timed out",
                120,
                vec![format!("{p}    {r}    {rest}")],
            ),
            // Too wide at 80 columns: the recovery keys and the way out take a row each.
            (
                Tunnel,
                Some(Tailscale),
                "timed out",
                80,
                vec![format!("{p}    {r}"), rest.to_string()],
            ),
            (
                Tunnel,
                Some(Tailscale),
                "timed out",
                60,
                vec![format!("{p}    {r}"), rest.to_string()],
            ),
            // At 60 columns the three hints no longer fit on one row.
            (
                Tunnel,
                Some(Cloudflare),
                funnel_msg,
                60,
                vec![p.to_string(), rest.to_string()],
            ),
            // A known Cloudflare failure gets no Funnel reset, whatever the message says.
            (
                Tunnel,
                Some(Cloudflare),
                funnel_msg,
                80,
                vec![format!("{p}    {rest}")],
            ),
            // A reattached daemon's transport is unknown, so the message decides.
            (
                Tunnel,
                None,
                funnel_msg,
                120,
                vec![format!("{p}    {r}    {rest}")],
            ),
            (Local, None, funnel_msg, 120, vec![rest.to_string()]),
        ];
        for (mode, transport, message, width, want) in cases {
            let case = format!("{mode:?} {transport:?} {message:?} at {width}");
            let mut view = view(ServeViewState::Error {
                message: message.to_string(),
                mode,
                transport,
            });
            let buf = draw_at(&view, width);
            let rows: Vec<String> = (0..buf.area.height)
                .map(|y| {
                    let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                    row.trim_matches(|c: char| c == '│' || c.is_whitespace())
                        .to_string()
                })
                .filter(|row| row.contains("[S] ") || row.contains("[P] ") || row.contains("[R] "))
                .collect();
            assert_eq!(rows, want, "{case}");
            // Every hint stays whole and clickable, on either row.
            let mut hints = vec![("[S]", KeyCode::Char('S')), ("[Enter]", KeyCode::Enter)];
            if mode == Tunnel {
                hints.push(("[P]", KeyCode::Char('P')));
            }
            for (label, code) in hints {
                let (x, y) = crate::tui::dialogs::test_render::find(&buf, label);
                assert_eq!(
                    view.handle_click(x, y).map(|k| k.code),
                    Some(code),
                    "{case}: {label}"
                );
            }
        }
    }
}

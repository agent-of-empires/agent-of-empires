//! The screens shown before a mode is live: the exposure picker, the restart
//! confirmation, and the progress view while the daemon is replaced.

use std::time::Duration;

use ratatui::prelude::*;
use ratatui::widgets::*;

use super::{
    exposure_label, split_url_and_token, truncate_to_width, Exposure, TransportStatus,
    TunnelTransport,
};
use crate::tui::styles::Theme;

pub(super) struct PickerModel<'a> {
    pub(super) selected: Exposure,
    pub(super) current: Option<Exposure>,
    pub(super) local_url: Option<&'a str>,
    pub(super) tunnel_available: bool,
    pub(super) network_address: Option<&'a str>,
    pub(super) flash: Option<&'a str>,
}

pub(super) fn render_picker(frame: &mut Frame, area: Rect, theme: &Theme, model: PickerModel) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Line::styled(
            " Remote Access ",
            Style::default().fg(theme.accent).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // status(4) + spacer + question + spacer + options(3 x 3) + flash + keys
    let content_height: u16 = 18;
    let max_width: u16 = 72;
    let width = max_width.min(inner.width.saturating_sub(2));
    let body = Rect {
        x: inner.x + inner.width.saturating_sub(width) / 2,
        y: inner.y + inner.height.saturating_sub(content_height) / 2,
        width,
        height: content_height.min(inner.height),
    };

    let dimmed = Style::default().fg(theme.dimmed);
    let text = Style::default().fg(theme.text);
    let (status, status_style) = match model.current {
        Some(exposure) => (exposure_label(exposure), Style::default().fg(theme.running)),
        None => (
            "Not reachable (reconnecting)",
            Style::default().fg(theme.error),
        ),
    };
    let heading = if model.current.is_some() {
        "aoe is running on this machine."
    } else {
        "aoe's daemon is not answering on this machine."
    };
    let mut lines = vec![
        Line::from(Span::styled(
            heading,
            Style::default().fg(theme.title).bold(),
        )),
        Line::from(vec![
            Span::styled("Exposure: ", dimmed),
            Span::styled(status, status_style.bold()),
        ]),
    ];
    // The token is what makes the URL usable, so split it rather than truncate.
    let accent = Style::default().fg(theme.accent);
    match model.local_url {
        Some(url) if url.chars().count() > width as usize => {
            let (base, token) = split_url_and_token(url);
            lines.push(Line::from(Span::styled(base, accent)));
            lines.push(Line::from(vec![
                Span::styled("token ", dimmed),
                Span::styled(token.unwrap_or_default().to_string(), accent),
            ]));
        }
        Some(url) => lines.extend([Line::from(Span::styled(url, accent)), Line::from("")]),
        None => lines.extend([Line::from(""), Line::from("")]),
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "How should it be reachable?",
            Style::default().fg(theme.title).bold(),
        )),
        Line::from(""),
    ]);

    let network = model
        .network_address
        .map(|address| format!("{address}. Token auth, plain HTTP."))
        .unwrap_or_else(|| "No non-loopback interface available.".to_string());
    let tunnel = if model.tunnel_available {
        "Tailscale or Cloudflare. Token + passphrase."
    } else {
        "Install tailscale or cloudflared to enable."
    };
    let options = [
        (
            Exposure::Localhost,
            "This machine only. Token auth.".to_string(),
            true,
        ),
        (Exposure::Network, network, model.network_address.is_some()),
        (Exposure::Tunnel, tunnel.to_string(), model.tunnel_available),
    ];
    for (number, (exposure, description, available)) in options.into_iter().enumerate() {
        let selected = exposure == model.selected;
        let label_style = match (selected, available) {
            (true, true) => Style::default().fg(theme.accent).bold(),
            (_, false) => dimmed,
            (false, true) => text.bold(),
        };
        let mut label = vec![
            Span::styled(if selected { "\u{25B8} " } else { "  " }, label_style),
            Span::styled(format!("{} ", number + 1), dimmed),
            Span::styled(exposure_label(exposure), label_style),
        ];
        if model.current == Some(exposure) {
            label.push(Span::styled(
                "  current",
                Style::default().fg(theme.running),
            ));
        }
        lines.push(Line::from(label));
        lines.push(Line::from(Span::styled(
            truncate_to_width(&format!("    {description}"), width as usize),
            if available { text } else { dimmed },
        )));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        model.flash.unwrap_or(""),
        Style::default().fg(theme.waiting).bold(),
    )));
    lines.push(Line::from(Span::styled(
        "[\u{2191}/\u{2193}] choose  [1-3] pick  [Enter] apply  [r] restart daemon  [Esc] close",
        dimmed,
    )));
    frame.render_widget(Paragraph::new(lines), body);
}

pub(super) fn render_applying(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    target: Exposure,
    elapsed: Duration,
) {
    frame.render_widget(Clear, area);
    let (title, wait_line1, wait_line2) = match target {
        Exposure::Tunnel => (
            " Starting HTTPS tunnel... ",
            "Restarting the daemon behind a tunnel",
            "(first-time Tailscale cert provisioning can take 30\u{2013}60s).",
        ),
        Exposure::Network => (
            " Exposing on the local network... ",
            "Restarting the daemon on 0.0.0.0",
            "(usually a few seconds).",
        ),
        Exposure::Localhost => (
            " Returning to localhost... ",
            "Restarting the daemon on 127.0.0.1",
            "(usually a few seconds).",
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(Line::styled(title, Style::default().fg(theme.title).bold()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content_height: u16 = 5;
    let v_pad = inner.height.saturating_sub(content_height) / 2;
    let centered = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(v_pad),
            Constraint::Length(content_height),
            Constraint::Min(0),
        ])
        .split(inner);

    let banner = vec![
        Line::from(""),
        Line::from(Span::styled(wait_line1, Style::default().fg(theme.text))),
        Line::from(Span::styled(wait_line2, Style::default().fg(theme.text))),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "Elapsed: {}s    [Esc] close (the change continues)",
                elapsed.as_secs()
            ),
            Style::default().fg(theme.dimmed),
        )),
    ];
    frame.render_widget(
        Paragraph::new(banner).alignment(Alignment::Center),
        centered[1],
    );
}

pub(super) fn render_confirm(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    selected: TunnelTransport,
    tailscale: TransportStatus,
    cloudflare: TransportStatus,
    flash: Option<&str>,
) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Line::styled(
            " Expose to Internet? ",
            Style::default().fg(theme.accent).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Center content vertically and constrain width
    let content_height: u16 = 19; // risk(6) + picker(1) + cards(8) + flash + keybinds + margins
    let v_pad = inner.height.saturating_sub(content_height) / 2;
    let centered = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(v_pad),
            Constraint::Length(content_height),
            Constraint::Min(0),
        ])
        .split(inner);

    let max_w: u16 = 82;
    let h_pad = centered[1].width.saturating_sub(max_w) / 2;
    let h_centered = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(h_pad),
            Constraint::Length(max_w.min(centered[1].width)),
            Constraint::Min(0),
        ])
        .split(centered[1]);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // risk explanation
            Constraint::Length(1), // "Pick a transport:"
            Constraint::Min(8),    // cards
            Constraint::Length(1), // flash
            Constraint::Length(1), // keybinds
        ])
        .split(h_centered[1]);

    // ── Risk explanation (compressed; picker below carries most of UI) ───
    let risk = vec![
        Line::from(Span::styled(
            "Your sessions become reachable from anywhere over HTTPS.",
            Style::default().fg(theme.text),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Two factors required to log in:",
            Style::default().fg(theme.title).bold(),
        )),
        Line::from(vec![
            Span::styled("  \u{2022} ", Style::default().fg(theme.running)),
            Span::styled(
                "token (in the URL / QR code)",
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("  \u{2022} ", Style::default().fg(theme.running)),
            Span::styled(
                "passphrase (typed on the login page)",
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(Span::styled(
            "Don't share screenshots with BOTH. Press [E] for localhost when done.",
            Style::default().fg(theme.dimmed),
        )),
    ];
    frame.render_widget(Paragraph::new(risk), rows[0]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Pick a transport:",
            Style::default().fg(theme.title).bold(),
        ))),
        rows[1],
    );

    // ── Transport cards ──────────────────────────────────────────────────
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Length(1),
            Constraint::Percentage(50),
        ])
        .split(rows[2]);

    render_transport_card(
        frame,
        cards[0],
        theme,
        "Tailscale Funnel",
        &[
            "Stable URL across restarts",
            "PWA-friendly on phones",
            "https://<host>.<tailnet>.ts.net",
        ],
        tailscale,
        selected == TunnelTransport::Tailscale,
        /*is_tailscale=*/ true,
    );
    render_transport_card(
        frame,
        cards[2],
        theme,
        "Cloudflare Tunnel",
        &[
            "Works anywhere",
            "URL rotates each restart",
            "Not PWA-friendly",
        ],
        cloudflare,
        selected == TunnelTransport::Cloudflare,
        /*is_tailscale=*/ false,
    );

    // ── Flash ────────────────────────────────────────────────────────────
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            flash.unwrap_or(""),
            Style::default().fg(theme.error).bold(),
        )))
        .alignment(Alignment::Center),
        rows[3],
    );

    // ── Keybinds ─────────────────────────────────────────────────────────
    let keybinds =
        "[←/→] select  [T] Tailscale  [C] Cloudflare  [R] refresh  [Enter] confirm  [Esc] cancel";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            keybinds,
            Style::default().fg(theme.dimmed),
        )))
        .alignment(Alignment::Center),
        rows[4],
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_transport_card(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    body_lines: &[&str],
    status: TransportStatus,
    is_selected: bool,
    is_tailscale: bool,
) {
    let ready = status.is_ready();
    let (border, title_color, body_color) = if is_selected && ready {
        (theme.accent, theme.accent, theme.text)
    } else if !ready {
        (theme.dimmed, theme.dimmed, theme.dimmed)
    } else {
        (theme.border, theme.title, theme.text)
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

    let mut lines = vec![Line::from("")];
    for text in body_lines {
        lines.push(Line::from(Span::styled(
            *text,
            Style::default().fg(body_color),
        )));
    }
    lines.push(Line::from(""));

    let (status_icon, status_text, status_style) = match status {
        TransportStatus::Ready => (
            "\u{2713}",
            "Ready".to_string(),
            Style::default().fg(theme.running).bold(),
        ),
        TransportStatus::NotInstalled => (
            "\u{26A0}",
            if is_tailscale {
                "Not installed (tailscale up)".to_string()
            } else {
                "Not installed (brew install cloudflared)".to_string()
            },
            Style::default().fg(theme.dimmed),
        ),
        TransportStatus::FunnelNotEnabled => (
            "\u{26A0}",
            "Funnel not enabled for this node".to_string(),
            Style::default().fg(theme.error).bold(),
        ),
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{status_icon} "), status_style),
        Span::styled(status_text, status_style),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

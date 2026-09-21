//! Compact system-health strip and its read-only preview view.
//!
//! The readout row sheds gracefully as width shrinks: the used/total detail
//! drops before the counts, and below that only the percent remains.

use ratatui::prelude::*;
use ratatui::widgets::*;
use unicode_width::UnicodeWidthStr;

use crate::process::metrics::{
    pressure_band, AgentMetric, MemorySample, MetricsSnapshot, PressureBand,
};
use crate::tui::components::truncate_to_width;
use crate::tui::styles::Theme;

const HEALTH_FIXED_ROWS: u16 = 6;
const AGENT_TABLE_HEADER_ROWS: u16 = 1;
/// Display width of the fixed metric block on the agent table, identical on the
/// header and on each row. The name column takes what is left, so both lines
/// must derive it the same way or the columns drift apart.
const AGENT_METRICS_WIDTH: usize = 24;

pub(crate) fn agent_table_visible_rows(preview_height: u16) -> usize {
    preview_height
        .saturating_sub(2)
        .saturating_sub(HEALTH_FIXED_ROWS)
        .saturating_sub(AGENT_TABLE_HEADER_ROWS) as usize
}

/// Gutter between the pane border and its contents, widening with the pane: the
/// name column stretches, so a wide pane otherwise reads as pinned to its edges,
/// while a narrow one needs the width more than the gutter. `width` is the outer
/// pane width, borders included.
fn health_padding(width: u16) -> u16 {
    match width {
        0..=47 => 1,
        48..=79 => 2,
        _ => 3,
    }
}

fn agent_name_width(table_width: u16) -> usize {
    (table_width as usize)
        .saturating_sub(AGENT_METRICS_WIDTH)
        .max(8)
}

/// Marker on a sandboxed row, matching the session list's `[container]` badge.
const CONTAINER_BADGE: &str = " [container]";

/// The name cell: the agent title, plus the container marker when the figures
/// come from a sandbox rather than a host process tree. The spans always total
/// `width`, so the metric columns stay under their headers.
fn agent_name_spans<'a>(agent: &AgentMetric, width: usize, theme: &Theme) -> Vec<Span<'a>> {
    // Below this the badge would crowd out the name; the row keeps its
    // numbers and just loses the marker.
    let show_badge = agent.sandboxed && width >= CONTAINER_BADGE.width() + 8;
    let title_width = if show_badge {
        width - CONTAINER_BADGE.width()
    } else {
        width
    };
    let mut spans = vec![Span::styled(
        format_agent_title(&agent.title, title_width),
        Style::default().fg(theme.text),
    )];
    if show_badge {
        spans.push(Span::styled(
            CONTAINER_BADGE,
            Style::default().fg(theme.sandbox),
        ));
    }
    spans
}

/// The fixed-width CPU / Mem / Procs block of an agent row. A figure the
/// sampler has no reading for prints "?" rather than a zero, which would read
/// as a measured idle.
fn agent_metrics_cell(agent: &AgentMetric) -> String {
    let cpu = agent
        .cpu_fraction
        .map_or_else(|| "?".to_string(), |v| format!("{:.1}%", v * 100.0));
    let mem = agent
        .rss_bytes
        .map_or_else(|| "?".to_string(), format_bytes);
    let procs = agent
        .procs
        .map_or_else(|| "?".to_string(), |procs| procs.to_string());
    format!(" {cpu:>6} {mem:>9} {procs:>6}")
}

fn format_agent_title(title: &str, width: usize) -> String {
    let title = truncate_to_width(title, width);
    let padding = width.saturating_sub(super::text::rendered_width(&title));
    format!("{title}{}", " ".repeat(padding))
}

fn band_color(theme: &Theme, band: PressureBand) -> Color {
    match band {
        PressureBand::Ok => theme.running,
        PressureBand::Warn => theme.waiting,
        PressureBand::Critical => theme.error,
    }
}

/// Compact binary-unit byte string: `22.7G`, `9.9G`, `512M`, `1K`, `512B`.
/// GiB keeps one decimal but drops a whole `.0`; smaller units round to a whole
/// number. Integer math throughout so the rounding is exact.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    if bytes >= GIB {
        // Tenths of a GiB, rounded, so 32 GiB reads "32G" and 22.7 GiB "22.7G".
        let tenths = (bytes as u128 * 10 + GIB as u128 / 2) / GIB as u128;
        if tenths % 10 == 0 {
            format!("{}G", tenths / 10)
        } else {
            format!("{}.{}G", tenths / 10, tenths % 10)
        }
    } else if bytes >= MIB {
        format!("{}M", (bytes + MIB / 2) / MIB)
    } else if bytes >= KIB {
        format!("{}K", (bytes + KIB / 2) / KIB)
    } else {
        format!("{}B", bytes)
    }
}

/// Render the single-row current-state strip into `area`.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    snapshot: &MetricsSnapshot,
    hovered: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mem = &snapshot.memory;
    let band = pressure_band(mem);
    let color = band_color(theme, band);

    let style = if hovered {
        Style::default().bg(theme.selection)
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(readout_line(
            theme,
            color,
            mem,
            snapshot,
            area.width as usize,
            hovered,
        ))
        .style(style),
        area,
    );
}

/// Build the compact CPU and memory readout, adding agent counts when width
/// permits. A chevron makes the drill-down action visible without consuming a
/// permanent keybinding.
fn readout_line<'a>(
    theme: &Theme,
    color: Color,
    mem: &MemorySample,
    snapshot: &MetricsSnapshot,
    avail: usize,
    hovered: bool,
) -> Line<'a> {
    let cpu = snapshot
        .system
        .cpu_fraction
        .map(|value| format!("CPU {}%", (value * 100.0).round() as u32))
        .unwrap_or_else(|| "CPU ?".to_string());
    let memory = if mem.total_bytes > 0 {
        format!("Mem {}%", (mem.used_fraction() * 100.0).round() as u32)
    } else {
        "Mem ?".to_string()
    };
    let counts = format!(
        "{} agents · {} procs",
        snapshot.counts.agents, snapshot.counts.procs
    );
    let separator = " · ";
    let detail_separator = "  │  ";
    let affordance = "  ›";
    let primary_width = 1 + cpu.width() + separator.width() + memory.width();
    let full_width = primary_width + detail_separator.width() + counts.width() + affordance.width();
    let affordance_color = if hovered { theme.title } else { theme.hint };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(cpu, Style::default().fg(theme.text).bold()),
        Span::styled(separator, Style::default().fg(theme.dimmed)),
        Span::styled(memory, Style::default().fg(color).bold()),
    ];
    if full_width <= avail {
        spans.extend([
            Span::styled(detail_separator, Style::default().fg(theme.dimmed)),
            Span::styled(counts, Style::default().fg(theme.dimmed)),
        ]);
    }
    if primary_width + affordance.width() <= avail {
        spans.push(Span::styled(
            affordance,
            Style::default().fg(affordance_color).bold(),
        ));
    }
    Line::from(spans)
}

/// Read-only system-health detail rendered in the ordinary preview pane.
pub fn render_system_health(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    snapshot: &MetricsSnapshot,
    scroll: usize,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " System Health ",
            Style::default().fg(theme.title).bold(),
        ))
        .padding(Padding::horizontal(health_padding(area.width)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let cpu = snapshot
        .system
        .cpu_fraction
        .map(|v| format!("{:>3}%", (v * 100.0).round() as u32))
        .unwrap_or_else(|| "  ?%".into());
    let memory = if snapshot.memory.total_bytes > 0 {
        format!(
            "{:>3}%",
            (snapshot.memory.used_fraction() * 100.0).round() as u32
        )
    } else {
        "  ?%".into()
    };
    let load = snapshot
        .system
        .load_average
        .map(|v| format!("{:.2} / {:.2} / {:.2}", v[0], v[1], v[2]))
        .unwrap_or_else(|| "? / ? / ?".into());
    let swap = if snapshot.system.swap_total_bytes > 0 {
        format!(
            "{} / {}",
            format_bytes(snapshot.system.swap_used_bytes),
            format_bytes(snapshot.system.swap_total_bytes)
        )
    } else {
        "none".into()
    };

    let rows = Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).split(inner);
    let band = pressure_band(&snapshot.memory);
    let severity = band.as_str().to_ascii_uppercase();
    let lines = vec![
        Line::from(vec![
            Span::styled("Status  ", Style::default().fg(theme.dimmed)),
            Span::styled(
                severity,
                Style::default().fg(band_color(theme, band)).bold(),
            ),
        ]),
        Line::from(vec![
            Span::styled("CPU     ", Style::default().fg(theme.dimmed)),
            Span::styled(cpu, Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            Span::styled("Memory  ", Style::default().fg(theme.dimmed)),
            Span::styled(memory, Style::default().fg(theme.text)),
            Span::raw(format!(
                "   {} / {}",
                format_bytes(snapshot.memory.used_bytes()),
                format_bytes(snapshot.memory.total_bytes)
            )),
        ]),
        Line::from(vec![
            Span::styled("Load    ", Style::default().fg(theme.dimmed)),
            Span::raw(load),
        ]),
        Line::from(vec![
            Span::styled("Swap    ", Style::default().fg(theme.dimmed)),
            Span::raw(swap),
        ]),
        Line::from(vec![
            Span::styled("Agents  ", Style::default().fg(theme.dimmed)),
            Span::raw(format!(
                "{} running · {} processes",
                snapshot.counts.agents, snapshot.counts.procs
            )),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), rows[0]);

    let table_area = rows[1];
    if table_area.height == 0 {
        return;
    }
    if snapshot.agents.is_empty() {
        frame.render_widget(
            Paragraph::new("No running AoE agents").style(Style::default().fg(theme.dimmed)),
            table_area,
        );
        return;
    }
    let name_width = agent_name_width(table_area.width);
    let mut table_lines = vec![Line::from(vec![
        Span::styled(
            format!("{:<name_width$}", "Agent"),
            Style::default().fg(theme.hint).bold(),
        ),
        Span::styled(
            // "Mem", not "RSS": a sandboxed row reports the container's memory
            // usage, which is not a resident-set sum.
            format!("{:>7} {:>9} {:>6}", "CPU", "Mem", "Procs"),
            Style::default().fg(theme.hint).bold(),
        ),
    ])];
    let visible = table_area.height.saturating_sub(1) as usize;
    for agent in snapshot.agents.iter().skip(scroll).take(visible) {
        let mut spans = agent_name_spans(agent, name_width, theme);
        spans.push(Span::raw(agent_metrics_cell(agent)));
        table_lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(table_lines), table_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_table_visible_rows_handles_boundaries() {
        let row_cases = [(9, 0), (10, 1), (16, 7)];
        for (height, expected) in row_cases {
            assert_eq!(agent_table_visible_rows(height), expected);
        }
    }

    #[test]
    fn agent_table_title_padding_keeps_metrics_aligned() {
        use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

        for title in ["agent", "日本語", "aaaaaaaé", "ｶﾞｷﾞｸﾞｹﾞ"] {
            let agent = AgentMetric {
                title: title.into(),
                ..AgentMetric::default()
            };
            let mut spans = agent_name_spans(&agent, 8, &Theme::default());
            spans.push(Span::raw("X"));
            let area = Rect::new(0, 0, 24, 1);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(Line::from(spans)).render(area, &mut buffer);
            let metric_column = (0..24).find(|x| buffer[(*x, 0)].symbol() == "X");
            assert_eq!(metric_column, Some(8), "title {title:?}");
        }
    }

    #[test]
    fn agent_table_header_and_rows_share_column_offsets() {
        // The metric block is fixed-width on both lines, so the name cell must
        // be the same width on both or the CPU/RSS/Procs columns drift apart.
        assert_eq!(
            format!("{:>7} {:>9} {:>6}", "CPU", "RSS", "Procs").width(),
            AGENT_METRICS_WIDTH
        );
        assert_eq!(
            agent_metrics_cell(&AgentMetric {
                cpu_fraction: Some(0.999),
                rss_bytes: Some(22 * (1 << 30)),
                procs: Some(12),
                ..AgentMetric::default()
            })
            .width(),
            AGENT_METRICS_WIDTH
        );
        // A sandbox with no runtime sample yet still fills its columns.
        assert_eq!(
            agent_metrics_cell(&AgentMetric {
                sandboxed: true,
                ..AgentMetric::default()
            }),
            format!("{:>7} {:>9} {:>6}", "?", "?", "?")
        );
        let theme = Theme::default();
        for table_width in [20u16, 32, 48, 60, 120] {
            let name_width = agent_name_width(table_width);
            let header = format!("{:<name_width$}", "Agent").width();
            for sandboxed in [false, true] {
                let agent = AgentMetric {
                    title: "some agent title".into(),
                    sandboxed,
                    ..AgentMetric::default()
                };
                let cell: usize = agent_name_spans(&agent, name_width, &theme)
                    .iter()
                    .map(|s| s.width())
                    .sum();
                assert_eq!(header, cell, "table width {table_width}, {sandboxed}");
            }
        }
    }

    #[test]
    fn health_padding_never_starves_the_agent_table() {
        // The gutter must not cost so much width that the metrics block and a
        // minimum name cell stop fitting, which is what would make a wider
        // pane render a worse table than a narrower one.
        let mut previous = 0;
        for width in 36u16..=200 {
            let padding = health_padding(width);
            assert!(padding >= previous, "padding shrank at width {width}");
            previous = padding;
            let inner = width - 2 - 2 * padding;
            assert!(
                inner as usize >= AGENT_METRICS_WIDTH + 8,
                "width {width} leaves only {inner} columns for the table"
            );
        }
    }

    #[test]
    fn container_badge_is_dropped_when_the_name_cell_is_narrow() {
        let theme = Theme::default();
        let agent = AgentMetric {
            title: "sandboxed agent".into(),
            sandboxed: true,
            ..AgentMetric::default()
        };
        let badged = |width| {
            agent_name_spans(&agent, width, &theme)
                .iter()
                .any(|s| s.content.contains("[container]"))
        };
        assert!(!badged(CONTAINER_BADGE.width() + 7));
        assert!(badged(CONTAINER_BADGE.width() + 8));
    }

    #[test]
    fn format_bytes_table() {
        let gib = 1u64 << 30;
        let cases = [
            (0u64, "0B"),
            (512, "512B"),
            (1024, "1K"),
            (536_870_912, "512M"), // 512 MiB
            (32 * gib, "32G"),     // whole GiB drops the .0
            (22 * gib + 7 * gib / 10, "22.7G"),
            (9 * gib + 9 * gib / 10, "9.9G"),
        ];
        for (bytes, expected) in cases {
            assert_eq!(format_bytes(bytes), expected, "bytes {bytes}");
        }
    }
}

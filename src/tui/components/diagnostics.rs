//! Compact system-health strip and its read-only preview view.
//!
//! The readout row sheds gracefully as width shrinks: the used/total detail
//! drops before the counts, and below that only the percent remains.

use ratatui::prelude::*;
use ratatui::widgets::*;
use unicode_width::UnicodeWidthStr;

use crate::process::metrics::{AgentMetric, MemorySample, MetricsSnapshot};
use crate::tui::components::truncate_to_width;
use crate::tui::styles::Theme;

/// Fraction used at or above which the strip reads Critical.
const HEADROOM_CRITICAL: f64 = 0.90;
/// Fraction used at or above which the strip reads Warn.
const HEADROOM_WARN: f64 = 0.70;
/// PSI `some` avg10 percent at or above which a present PSI signal reads Critical.
const PSI_CRITICAL: f32 = 20.0;
/// PSI `some` avg10 percent at or above which a present PSI signal reads Warn.
const PSI_WARN: f32 = 5.0;
const HEALTH_FIXED_ROWS: u16 = 6;
const AGENT_TABLE_HEADER_ROWS: u16 = 1;
/// Display width of the fixed metric block on the agent table, identical on the
/// header (`{:>7} {:>9} {:>6}`) and on each row (` {cpu:>6} {:>9} {:>6}`). The
/// name column takes whatever is left, so both lines must derive it the same
/// way or the columns drift apart.
const AGENT_METRICS_WIDTH: usize = 24;

pub(crate) fn agent_table_visible_rows(preview_height: u16) -> usize {
    preview_height
        .saturating_sub(2)
        .saturating_sub(HEALTH_FIXED_ROWS)
        .saturating_sub(AGENT_TABLE_HEADER_ROWS) as usize
}

/// Gutter between the pane border and its contents, widening as the pane does.
/// The agent table stretches its name column to whatever it is given, so at one
/// column of padding a wide pane reads as pinned to its edges; a narrow one
/// needs the width for the columns more than it needs the gutter. `width` is
/// the outer pane width, borders included.
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

/// The name cell: the agent title, plus the container marker when the row's
/// figures come from a sandbox rather than a host process tree (its memory
/// figure is the container's usage, not an RSS sum). The spans always total
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
    let padding = width.saturating_sub(title.width());
    format!("{title}{}", " ".repeat(padding))
}

/// Memory-pressure severity, worst-of across the available signals. Ordered
/// ascending so the derived `Ord` lets callers fold inputs with `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PressureBand {
    Ok,
    Warn,
    Critical,
}

/// Classify a memory sample into a pressure band. Headroom is always an input;
/// PSI (Linux) and the macOS pressure level contribute only when present, so a
/// platform that omits a signal never has it read as a false all-clear. The
/// worst band across the present inputs wins.
pub(crate) fn pressure_band(mem: &MemorySample) -> PressureBand {
    let mut band = band_from_headroom(mem.used_fraction());
    if let Some(v) = mem.psi_mem_some_avg10 {
        band = band.max(band_from_psi(v));
    }
    if let Some(v) = mem.psi_io_some_avg10 {
        band = band.max(band_from_psi(v));
    }
    if let Some(level) = mem.macos_pressure_level {
        band = band.max(band_from_macos(level));
    }
    band
}

fn band_from_headroom(used_fraction: f64) -> PressureBand {
    if used_fraction >= HEADROOM_CRITICAL {
        PressureBand::Critical
    } else if used_fraction >= HEADROOM_WARN {
        PressureBand::Warn
    } else {
        PressureBand::Ok
    }
}

fn band_from_psi(some_avg10: f32) -> PressureBand {
    if some_avg10 >= PSI_CRITICAL {
        PressureBand::Critical
    } else if some_avg10 >= PSI_WARN {
        PressureBand::Warn
    } else {
        PressureBand::Ok
    }
}

fn band_from_macos(level: u8) -> PressureBand {
    match level {
        4 => PressureBand::Critical,
        2 => PressureBand::Warn,
        _ => PressureBand::Ok,
    }
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
    let severity = match band {
        PressureBand::Ok => "OK",
        PressureBand::Warn => "WARN",
        PressureBand::Critical => "CRITICAL",
    };
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

    fn sample_with_fraction(used_fraction: f64) -> MemorySample {
        // total 1000 so available maps cleanly to the target fraction.
        MemorySample {
            total_bytes: 1000,
            available_bytes: (1000.0 * (1.0 - used_fraction)).round() as u64,
            ..MemorySample::default()
        }
    }

    #[test]
    fn pressure_band_from_headroom_only() {
        let cases = [
            (0.0, PressureBand::Ok),
            (0.69, PressureBand::Ok),
            (0.70, PressureBand::Warn),
            (0.89, PressureBand::Warn),
            (0.90, PressureBand::Critical),
            (0.99, PressureBand::Critical),
        ];
        for (frac, expected) in cases {
            assert_eq!(
                pressure_band(&sample_with_fraction(frac)),
                expected,
                "headroom fraction {frac}"
            );
        }
    }

    #[test]
    fn pressure_band_escalates_on_psi_when_headroom_calm() {
        // Headroom alone is Ok (50% used); PSI drives the band up.
        let mut mem = sample_with_fraction(0.50);
        assert_eq!(pressure_band(&mem), PressureBand::Ok);

        mem.psi_mem_some_avg10 = Some(6.0);
        assert_eq!(pressure_band(&mem), PressureBand::Warn);

        mem.psi_mem_some_avg10 = Some(25.0);
        assert_eq!(pressure_band(&mem), PressureBand::Critical);

        // io pressure alone (mem PSI absent) still escalates.
        let mem_io = MemorySample {
            psi_io_some_avg10: Some(25.0),
            ..sample_with_fraction(0.50)
        };
        assert_eq!(pressure_band(&mem_io), PressureBand::Critical);
    }

    #[test]
    fn pressure_band_from_macos_level() {
        let cases = [
            (1u8, PressureBand::Ok),
            (2, PressureBand::Warn),
            (4, PressureBand::Critical),
        ];
        for (level, expected) in cases {
            let mem = MemorySample {
                macos_pressure_level: Some(level),
                ..sample_with_fraction(0.50)
            };
            assert_eq!(pressure_band(&mem), expected, "macos level {level}");
        }
    }

    #[test]
    fn pressure_band_takes_worst_of_inputs() {
        // Critical headroom must not be softened by a calm PSI reading.
        let mem = MemorySample {
            psi_mem_some_avg10: Some(1.0),
            ..sample_with_fraction(0.95)
        };
        assert_eq!(pressure_band(&mem), PressureBand::Critical);
    }

    #[test]
    fn pressure_band_default_sample_is_ok() {
        assert_eq!(pressure_band(&MemorySample::default()), PressureBand::Ok);
    }

    #[test]
    fn agent_table_visible_rows_handles_boundaries() {
        let row_cases = [(9, 0), (10, 1), (16, 7)];
        for (height, expected) in row_cases {
            assert_eq!(agent_table_visible_rows(height), expected);
        }
    }

    #[test]
    fn agent_table_title_padding_uses_display_width() {
        for title in ["agent", "日本語", "aaaaaaaé"] {
            let formatted = format_agent_title(title, 8);
            assert_eq!(formatted.width(), 8, "title {title:?}");
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

//! Preview panel component

use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::session::Instance;
use crate::tui::styles::Theme;

/// Light value type the renderers consume in place of a raw `&str`.
/// The caller is expected to hand over the cached parse from
/// `PreviewCache::ensure_parsed`; we then read it directly for the
/// actual render.
///
/// Passing a pre-parsed `Text` is the whole point of the
/// optimisation: it lets the cache update once per content change
/// rather than re-running the full `ansi-to-tui` pipeline on every
/// frame. See `PreviewCache::ensure_parsed` for the parse-and-cache
/// contract.
pub struct CachedPreview<'a> {
    /// `None` means the source `content` was empty (no pane bytes
    /// yet, or just cleared); callers render their own placeholder.
    pub text: Option<&'a Text<'static>>,
}

impl<'a> CachedPreview<'a> {
    pub fn from_text(text: Option<&'a Text<'static>>) -> Self {
        Self { text }
    }
}

/// Row count of the Agent-view info header (profile/tool, path, status,
/// optional sandbox line, optional worktree block) for `instance`.
///
/// Exposed at the module level so callers outside `Preview::render_with_cache`
/// can compute the same split. In particular, the live-send sync resize
/// in `HomeView::finalize_live_send_resize` needs to size the tmux pane
/// to the OUTPUT portion, not the full inner. The output portion is
/// `inner.height - agent_info_height(inst) - 1`: subtract the info
/// header, then subtract one more row for the inner ` Output ` banner
/// that `render_output_cached` draws on top of the output sub-rect (a
/// `Borders::TOP` block consumes one row). If the agent renders into a
/// taller pane than the visible output area, the top of its output gets
/// clipped on every frame and the user sees content shifted up.
pub fn agent_info_height(instance: &Instance) -> u16 {
    let base: u16 = 3; // profile+tool / path / status
    let sandbox_lines: u16 = if instance.is_sandboxed() { 1 } else { 0 };
    if let Some(wt) = instance.worktree_info.as_ref() {
        // blank + header + branch + main (+ optional base)
        let base_branch_line: u16 = if wt.base_branch.is_some() { 1 } else { 0 };
        base + sandbox_lines + 4 + base_branch_line
    } else {
        base + sandbox_lines
    }
}

/// Row count of the Terminal-view (and Tool-view) info header
/// (title / path / status, plus one optional sandbox row) for
/// `instance`.
///
/// Symmetric with [`agent_info_height`]: the live-send sync resize
/// against a terminal target needs the OUTPUT portion of the preview
/// pane, which is `inner.height - terminal_info_height(inst) - 1`
/// (info header + one row for the inner ` Terminal Output ` banner).
pub fn terminal_info_height(instance: &Instance) -> u16 {
    let base: u16 = 3; // title / path / status
    let sandbox_lines: u16 = if instance.sandbox_info.as_ref().is_some_and(|s| s.enabled) {
        1
    } else {
        0
    };
    base + sandbox_lines
}

/// The geometry of the preview body, computed once so every consumer agrees on
/// where the output goes and how many rows it spans.
///
/// Historically the info-header / banner / output split was re-derived
/// independently in the renderers (`Layout` + `Borders`), in `render_preview`
/// (for the tmux pane size and the scroll clamp), and in the live `[offset/max]`
/// footer. Each derivation drifted by a row at some point, which is the bug
/// #1521, #1570, and #1604 each chased in turn. `PreviewLayout::compute` is the
/// single definition; `output.height` is THE visible-row count. Anyone needing
/// preview geometry calls this rather than re-counting rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewLayout {
    /// The info-header rect, present iff the header is shown (header toggle on
    /// and the viewport is not compact).
    pub info: Option<Rect>,
    /// The inner ` Output ` / ` Terminal Output ` banner row, present exactly
    /// when `info` is (it visually separates the header from the body).
    pub banner: Option<Rect>,
    /// Where captured agent/terminal output paints. `output.height` is the
    /// authoritative visible-row count for scrolling and pane sizing.
    pub output: Rect,
}

impl PreviewLayout {
    /// Split `area` (the preview block's inner rect) into header / banner /
    /// output. With the header hidden (toggle off) or the viewport compact, the
    /// output claims the whole `area` and there is no banner. Otherwise the
    /// header takes the top `info_height` rows, a one-row banner follows, and
    /// the output gets the rest, clamped so a pane shorter than that chrome
    /// yields a zero-height output instead of underflowing.
    pub(crate) fn compute(area: Rect, compact: bool, show_info: bool, info_height: u16) -> Self {
        if compact || !show_info {
            return Self {
                info: None,
                banner: None,
                output: area,
            };
        }
        let chrome = info_height.saturating_add(1).min(area.height);
        let info_h = info_height.min(area.height);
        let info = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: info_h,
        };
        // The banner row only exists when the pane had room for it on top of
        // the header (i.e. `chrome` reached `info_height + 1`).
        let banner = if chrome > info_h {
            Some(Rect {
                x: area.x,
                y: area.y + info_h,
                width: area.width,
                height: 1,
            })
        } else {
            None
        };
        let output = Rect {
            x: area.x,
            y: area.y + chrome,
            width: area.width,
            height: area.height - chrome,
        };
        Self {
            info: Some(info),
            banner,
            output,
        }
    }
}

pub struct Preview;

impl Preview {
    #[allow(clippy::too_many_arguments)]
    pub fn render_terminal_preview(
        frame: &mut Frame,
        area: Rect,
        instance: &Instance,
        terminal_running: bool,
        cached_output: CachedPreview<'_>,
        scroll_offset: u16,
        theme: &Theme,
        compact: bool,
        show_info: bool,
    ) {
        // One source of truth for the header / banner / output split. Compact
        // viewports and the hidden-header toggle both collapse to "output owns
        // the whole area" inside `PreviewLayout::compute`, symmetric with the
        // Structured view's `render_with_cache`.
        let layout =
            PreviewLayout::compute(area, compact, show_info, terminal_info_height(instance));

        if let Some(info_area) = layout.info {
            // Minimal info for terminal view.
            let mut info_lines = vec![
                Line::from(vec![
                    Span::styled("Title:   ", Style::default().fg(theme.dimmed)),
                    Span::styled(&instance.title, Style::default().fg(theme.text).bold()),
                ]),
                Line::from(vec![
                    Span::styled("Path:    ", Style::default().fg(theme.dimmed)),
                    Span::styled(
                        shorten_path(&instance.project_path),
                        Style::default().fg(theme.text),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Status:  ", Style::default().fg(theme.dimmed)),
                    Span::styled(
                        if terminal_running {
                            "Running"
                        } else {
                            "Not started"
                        },
                        Style::default().fg(if terminal_running {
                            theme.terminal_active
                        } else {
                            theme.dimmed
                        }),
                    ),
                ]),
            ];
            if let Some(sandbox) = &instance.sandbox_info {
                if sandbox.enabled {
                    info_lines.push(Line::from(vec![
                        Span::styled("Sandbox: ", Style::default().fg(theme.dimmed)),
                        Span::styled(&sandbox.container_name, Style::default().fg(theme.sandbox)),
                    ]));
                }
            }
            frame.render_widget(Paragraph::new(info_lines), info_area);
        }

        // `output.height` is the authoritative visible-row count; no separate
        // banner subtraction (that lives entirely in `PreviewLayout::compute`).
        let visible_height = layout.output.height as usize;
        // Use the pre-parsed cache when the terminal is up; suppress
        // it otherwise so the "press Enter to start terminal" hint
        // can take the inner area instead of a stale capture.
        let parsed_output = if terminal_running {
            cached_output.text
        } else {
            None
        };
        let line_count = parsed_output.map_or(0, |t| t.lines.len());

        // The inner ` Terminal Output ` banner is present exactly when the info
        // section is (see `PreviewLayout`): with it hidden the outer block title
        // already names the view and the scroll indicator is hoisted there by
        // the caller, so the body claims the freed row.
        if let Some(banner) = layout.banner {
            let mut block = Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(theme.border))
                .title(" Terminal Output ")
                .title_style(Style::default().fg(theme.dimmed));
            if let Some(indicator) =
                format_scroll_indicator(line_count, visible_height, scroll_offset)
            {
                block = block.title_top(
                    Line::from(indicator)
                        .right_aligned()
                        .style(Style::default().fg(theme.dimmed)),
                );
            }
            frame.render_widget(block, banner_block_area(layout.output, banner));
        }

        let inner = layout.output;
        if !terminal_running {
            let hint = Paragraph::new("Press Enter to start terminal")
                .style(Style::default().fg(theme.dimmed))
                .alignment(Alignment::Center);
            frame.render_widget(hint, inner);
        } else if let Some(output_text) = parsed_output {
            let paragraph_scroll = compute_scroll(line_count, visible_height, scroll_offset);

            // ratatui's `Paragraph::new` takes ownership of the
            // `Text`, so we clone the cached parse here. The clone
            // walks the parsed `Vec<Line<'static>>` (one allocation
            // per Span's `Cow`) but is still much cheaper than
            // re-running `ansi-to-tui` on the raw pane bytes, which
            // is what this whole caching dance avoids.
            let paragraph = Paragraph::new(output_text.clone())
                .style(Style::default().fg(theme.text))
                .scroll((paragraph_scroll, 0));

            frame.render_widget(paragraph, inner);
        } else {
            let hint = Paragraph::new("No output available")
                .style(Style::default().fg(theme.dimmed))
                .alignment(Alignment::Center);
            frame.render_widget(hint, inner);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_with_cache(
        frame: &mut Frame,
        area: Rect,
        instance: &Instance,
        cached_output: CachedPreview<'_>,
        scroll_offset: u16,
        theme: &Theme,
        idle_decay_window: Duration,
        compact: bool,
        show_info: bool,
    ) {
        // One source of truth for the split. With the header hidden or the
        // viewport compact, `PreviewLayout::compute` returns `info: None` /
        // `banner: None` and the output claims the whole pane (the outer block
        // already says "Preview", so an inner banner would be redundant chrome).
        let layout = PreviewLayout::compute(area, compact, show_info, agent_info_height(instance));
        if let Some(info_area) = layout.info {
            Self::render_info(frame, info_area, instance, theme, idle_decay_window);
        }
        Self::render_output_cached(
            frame,
            layout.output,
            layout.banner,
            instance,
            cached_output,
            scroll_offset,
            theme,
        );
    }

    pub(crate) fn render_info(
        frame: &mut Frame,
        area: Rect,
        instance: &Instance,
        theme: &Theme,
        idle_decay_window: Duration,
    ) {
        let mut info_lines = Vec::new();

        // Profile and Tool on the same row to save vertical space
        let mut profile_tool_spans = Vec::new();
        if !instance.source_profile.is_empty() {
            profile_tool_spans.push(Span::styled("Profile: ", Style::default().fg(theme.dimmed)));
            profile_tool_spans.push(Span::styled(
                &instance.source_profile,
                Style::default().fg(theme.accent),
            ));
            profile_tool_spans.push(Span::raw("  "));
        }
        profile_tool_spans.push(Span::styled("Tool: ", Style::default().fg(theme.dimmed)));
        profile_tool_spans.push(Span::styled(
            &instance.tool,
            Style::default().fg(theme.accent),
        ));
        info_lines.push(Line::from(profile_tool_spans));

        info_lines.extend([
            Line::from(vec![
                Span::styled("Path:    ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    shorten_path(&instance.project_path),
                    Style::default().fg(theme.text),
                ),
            ]),
            Line::from(vec![
                Span::styled("Status:  ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    format!("{:?}", instance.status),
                    Style::default().fg(match instance.status {
                        crate::session::Status::Running => theme.running,
                        crate::session::Status::Waiting => theme.waiting,
                        crate::session::Status::Idle => {
                            theme.idle_color_at_age(instance.idle_age(), idle_decay_window)
                        }
                        crate::session::Status::Unknown => theme.waiting,
                        crate::session::Status::Stopped => theme.dimmed,
                        crate::session::Status::Error => theme.error,
                        crate::session::Status::Starting => theme.dimmed,
                        crate::session::Status::Deleting => theme.waiting,
                        crate::session::Status::Creating => theme.accent,
                    }),
                ),
            ]),
        ]);

        // Add sandbox information if present
        if let Some(sandbox) = &instance.sandbox_info {
            if sandbox.enabled {
                info_lines.push(Line::from(vec![
                    Span::styled("Sandbox: ", Style::default().fg(theme.dimmed)),
                    Span::styled(&sandbox.container_name, Style::default().fg(theme.sandbox)),
                ]));
            }
        }

        // Add worktree information if present
        if let Some(wt_info) = &instance.worktree_info {
            info_lines.push(Line::from(""));
            info_lines.push(Line::from(vec![
                Span::styled("─", Style::default().fg(theme.border)),
                Span::styled(" Worktree ", Style::default().fg(theme.dimmed)),
                Span::styled("─", Style::default().fg(theme.border)),
            ]));
            info_lines.push(Line::from(vec![
                Span::styled("Branch:  ", Style::default().fg(theme.dimmed)),
                Span::styled(&wt_info.branch, Style::default().fg(theme.branch)),
            ]));
            info_lines.push(Line::from(vec![
                Span::styled("Main:    ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    shorten_path(&wt_info.main_repo_path),
                    Style::default().fg(theme.text),
                ),
            ]));
            if let Some(base) = wt_info.base_branch.as_deref() {
                info_lines.push(Line::from(vec![
                    Span::styled("Base:    ", Style::default().fg(theme.dimmed)),
                    Span::styled(base, Style::default().fg(theme.branch)),
                ]));
            }
        }

        let paragraph = Paragraph::new(info_lines);
        frame.render_widget(paragraph, area);
    }

    fn render_output_cached(
        frame: &mut Frame,
        output: Rect,
        banner: Option<Rect>,
        instance: &Instance,
        cached_output: CachedPreview<'_>,
        scroll_offset: u16,
        theme: &Theme,
    ) {
        // `output.height` is the visible-row count straight from `PreviewLayout`;
        // there is no banner subtraction here (the banner, when present, sits in
        // its own row above `output`).
        let visible_height = output.height as usize;
        // The error path below returns early, so by the time we use
        // `parsed_output` for the output Paragraph the error case has
        // been handled. Until then `parsed_output` is just the cached
        // parse passed in by the caller (renamed for readability).
        let parsed_output = cached_output.text;
        let line_count = parsed_output.map_or(0, |t| t.lines.len());

        // The inner ` Output ` banner is drawn only when `PreviewLayout` gave us
        // a banner row (info header shown, non-compact). The outer block already
        // names the session when it's hidden, so the body claims the freed row.
        if let Some(banner) = banner {
            let mut block = Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(theme.border))
                .title(" Output ")
                .title_style(Style::default().fg(theme.dimmed));
            if let Some(indicator) =
                format_scroll_indicator(line_count, visible_height, scroll_offset)
            {
                block = block.title_top(
                    Line::from(indicator)
                        .right_aligned()
                        .style(Style::default().fg(theme.dimmed)),
                );
            }
            frame.render_widget(block, banner_block_area(output, banner));
        }
        let inner = output;

        if let Some(error) = &instance.last_error {
            let mut error_lines: Vec<Line> = vec![
                Line::from(Span::styled(
                    "Error:",
                    Style::default().fg(theme.error).bold(),
                )),
                Line::from(""),
            ];
            for line in error.split('\n') {
                error_lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(theme.error),
                )));
            }
            let paragraph = Paragraph::new(error_lines).wrap(Wrap { trim: false });
            frame.render_widget(paragraph, inner);
            return;
        }

        if let Some(output_text) = parsed_output {
            let paragraph_scroll = compute_scroll(line_count, visible_height, scroll_offset);

            // ratatui's `Paragraph::new` takes ownership of the
            // `Text`, so we clone the cached parse here. The clone
            // walks the parsed `Vec<Line<'static>>` (one allocation
            // per Span's Cow), which is a few-millisecond operation
            // but well under the cost of re-running `ansi-to-tui` on
            // the raw bytes; the latter is what this whole caching
            // dance avoids. If a cheaper "render Paragraph by
            // reference" path appears in a future ratatui release,
            // we can revisit.
            let paragraph = Paragraph::new(output_text.clone())
                .style(Style::default().fg(theme.text))
                .scroll((paragraph_scroll, 0));

            frame.render_widget(paragraph, inner);
        } else {
            let hint = Paragraph::new("No output available")
                .style(Style::default().fg(theme.dimmed))
                .alignment(Alignment::Center);
            frame.render_widget(hint, inner);
        }
    }
}

/// The `Borders::TOP` block that draws the ` Output ` / ` Terminal Output `
/// banner. It spans the banner row plus the whole output body so its single top
/// border lands on the banner row and `block.inner()` coincides exactly with
/// `output`. Built from `PreviewLayout`'s `output` + `banner` rects so the
/// banner can never be sized independently of the body it caps.
fn banner_block_area(output: Rect, banner: Rect) -> Rect {
    Rect {
        x: output.x,
        y: banner.y,
        width: output.width,
        height: output.height.saturating_add(1),
    }
}

/// Pick the row offset passed to `Paragraph::scroll`. Zero user offset shows
/// the bottom of the cached pane (live-follow). A positive offset scrolls the
/// same number of lines back, saturating at the top of the capture.
///
/// Exposed at crate visibility so the preview drag-select code can map a
/// screen row to the absolute content line under it: the value returned here
/// is the index of the parsed-text line painted on the output pane's top row,
/// so `first_line + (screen_row - pane.y)` is the line beneath any cell.
pub(crate) fn compute_scroll(line_count: usize, visible_height: usize, user_offset: u16) -> u16 {
    if line_count <= visible_height {
        return 0;
    }
    let bottom = (line_count - visible_height) as u16;
    bottom.saturating_sub(user_offset)
}

/// Render a tmux-style ` [offset/max] ` indicator when the user has scrolled
/// back. Returns `None` while live-following or when the content fits in view.
pub fn format_scroll_indicator(
    line_count: usize,
    visible_height: usize,
    user_offset: u16,
) -> Option<String> {
    if user_offset == 0 || line_count <= visible_height {
        return None;
    }
    let max_offset = (line_count - visible_height) as u16;
    let clamped = user_offset.min(max_offset);
    Some(format!(" [{}/{}] ", clamped, max_offset))
}

/// Parse a captured ANSI string into a ratatui `Text`.
///
/// Visible at the module level so `PreviewCache::ensure_parsed` can
/// call it from `src/tui/home/mod.rs` to drive the cache.
pub fn parse_output_text(content: &str) -> Text<'static> {
    let cleaned = crate::tmux::utils::strip_osc_st(content);
    cleaned.into_text().unwrap_or_else(|_| Text::from(cleaned))
}

fn shorten_path(path: &str) -> String {
    let path_buf = std::path::PathBuf::from(path);

    if let Some(home) = dirs::home_dir() {
        if let (Ok(canonical_path), Ok(canonical_home)) =
            (path_buf.canonicalize(), home.canonicalize())
        {
            let path_str = canonical_path.to_string_lossy();
            if let Some(home_str) = canonical_home.to_str() {
                if let Some(stripped) = path_str.strip_prefix(home_str) {
                    return format!("~{}", stripped);
                }
            }
            return path_str.into_owned();
        }

        if let Some(home_str) = home.to_str() {
            if let Some(stripped) = path.strip_prefix(home_str) {
                return format!("~{}", stripped);
            }
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shorten_path_with_home() {
        if let Some(home) = dirs::home_dir() {
            if let Some(home_str) = home.to_str() {
                let path = format!("{}/projects/myapp", home_str);
                let shortened = shorten_path(&path);
                assert_eq!(shortened, "~/projects/myapp");
            }
        }
    }

    #[test]
    fn test_shorten_path_without_home_prefix() {
        let path = "/tmp/some/path";
        let shortened = shorten_path(path);
        assert_eq!(shortened, "/tmp/some/path");
    }

    #[test]
    fn test_shorten_path_exact_home() {
        if let Some(home) = dirs::home_dir() {
            if let Some(home_str) = home.to_str() {
                let shortened = shorten_path(home_str);
                assert_eq!(shortened, "~");
            }
        }
    }

    #[test]
    fn test_shorten_path_relative() {
        let path = "relative/path";
        let shortened = shorten_path(path);
        assert_eq!(shortened, "relative/path");
    }

    #[test]
    fn test_shorten_path_empty() {
        let path = "";
        let shortened = shorten_path(path);
        assert_eq!(shortened, "");
    }

    #[test]
    fn test_shorten_path_similar_prefix_not_home() {
        if let Some(home) = dirs::home_dir() {
            if let Some(home_str) = home.to_str() {
                let path = format!("{}extra/not/home", home_str);
                let shortened = shorten_path(&path);
                assert_eq!(shortened, format!("~extra/not/home"));
            }
        }
    }

    #[test]
    fn test_shorten_path_preserves_trailing_slash() {
        if let Some(home) = dirs::home_dir() {
            if let Some(home_str) = home.to_str() {
                let path = format!("{}/projects/", home_str);
                let shortened = shorten_path(&path);
                assert_eq!(shortened, "~/projects/");
            }
        }
    }

    // Single source of truth for the preview split. These pin down the row
    // arithmetic that #1521 / #1570 / #1604 each got wrong in a different
    // derivation; now there is only one.
    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn layout_hidden_info_gives_output_the_whole_area() {
        // Header toggled off (or compact): no header, no banner, output == area.
        let area = rect(0, 0, 80, 40);
        let l = PreviewLayout::compute(area, false, false, 7);
        assert_eq!(l.info, None);
        assert_eq!(l.banner, None);
        assert_eq!(l.output, area);
        // Compact forces the same regardless of the toggle.
        assert_eq!(PreviewLayout::compute(area, true, true, 7).output, area);
    }

    #[test]
    fn layout_shown_info_carves_header_plus_banner_once() {
        let area = rect(2, 3, 80, 40);
        let l = PreviewLayout::compute(area, false, true, 7);
        // Header: top 7 rows.
        assert_eq!(l.info, Some(rect(2, 3, 80, 7)));
        // Banner: the single row just below the header.
        assert_eq!(l.banner, Some(rect(2, 3 + 7, 80, 1)));
        // Output: the rest, shifted down by header + banner (7 + 1).
        assert_eq!(l.output, rect(2, 3 + 8, 80, 40 - 8));
        // The banner block spans banner + output so its inner == output.
        let block_area = banner_block_area(l.output, l.banner.unwrap());
        assert_eq!(block_area, rect(2, 3 + 7, 80, 40 - 8 + 1));
    }

    #[test]
    fn layout_clamps_when_pane_shorter_than_chrome() {
        // Pane shorter than header + banner: output clamps to zero height and
        // never underflows (the old panic-on-subtraction case).
        let area = rect(0, 0, 80, 3);
        let l = PreviewLayout::compute(area, false, true, 4);
        assert_eq!(l.output.height, 0);
        assert!(l.output.y <= area.y + area.height);
    }

    // End to end: a captured screen exactly as tall as the banner-less output,
    // live-following (offset 0). The scroll must be 0 so the top row (a fresh
    // shell's cursor) stays on screen. This is the #1604 "first row hidden"
    // regression, now expressed against the single layout source.
    #[test]
    fn full_height_capture_does_not_scroll_when_banner_hidden() {
        let area = rect(0, 0, 80, 40);
        let l = PreviewLayout::compute(area, false, false, 7);
        let visible = l.output.height as usize;
        assert_eq!(visible, 40);
        assert_eq!(compute_scroll(visible, visible, 0), 0);
    }

    #[test]
    fn compute_scroll_live_follow_when_content_fits() {
        assert_eq!(compute_scroll(5, 10, 0), 0);
        assert_eq!(compute_scroll(5, 10, 20), 0);
    }

    #[test]
    fn compute_scroll_sticks_to_bottom_with_zero_offset() {
        assert_eq!(compute_scroll(100, 20, 0), 80);
    }

    #[test]
    fn compute_scroll_walks_back_by_offset() {
        assert_eq!(compute_scroll(100, 20, 15), 65);
    }

    #[test]
    fn compute_scroll_saturates_at_top() {
        assert_eq!(compute_scroll(100, 20, 500), 0);
    }

    #[test]
    fn scroll_indicator_hidden_when_live() {
        assert_eq!(format_scroll_indicator(100, 20, 0), None);
    }

    #[test]
    fn scroll_indicator_hidden_when_content_fits() {
        assert_eq!(format_scroll_indicator(10, 20, 5), None);
    }

    #[test]
    fn scroll_indicator_reports_position_and_max() {
        assert_eq!(
            format_scroll_indicator(100, 20, 15),
            Some(" [15/80] ".to_string())
        );
    }

    #[test]
    fn scroll_indicator_clamps_to_max() {
        assert_eq!(
            format_scroll_indicator(100, 20, 500),
            Some(" [80/80] ".to_string())
        );
    }

    // `agent_info_height` drives both the preview layout split in
    // `render_with_cache` and the live-send sync resize in
    // `HomeView::finalize_live_send_resize`. A one-row drift here brings
    // the shifted-preview bug right back, so each branch of the formula
    // gets a dedicated case.
    mod agent_info_height {
        use super::super::agent_info_height;
        use crate::session::{Instance, SandboxInfo, WorktreeInfo};
        use chrono::Utc;

        fn worktree(base_branch: Option<&str>) -> WorktreeInfo {
            WorktreeInfo {
                branch: "feature/x".into(),
                main_repo_path: "/repo".into(),
                managed_by_aoe: true,
                created_at: Utc::now(),
                base_branch: base_branch.map(str::to_string),
            }
        }

        fn enabled_sandbox() -> SandboxInfo {
            SandboxInfo {
                enabled: true,
                container_id: None,
                image: "img".into(),
                container_name: "ctr".into(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            }
        }

        #[test]
        fn plain_session_is_three_rows() {
            let inst = Instance::new("plain", "/tmp/plain");
            assert_eq!(agent_info_height(&inst), 3);
        }

        #[test]
        fn sandboxed_adds_one_row() {
            let mut inst = Instance::new("sandboxed", "/tmp/sandboxed");
            inst.sandbox_info = Some(enabled_sandbox());
            assert_eq!(agent_info_height(&inst), 4);
        }

        #[test]
        fn worktree_without_base_branch_adds_four_rows() {
            let mut inst = Instance::new("wt", "/tmp/wt");
            inst.worktree_info = Some(worktree(None));
            assert_eq!(agent_info_height(&inst), 3 + 4);
        }

        #[test]
        fn worktree_with_base_branch_adds_five_rows() {
            let mut inst = Instance::new("wt-base", "/tmp/wt-base");
            inst.worktree_info = Some(worktree(Some("main")));
            assert_eq!(agent_info_height(&inst), 3 + 4 + 1);
        }

        #[test]
        fn sandboxed_plus_worktree_with_base_branch_is_max() {
            let mut inst = Instance::new("both", "/tmp/both");
            inst.sandbox_info = Some(enabled_sandbox());
            inst.worktree_info = Some(worktree(Some("main")));
            assert_eq!(agent_info_height(&inst), 3 + 1 + 4 + 1);
        }

        #[test]
        fn disabled_sandbox_does_not_count() {
            let mut inst = Instance::new("disabled", "/tmp/disabled");
            let mut sandbox = enabled_sandbox();
            sandbox.enabled = false;
            inst.sandbox_info = Some(sandbox);
            assert_eq!(agent_info_height(&inst), 3);
        }
    }

    // Terminal-view counterpart of `agent_info_height`. Same drift-guard
    // motivation: the live-send sync resize against a terminal target
    // sizes the tmux pane to `inner - terminal_info_height - 1`. A wrong
    // formula here brings the shifted-preview bug back in Terminal view.
    mod terminal_info_height {
        use super::super::terminal_info_height;
        use crate::session::{Instance, SandboxInfo, WorktreeInfo};
        use chrono::Utc;

        fn enabled_sandbox() -> SandboxInfo {
            SandboxInfo {
                enabled: true,
                container_id: None,
                image: "img".into(),
                container_name: "ctr".into(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            }
        }

        #[test]
        fn plain_session_is_three_rows() {
            let inst = Instance::new("plain", "/tmp/plain");
            assert_eq!(terminal_info_height(&inst), 3);
        }

        #[test]
        fn sandboxed_adds_one_row() {
            let mut inst = Instance::new("sandboxed", "/tmp/sandboxed");
            inst.sandbox_info = Some(enabled_sandbox());
            assert_eq!(terminal_info_height(&inst), 4);
        }

        #[test]
        fn disabled_sandbox_does_not_count() {
            let mut inst = Instance::new("disabled", "/tmp/disabled");
            let mut sandbox = enabled_sandbox();
            sandbox.enabled = false;
            inst.sandbox_info = Some(sandbox);
            assert_eq!(terminal_info_height(&inst), 3);
        }

        #[test]
        fn worktree_info_does_not_count() {
            // Worktree info is an Agent-view-only block; the terminal
            // view doesn't render it, so the height stays at 3.
            let mut inst = Instance::new("wt", "/tmp/wt");
            inst.worktree_info = Some(WorktreeInfo {
                branch: "feature/x".into(),
                main_repo_path: "/repo".into(),
                managed_by_aoe: true,
                created_at: Utc::now(),
                base_branch: Some("main".into()),
            });
            assert_eq!(terminal_info_height(&inst), 3);
        }
    }
}

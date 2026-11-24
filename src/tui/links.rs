//! Finding the links on a rendered preview row: the OSC 8 runs the pane
//! advertised there, then plain URLs in the row's own text. An advertised
//! target wins wherever the two overlap, since a bare match is only inference.
//!
//! A ratatui cell cannot carry a hyperlink, so each row's OSC 8 runs ride
//! alongside the text (`PreviewCache::links`) at the character offset the frame
//! put them, and are placed here by the columns those characters paint.
//!
//! The result never overlaps itself: the painter and the click hit-test must
//! agree about which candidate owns a cell, or the terminal advertises one
//! target on hover while a click opens another.

use crate::tui::components::text::line_columns;

/// One link's place on a rendered row: `[start, end)` column offsets from the
/// output pane's left edge, and the target a click there opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkSpan {
    pub(crate) start: u16,
    pub(crate) end: u16,
    pub(crate) uri: String,
}

/// Trailing characters a URL is very unlikely to end on, so `(see https://x)`
/// and `https://x.` resolve to the URL rather than to the punctuation around
/// it. Closing brackets are trimmed only when the URL does not open them
/// itself, which keeps a Wikipedia-style `..._(disambiguation)` intact.
const TRAILING_TRIM: &[char] = &['.', ',', ';', ':', '!', '?', '\'', '"', '>', ')', ']', '}'];

/// Find where `links`, the OSC 8 runs the frame put on this row, and any bare
/// URLs sit on one rendered row.
///
/// Only a link [`crate::util::is_openable_link`] accepts under `extra_prefixes`
/// is placed, so the painter, the click hit-test and the OSC 8 re-emitted to the
/// host terminal all see the same set. A run whose text the row does not show
/// where the frame put it is not placed. A bare URL wrapped by the pane is cut
/// at the edge rather than joined.
pub(crate) fn link_spans_for_line(
    line: &ratatui::text::Line,
    width: u16,
    links: &[crate::tmux::osc8::RowLink],
    extra_prefixes: &[String],
) -> Vec<LinkSpan> {
    if width == 0 {
        return Vec::new();
    }
    // Cheap reject before building the column map, which allocates a scratch
    // buffer per row. Almost no row holds a link.
    let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let usable: Vec<&crate::tmux::osc8::RowLink> = links
        .iter()
        .filter(|l| crate::util::is_openable_link(&l.uri, extra_prefixes))
        .collect();
    if usable.is_empty() && !holds_scheme(&plain) {
        return Vec::new();
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    if !usable.is_empty() {
        let chars: Vec<char> = plain.chars().collect();
        let (columns, painted) = char_columns(line, width);
        for (rank, link) in usable.iter().enumerate() {
            let len = link.text.chars().count();
            let shown = chars
                .get(link.start..link.start + len)
                .is_some_and(|run| run.iter().copied().eq(link.text.chars()));
            let (Some(Some(start)), true) = (columns.get(link.start), shown) else {
                continue;
            };
            let end = columns[link.start + len].unwrap_or(painted);
            if end > *start {
                candidates.push(Candidate {
                    span: LinkSpan {
                        start: *start,
                        end,
                        uri: link.uri.clone(),
                    },
                    advertised: true,
                    rank,
                });
            }
        }
    }

    let columns = line_columns(line, width);
    for (start, end) in bare_url_ranges(&columns.text) {
        let end_column = columns.column_at(end);
        // `columns.text` is the row as painted, so it stops where the pane does.
        // A URL running to that boundary may be a prefix of the real one, and
        // opening `https://example.com/log` for `https://example.com/logout` is a
        // wrong action, not a missing one. The boundary is not always `width`: a
        // wide grapheme that does not fit leaves the text a column short. Refuse
        // anything reaching the end of a clipped row or the pane edge.
        if end_column >= width || (columns.is_clipped() && end == columns.text.len()) {
            continue;
        }
        candidates.push(Candidate {
            span: LinkSpan {
                start: columns.column_at(start),
                end: end_column,
                uri: columns.text[start..end].to_string(),
            },
            advertised: false,
            rank: 0,
        });
    }

    resolve_overlaps(candidates)
}

/// The column each `char` of `line` starts painting at (one past the last
/// included), `None` past what fits in `width`, and the columns painted. Same
/// grapheme and width rules as [`line_columns`].
fn char_columns(line: &ratatui::text::Line, width: u16) -> (Vec<Option<u16>>, u16) {
    use ratatui::buffer::CellWidth;
    use unicode_segmentation::UnicodeSegmentation;

    let mut at = Vec::new();
    let mut col = 0u16;
    let mut clipped = false;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let cells = if grapheme.contains(char::is_control) {
                0
            } else {
                grapheme.cell_width()
            };
            clipped |= col + cells > width;
            let start = (!clipped).then_some(col);
            at.extend(std::iter::repeat_n(start, grapheme.chars().count()));
            if !clipped {
                col += cells;
            }
        }
    }
    at.push((!clipped).then_some(col));
    (at, col)
}

/// A span before precedence has been applied.
struct Candidate {
    span: LinkSpan,
    /// The pane said so, rather than the row text implying it.
    advertised: bool,
    /// Position in the frame's link list; higher is lower on the pane.
    rank: usize,
}

/// Reduce candidates to a set that never overlaps itself, so the painter, the
/// OSC 8 emission and the click hit-test cannot disagree about a cell.
///
/// Precedence: advertised over inferred, then the longer span, then the more
/// recently printed, then the leftmost, so the result is order-independent.
fn resolve_overlaps(mut candidates: Vec<Candidate>) -> Vec<LinkSpan> {
    candidates.sort_by(|a, b| {
        b.advertised
            .cmp(&a.advertised)
            .then((b.span.end - b.span.start).cmp(&(a.span.end - a.span.start)))
            .then(b.rank.cmp(&a.rank))
            .then(a.span.start.cmp(&b.span.start))
    });
    let mut kept: Vec<LinkSpan> = Vec::new();
    for candidate in candidates {
        if kept
            .iter()
            .any(|held| held.start < candidate.span.end && candidate.span.start < held.end)
        {
            continue;
        }
        kept.push(candidate.span);
    }
    kept.sort_by_key(|s| s.start);
    kept
}

/// Whether `text` holds anything that could open a URL, matched without
/// allocating a lowercased copy of the row.
fn holds_scheme(text: &str) -> bool {
    text.as_bytes()
        .windows(4)
        .any(|w| w.eq_ignore_ascii_case(b"http"))
}

/// Byte ranges of the bare `http(s)://` URLs in `text`.
/// Deliberately simple: a scheme, then everything up to whitespace, with
/// trailing punctuation trimmed. Matching too little costs the user a link they
/// can see and cannot click.
fn bare_url_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while from < lower.len() {
        let Some(offset) = lower[from..].find("http") else {
            break;
        };
        let start = from + offset;
        let rest = &lower[start..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            from = start + "http".len();
            continue;
        }
        let end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |i| start + i);
        let end = trim_url_end(text, start, end);
        // A scheme with nothing after it is not a link.
        if crate::util::is_http_url(&text[start..end]) && text[start..end].len() > "https://".len()
        {
            out.push((start, end));
        }
        from = end.max(start + 1);
    }
    out
}

/// Walk `end` back over punctuation that reads as sentence structure rather
/// than part of the URL.
fn trim_url_end(text: &str, start: usize, mut end: usize) -> usize {
    while end > start {
        let candidate = text[start..end].chars().next_back();
        let Some(ch) = candidate else { break };
        if !TRAILING_TRIM.contains(&ch) {
            break;
        }
        // `)` closes something the URL opened, so it belongs to the URL.
        if ch == ')'
            && text[start..end].matches('(').count() >= text[start..end].matches(')').count()
        {
            break;
        }
        end -= ch.len_utf8();
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    fn link(start: usize, text: &str, uri: &str) -> crate::tmux::osc8::RowLink {
        crate::tmux::osc8::RowLink {
            row: 0,
            start,
            text: text.to_string(),
            uri: uri.to_string(),
        }
    }

    fn spans(row: &str, width: u16, links: &[crate::tmux::osc8::RowLink]) -> Vec<LinkSpan> {
        link_spans_for_line(&Line::from(row.to_string()), width, links, &[])
    }

    fn placed(found: &[LinkSpan]) -> Vec<(u16, u16, &str)> {
        found
            .iter()
            .map(|s| (s.start, s.end, s.uri.as_str()))
            .collect()
    }

    #[test]
    fn a_non_web_target_is_placed_only_under_an_allowed_prefix() {
        let line = Line::from("see notes/a.md here");
        let links = [link(4, "notes/a.md", "vscode://file/Users/u/notes/a.md")];
        assert!(link_spans_for_line(&line, 40, &links, &[]).is_empty());
        let found = link_spans_for_line(&line, 40, &links, &["vscode://file/".to_string()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].uri, "vscode://file/Users/u/notes/a.md");
    }

    #[test]
    fn osc8_runs_are_placed_where_the_frame_put_them() {
        const A: &str = "https://a.example";
        const B: &str = "https://b.example";
        type Case = (
            &'static str,
            &'static str,
            u16,
            Vec<crate::tmux::osc8::RowLink>,
            Vec<(u16, u16, &'static str)>,
        );
        let cases: Vec<Case> = vec![
            (
                "the visible text hides the target",
                "see the AoE repo now",
                40,
                vec![link(4, "the AoE repo", A)],
                vec![(4, 16, A)],
            ),
            (
                "wide graphemes occupy both of their columns",
                "\u{6f22}\u{5b57} the AoE repo",
                40,
                vec![link(3, "the AoE repo", A)],
                vec![(5, 17, A)],
            ),
            (
                "a repeated label keeps each run's own target",
                "see docs and docs",
                40,
                vec![link(4, "docs", A), link(13, "docs", B)],
                vec![(4, 8, A), (13, 17, B)],
            ),
            (
                "the same text outside the run stays plain",
                "docs: see docs",
                40,
                vec![link(10, "docs", A)],
                vec![(10, 14, A)],
            ),
            (
                "a short wrapped tail is still a link",
                "er, and then",
                40,
                vec![link(0, "er", A)],
                vec![(0, 2, A)],
            ),
            (
                "a run the row does not show there is not placed",
                "see the docs",
                40,
                vec![link(4, "docs", A)],
                vec![],
            ),
            (
                "a run the pane edge cuts is placed up to the edge",
                "the AoE repo",
                6,
                vec![link(0, "the AoE repo", A)],
                vec![(0, 6, A)],
            ),
            (
                "a run past the pane edge is not placed",
                "the AoE repo",
                3,
                vec![link(4, "AoE", A)],
                vec![],
            ),
        ];
        for (name, row, width, links, want) in cases {
            assert_eq!(placed(&spans(row, width, &links)), want, "{name}");
        }
    }

    #[test]
    fn osc8_spans_ignore_styling_around_the_text() {
        // `ansi-to-tui` splits a styled row into several spans; the run's
        // offset counts the characters of all of them.
        let line = Line::from(vec![
            Span::raw("see "),
            Span::styled("the AoE", Style::default().fg(Color::Green)),
            Span::raw(" repo"),
        ]);
        let found = link_spans_for_line(&line, 40, &[link(4, "the AoE repo", "https://a.co")], &[]);
        assert_eq!(placed(&found), vec![(4, 16, "https://a.co")]);
    }

    #[test]
    fn bare_urls_in_the_row_are_links_too() {
        // No sequence is involved; without this the user can see a URL the
        // preview will not open.
        // (row text, expected `[start, end)` columns and the URL each resolves)
        type Case = (&'static str, Vec<(u16, u16, &'static str)>);
        let cases: [Case; 7] = [
            (
                "see https://example.com/a now",
                vec![(4, 25, "https://example.com/a")],
            ),
            // Sentence punctuation is not part of the URL.
            (
                "go to https://example.com/a.",
                vec![(6, 27, "https://example.com/a")],
            ),
            (
                "(see https://example.com/a)",
                vec![(5, 26, "https://example.com/a")],
            ),
            // ...but a bracket the URL opened is.
            (
                "https://example.com/a_(b)",
                vec![(0, 25, "https://example.com/a_(b)")],
            ),
            // Two on one row.
            (
                "http://a.example and https://b.example",
                vec![(0, 16, "http://a.example"), (21, 38, "https://b.example")],
            ),
            // A scheme alone is not a link, and neither is a bare word.
            ("https:// and http and example.com", vec![]),
            ("nothing here", vec![]),
        ];
        for (row, expected) in cases {
            assert_eq!(placed(&spans(row, 60, &[])), expected, "{row:?}");
        }
    }

    #[test]
    fn a_bare_url_cut_off_by_the_pane_edge_is_not_a_link() {
        // Opening a prefix of the address is a wrong action, not a missing
        // one, so a URL that reaches the right edge is refused: at that point
        // it is indistinguishable from one the pane truncated.
        let row = "see https://example.com/logout";
        assert!(
            spans(row, 23, &[]).is_empty(),
            "a URL cut at the edge must not resolve to its prefix"
        );
        // The same row with room to spare resolves in full.
        let found = spans(row, 60, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].uri, "https://example.com/logout");
    }

    #[test]
    fn wide_graphemes_do_not_break_placement() {
        let found = spans(
            "見る 日本語ドキュメント です",
            60,
            &[link(3, "日本語ドキュメント", "https://example.com/ja")],
        );
        // 見る = 4 columns, space = 1, so the label starts at column 5 and
        // spans 9 wide graphemes = 18 columns.
        assert_eq!(placed(&found), vec![(5, 23, "https://example.com/ja")]);

        // The same for a bare URL carrying a wide path segment.
        let bare = spans("see https://example.com/日本 now", 60, &[]);
        assert_eq!(bare.len(), 1, "{bare:?}");
        assert_eq!(bare[0].uri, "https://example.com/日本");
    }

    #[test]
    fn an_advertised_target_wins_over_a_bare_match() {
        // A pane that wrapped a visible URL in OSC 8 pointing somewhere else
        // means it; the row text is not the authority.
        let found = spans(
            "https://example.com/shown",
            60,
            &[link(
                0,
                "https://example.com/shown",
                "https://example.com/real",
            )],
        );
        assert_eq!(found.len(), 1, "one span, not two overlapping ones");
        assert_eq!(found[0].uri, "https://example.com/real");
    }

    #[test]
    fn a_bare_url_beside_an_osc8_link_still_resolves() {
        let found = spans(
            "the AoE repo and https://example.com/b",
            60,
            &[link(0, "the AoE repo", "https://example.com/a")],
        );
        assert_eq!(
            placed(&found),
            vec![
                (0, 12, "https://example.com/a"),
                (17, 38, "https://example.com/b")
            ]
        );
    }
}

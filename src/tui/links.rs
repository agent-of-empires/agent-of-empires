//! Finding the links on a rendered preview row.
//!
//! Two sources, checked in that order:
//!
//! 1. **OSC 8 targets** the pane advertised. Neither the vt100 grid nor a
//!    ratatui cell can carry a hyperlink, so these ride alongside the text
//!    (`PreviewCache::links`) and are re-anchored here to the row showing them.
//! 2. **Plain URLs in the row's own text**, which carry no sequence at all and
//!    would otherwise be reachable only through the host terminal's own URL
//!    matching, and then only with a modifier because aoe holds the mouse.
//!
//! An OSC 8 target wins wherever the two overlap: it is what the pane actually
//! said the text points at, while a bare match is inference.
//!
//! Known limit of matching on text: the pane's table outlives the row that
//! filled it, so a link once printed as `[docs](x)` makes every later
//! standalone `docs` on screen resolve to `x`, on rows that carried no
//! sequence. `MIN_LINK_TEXT` and word alignment rule out the worst of it, but
//! not a common word. Hovering shows the target before the click, and only
//! http/https is ever opened, which is what keeps this misleading rather than
//! dangerous. Removing it needs positions the emulator does not keep: `vt100`
//! models no hyperlink per cell, and positions do not survive a reseed.
//!
//! The result never overlaps itself. Text matching can hand the same cell to
//! several candidates (an agent that prints `[docs](A)` then `[docs](B)`, or a
//! short link text occurring inside a longer one), and the painter and the
//! click hit-test must agree about which one owns that cell, or the terminal
//! advertises one target on hover while a click opens another.

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

/// Find where `links` and any bare URLs sit on one rendered row.
///
/// Matching on text is what makes the OSC 8 half survive the grid scrolling,
/// being reseeded from `capture-pane`, or the sequence itself having left the
/// stream long ago; none of those preserve cell positions. A link whose text
/// wraps across two rows matches on neither.
pub(crate) fn link_spans_for_line(
    line: &ratatui::text::Line,
    width: u16,
    links: &[crate::tmux::osc8::PaneLink],
) -> Vec<LinkSpan> {
    if width == 0 {
        return Vec::new();
    }
    // Cheap reject before building the column map, which allocates a scratch
    // buffer per row. Every visible row is scanned every frame, and almost none
    // hold a link, so this tests both sources against the line's own text
    // first: painting only truncates, so a needle absent from the raw text
    // cannot appear in the painted row.
    let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    // A needle rejected by `matchable` is empty, and `contains("")` is true for
    // every row, so one short link in the table would defeat the reject
    // entirely and put a scratch buffer back on every row of every frame.
    let usable: Vec<&crate::tmux::osc8::PaneLink> = links
        .iter()
        .filter(|l| {
            let needle = matchable(l);
            !needle.is_empty() && plain.contains(needle)
        })
        .collect();
    if usable.is_empty() && !holds_scheme(&plain) {
        return Vec::new();
    }

    let columns = line_columns(line, width);
    let mut candidates: Vec<Candidate> = Vec::new();

    // Newest last in the table, so a later index is the more recent print.
    for (rank, link) in usable.iter().enumerate() {
        let needle = matchable(link);
        let mut from = 0;
        while let Some(offset) = columns.text[from..].find(needle) {
            let start = from + offset;
            let end = start + needle.len();
            if word_aligned(&columns.text, start, end) {
                candidates.push(Candidate {
                    span: LinkSpan {
                        start: columns.column_at(start),
                        end: columns.column_at(end),
                        uri: link.uri.clone(),
                    },
                    advertised: true,
                    rank,
                });
            }
            from = end;
        }
    }

    for (start, end) in bare_url_ranges(&columns.text) {
        let end_column = columns.column_at(end);
        // `columns.text` is the row as painted, so it is cut at the pane's
        // right edge. A URL running to that edge may be a prefix of the real
        // one, and opening `https://example.com/log` for a link that reads
        // `https://example.com/logout` is a wrong action, not a missing one.
        // An advertised target cannot hit this: a truncated link text simply
        // fails to match. Rows wider than the pane show up when scrollback was
        // captured at an older geometry, or mid-resize.
        if end_column >= width {
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

/// A span before precedence has been applied.
struct Candidate {
    span: LinkSpan,
    /// The pane said so, rather than the row text implying it.
    advertised: bool,
    /// Position in the pane's link table; higher is more recently printed.
    rank: usize,
}

/// Reduce candidates to a set that never overlaps itself, so the painter, the
/// OSC 8 emission and the click hit-test cannot disagree about a cell.
///
/// Precedence: an advertised target over an inferred one, then the longer span
/// (a link text occurring inside another's belongs to the enclosing one), then
/// the more recently printed. Ties beyond that fall to the leftmost span, which
/// makes the result independent of iteration order.
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

/// Shortest link text worth matching. Text matching cannot tell one occurrence
/// of `1` from another, and a spurious span steals a click from the agent's own
/// mouse handling, so anything this short is left to the host terminal.
const MIN_LINK_TEXT: usize = 3;

/// The text of `link` if it is long enough to identify a run on a row.
fn matchable(link: &crate::tmux::osc8::PaneLink) -> &str {
    let text = link.text.trim();
    if text.chars().count() < MIN_LINK_TEXT {
        return "";
    }
    text
}

/// Whether `text[start..end]` stands alone rather than sitting inside a longer
/// word, so a `repo` link does not arm every `repository` on the row.
fn word_aligned(text: &str, start: usize, end: usize) -> bool {
    let before_ok = text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric());
    let after_ok = text[end..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_alphanumeric());
    let starts_word = text[start..end]
        .chars()
        .next()
        .is_some_and(char::is_alphanumeric);
    let ends_word = text[start..end]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric);
    (!starts_word || before_ok) && (!ends_word || after_ok)
}

/// Whether `text` holds anything that could open a URL, matched without
/// allocating a lowercased copy of the row.
fn holds_scheme(text: &str) -> bool {
    text.as_bytes()
        .windows(4)
        .any(|w| w.eq_ignore_ascii_case(b"http"))
}

/// Byte ranges of the bare `http(s)://` URLs in `text`.
///
/// Deliberately simple: a scheme, then everything up to whitespace, then
/// trailing punctuation trimmed off. A row of terminal output is not a document
/// and the payoff for a stricter grammar is small, while the cost of matching
/// too little is a link the user can see and cannot click.
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

    fn link(text: &str, uri: &str) -> crate::tmux::osc8::PaneLink {
        crate::tmux::osc8::PaneLink {
            text: text.to_string(),
            uri: uri.to_string(),
        }
    }

    fn spans(row: &str, width: u16, links: &[crate::tmux::osc8::PaneLink]) -> Vec<LinkSpan> {
        link_spans_for_line(&Line::from(row.to_string()), width, links)
    }

    #[test]
    fn osc8_spans_locate_the_target_by_display_column() {
        let links = vec![link("the AoE repo", "https://example.com/aoe")];
        // (row text, pane width, expected `[start, end)` column spans)
        type Case = (&'static str, u16, Vec<(u16, u16)>);
        let cases: [Case; 5] = [
            // The reported case: the visible text hides the target.
            ("see the AoE repo now", 40, vec![(4, 16)]),
            // Wide graphemes occupy both of their columns, so the span starts
            // and ends where the terminal painted the text.
            ("漢字 the AoE repo", 40, vec![(5, 17)]),
            // Two prints of the same link are two spans.
            ("the AoE repo the AoE repo", 40, vec![(0, 12), (13, 25)]),
            // Text that scrolled away or never matched marks nothing.
            ("unrelated output", 40, vec![]),
            // A row too narrow to hold the whole text has no span on it.
            ("the AoE repo", 6, vec![]),
        ];
        for (row, width, expected) in cases {
            let found = spans(row, width, &links);
            assert_eq!(
                found.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(),
                expected,
                "{row:?} at width {width}"
            );
            assert!(found.iter().all(|s| s.uri == "https://example.com/aoe"));
        }
    }

    #[test]
    fn osc8_spans_ignore_styling_around_the_text() {
        // `ansi-to-tui` splits a styled row into several spans; the match runs
        // over the painted cells, not one span's string.
        let line = Line::from(vec![
            Span::raw("see "),
            Span::styled("the AoE", Style::default().fg(Color::Green)),
            Span::raw(" repo"),
        ]);
        let found = link_spans_for_line(&line, 40, &[link("the AoE repo", "https://a.co")]);
        assert_eq!(
            found,
            vec![LinkSpan {
                start: 4,
                end: 16,
                uri: "https://a.co".to_string(),
            }]
        );
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
            let found = spans(row, 60, &[]);
            assert_eq!(
                found
                    .iter()
                    .map(|s| (s.start, s.end, s.uri.as_str()))
                    .collect::<Vec<_>>(),
                expected,
                "{row:?}"
            );
        }
    }

    #[test]
    fn the_same_text_twice_resolves_to_one_target_everywhere() {
        // An agent that prints [docs](A) then [docs](B) leaves both in the
        // table. Whatever the painter, the OSC 8 emission and the click agree
        // on, they must agree: a click opening A while the terminal reports B
        // on hover is a silent shadowing vector.
        let found = spans(
            "see the docs now",
            40,
            &[
                link("the docs", "https://a.example"),
                link("the docs", "https://b.example"),
            ],
        );
        assert_eq!(found.len(), 1, "one span owns those cells: {found:?}");
        // The more recently printed target wins.
        assert_eq!(found[0].uri, "https://b.example");
    }

    #[test]
    fn a_longer_link_text_owns_cells_a_shorter_one_sits_inside() {
        let found = spans(
            "open the AoE repo now",
            40,
            &[
                link("repo", "https://short.example"),
                link("the AoE repo", "https://long.example"),
            ],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].uri, "https://long.example");
        assert_eq!((found[0].start, found[0].end), (5, 17));
    }

    #[test]
    fn short_and_mid_word_texts_do_not_arm_spans() {
        // A spurious span steals the click from a mouse-tracking agent, so
        // text this weak is left to the host terminal.
        assert!(
            spans(
                "step 1 of 11 (1 done)",
                40,
                &[link("1", "https://a.example")]
            )
            .is_empty(),
            "a single character identifies nothing"
        );
        assert!(
            spans("that is okay", 40, &[link("ok", "https://a.example")]).is_empty(),
            "two characters identify nothing"
        );
        assert!(
            spans("the repository", 40, &[link("repo", "https://a.example")]).is_empty(),
            "a word fragment is not the link"
        );
        // The same text standing alone is a real match.
        let found = spans("the repo here", 40, &[link("repo", "https://a.example")]);
        assert_eq!(found.len(), 1, "{found:?}");
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
    fn an_advertised_target_wins_over_a_bare_match() {
        // A pane that wrapped a visible URL in OSC 8 pointing somewhere else
        // means it; the row text is not the authority.
        let found = spans(
            "https://example.com/shown",
            60,
            &[link(
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
            &[link("the AoE repo", "https://example.com/a")],
        );
        assert_eq!(
            found
                .iter()
                .map(|s| s.uri.as_str())
                .collect::<std::collections::HashSet<_>>(),
            ["https://example.com/a", "https://example.com/b"]
                .into_iter()
                .collect()
        );
    }
}

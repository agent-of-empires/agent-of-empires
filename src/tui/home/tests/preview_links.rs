//! A hyperlink in the preview keeps its OSC 8 target. Neither the vt100 grid
//! the live path renders from nor a ratatui cell can carry one, so the targets
//! ride alongside the text and are re-anchored to the row that shows them; a
//! plain click on that row then opens the target (#3735).

use super::*;
use crate::tmux::osc8::PaneLink;
use crate::tui::home::PreviewTextView;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};

const PANE: Rect = Rect {
    x: 2,
    y: 3,
    width: 40,
    height: 4,
};

fn link(text: &str, uri: &str) -> PaneLink {
    PaneLink {
        text: text.to_string(),
        uri: uri.to_string(),
    }
}

/// Stage the preview text view and cache the render path would have set, so
/// the click handler maps a screen cell onto a content line the same way.
fn stage(env: &mut TestEnv, lines: &[&str], links: Vec<PaneLink>) {
    let text: Text<'static> = lines.iter().map(|l| Line::from(l.to_string())).collect();
    let total_lines = text.lines.len();
    env.view.preview_cache.parsed_text = Some(text);
    env.view.preview_cache.links = links;
    env.view.preview_area = PANE;
    env.view.preview_text_view = PreviewTextView {
        pane: PANE,
        first_line: 0,
        total_lines,
    };
}

#[test]
#[serial]
fn click_on_a_link_whose_text_hides_the_target_resolves_it() {
    let mut env = create_test_env_empty();
    stage(
        &mut env,
        &["see the AoE repo now"],
        vec![link("the AoE repo", "https://example.com/aoe")],
    );

    // Columns 4..16 of the row carry the link; `see ` and ` now` do not.
    for col in 4..16 {
        assert_eq!(
            env.view.preview_link_at(PANE.x + col, PANE.y),
            Some("https://example.com/aoe".to_string()),
            "column {col} is inside the link"
        );
    }
    for col in [0, 3, 16, 19] {
        assert_eq!(
            env.view.preview_link_at(PANE.x + col, PANE.y),
            None,
            "column {col} is outside the link"
        );
    }
}

#[test]
#[serial]
fn a_link_resolves_on_whichever_row_shows_it() {
    let mut env = create_test_env_empty();
    stage(
        &mut env,
        &["first", "the AoE repo", "third"],
        vec![link("the AoE repo", "https://example.com/aoe")],
    );

    assert_eq!(
        env.view.preview_link_at(PANE.x, PANE.y + 1),
        Some("https://example.com/aoe".to_string())
    );
    // Rows whose text no longer holds the link are inert, even though the
    // pane still remembers the target.
    assert_eq!(env.view.preview_link_at(PANE.x, PANE.y), None);
    assert_eq!(env.view.preview_link_at(PANE.x, PANE.y + 2), None);
}

#[test]
#[serial]
fn clicks_off_the_text_or_with_no_links_resolve_nothing() {
    let mut env = create_test_env_empty();
    stage(&mut env, &["the AoE repo"], vec![]);
    assert_eq!(env.view.preview_link_at(PANE.x, PANE.y), None);

    stage(
        &mut env,
        &["the AoE repo"],
        vec![link("the AoE repo", "https://example.com/aoe")],
    );
    // Outside the pane rect, and on a row past the painted content.
    assert_eq!(env.view.preview_link_at(PANE.x - 1, PANE.y), None);
    assert_eq!(env.view.preview_link_at(PANE.x, PANE.y + 1), None);
}

#[test]
#[serial]
fn link_columns_are_underlined_so_the_text_reads_as_a_link() {
    // The host terminal has no OSC 8 and no URL to match on here, so nothing
    // else marks this text as clickable.
    let mut env = create_test_env_empty();
    stage(
        &mut env,
        &["see the AoE repo now"],
        vec![link("the AoE repo", "https://example.com/aoe")],
    );
    let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 60, 10));
    env.view.paint_preview_links(&mut buf);

    let underlined: Vec<u16> = (0..60)
        .filter(|col| {
            buf[(*col, PANE.y)]
                .modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        })
        .collect();
    assert_eq!(
        underlined,
        (PANE.x + 4..PANE.x + 16).collect::<Vec<_>>(),
        "only the link's own columns are underlined"
    );
    assert!(
        (0..60).all(|col| !buf[(col, PANE.y + 1)]
            .modifier
            .contains(ratatui::style::Modifier::UNDERLINED)),
        "a row with no link text is untouched"
    );
}

#[test]
#[serial]
fn status_flash_shows_then_expires_without_acknowledgement() {
    // Opening a link needs feedback, not a notice the user has to dismiss.
    let mut env = create_test_env_empty();
    assert_eq!(env.view.status_flash_text(), None);

    env.view.flash_status("opened https://example.com/aoe");
    assert_eq!(
        env.view.status_flash_text(),
        Some("opened https://example.com/aoe")
    );
    // Still inside its window, so nothing to clear yet.
    assert!(!env.view.expire_status_flash());

    // Reach the deadline without sleeping through it.
    env.view.status_flash = Some((
        "opened https://example.com/aoe".to_string(),
        std::time::Instant::now(),
    ));
    assert_eq!(env.view.status_flash_text(), None, "window has closed");
    assert!(
        env.view.expire_status_flash(),
        "expiry reports the one repaint that clears the row"
    );
    assert!(
        !env.view.expire_status_flash(),
        "an already-cleared flash asks for no further repaints"
    );
}

#[test]
#[serial]
fn hovering_a_link_reveals_its_target_before_the_click() {
    // The click opens with no confirmation and the pane controls both the
    // visible text and the target, so hover is the user's only look at where a
    // link actually goes.
    let mut env = create_test_env_empty();
    stage(
        &mut env,
        &["see the AoE repo now"],
        vec![link("the AoE repo", "https://example.com/aoe")],
    );

    assert_eq!(env.view.hovered_link(), None);
    assert!(env.view.update_hovered_link(PANE.x + 6, PANE.y), "changed");
    assert_eq!(
        env.view.hovered_link().as_deref(),
        Some("https://example.com/aoe")
    );

    // Resting on the same link reports no change, so the pointer moving within
    // it does not repaint every frame.
    assert!(!env.view.update_hovered_link(PANE.x + 7, PANE.y));

    // Leaving the link clears it rather than leaving a stale target up.
    assert!(env.view.update_hovered_link(PANE.x, PANE.y), "changed");
    assert_eq!(env.view.hovered_link(), None);
}

#[test]
#[serial]
fn capture_frames_carry_their_own_targets_through_the_parse() {
    // With the VT transport off the frame text still holds the sequences, so
    // the cache collects them as it strips them for `ansi-to-tui`.
    let mut env = create_test_env_empty();
    env.view.preview_cache.store_capture(
        "see \x1b]8;;https://example.com/aoe\x1b\\the AoE repo\x1b]8;;\x1b\\ now\n".to_string(),
        "s1".to_string(),
        "aoe_s1".to_string(),
        1,
        (40, 4),
        None,
    );
    env.view.preview_cache.ensure_parsed();

    assert_eq!(
        env.view.preview_cache.links,
        vec![link("the AoE repo", "https://example.com/aoe")]
    );
    let text = env.view.preview_cache.parsed_text.as_ref().unwrap();
    assert_eq!(text.lines[0].to_string(), "see the AoE repo now");

    env.view.preview_area = PANE;
    env.view.preview_text_view = PreviewTextView {
        pane: PANE,
        first_line: 0,
        total_lines: text.lines.len(),
    };
    assert_eq!(
        env.view.preview_link_at(PANE.x + 4, PANE.y),
        Some("https://example.com/aoe".to_string())
    );
}

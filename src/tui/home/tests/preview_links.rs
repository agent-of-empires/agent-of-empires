//! A hyperlink in the preview keeps its OSC 8 target. Neither the vt100 grid nor a ratatui
//! cell can carry one, so targets ride alongside the text and are re-anchored to the row
//! that shows them; a plain click on that row opens the target (#3735).

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
    // The click opens with no confirmation and the pane controls both the visible text and
    // the target, so hover is the user's only look at where a link goes.
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
fn a_generation_change_re_collects_targets_for_an_unchanged_grid() {
    // vt100 strips both sequences, so a pane that reprints the same label against a new
    // target leaves the grid byte-identical: keying on the rendered text would keep serving
    // the old target. Driven through `ensure_parsed` and `collect_links` so the branch under
    // test is the production one.
    let mut env = create_test_env_empty();
    let advertised = "see \x1b]8;;https://example.com/a\x1b\\the docs\x1b]8;;\x1b\\ now\n";
    env.view.preview_cache.store_capture(
        advertised.to_string(),
        "s1".to_string(),
        "aoe_s1".to_string(),
        1,
        (40, 4),
        None,
    );
    env.view.preview_cache.ensure_parsed();
    let painted = env.view.preview_cache.parsed_text.as_ref().unwrap();
    assert_eq!(painted.lines[0].to_string(), "see the docs now");
    let total_lines = painted.lines.len();
    env.view.preview_area = PANE;
    env.view.preview_text_view = PreviewTextView {
        pane: PANE,
        first_line: 0,
        total_lines,
    };
    assert_eq!(
        env.view.preview_link_at(PANE.x + 4, PANE.y).as_deref(),
        Some("https://example.com/a")
    );

    // The pane repoints the label. The visible cells do not move, so `parsed_text` stays
    // valid and only the advertised target changed, which the generation reports.
    env.view.preview_cache.content =
        "see \x1b]8;;https://example.com/b\x1b\\the docs\x1b]8;;\x1b\\ now\n".to_string();
    env.view.preview_cache.links_generation = 99;
    env.view.preview_cache.ensure_parsed();
    assert_eq!(
        env.view.preview_cache.parsed_text.as_ref().unwrap().lines[0].to_string(),
        "see the docs now",
        "the visible cells must be unchanged"
    );
    assert_eq!(
        env.view.preview_link_at(PANE.x + 4, PANE.y).as_deref(),
        Some("https://example.com/b"),
        "a repointed label must resolve to its new target"
    );

    // The pane reprints the label as plain text. Nothing is advertised any
    // more, so the label must stop being actionable.
    env.view.preview_cache.content = "see the docs now\n".to_string();
    env.view.preview_cache.links_generation = 100;
    env.view.preview_cache.ensure_parsed();
    assert_eq!(
        env.view.preview_link_at(PANE.x + 4, PANE.y),
        None,
        "an obsolete target must not survive the frame that dropped it"
    );
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

#[test]
#[serial]
fn a_non_web_target_stays_inert_until_a_prefix_allows_it() {
    // Extraction, the config reload, painting, the click hit-test and the hand-off
    // to the host terminal all follow `session.preview_link_prefixes`.
    use crate::session::config::update_config;
    use ratatui::style::Modifier;
    const VSCODE: &str = "vscode://file/Users/u/notes/a.md:12";
    let mut env = create_test_env_empty();
    let frame = format!(
        "see \x1b]8;;{VSCODE}\x1b\\notes/a.md\x1b]8;;\x1b\\ now\n\
         and \x1b]8;;javascript:alert(1)\x1b\\click me\x1b]8;;\x1b\\ too\n"
    );
    for (prefixes, live) in [
        (vec![], false),
        (vec!["vscode://file/".to_string()], true),
        (vec![], false),
    ] {
        update_config(|c| c.session.preview_link_prefixes = prefixes.clone()).unwrap();
        env.view.try_refresh_from_config_watcher().unwrap();
        env.view.preview_cache = crate::tui::home::PreviewCache::default();
        env.view.preview_cache.store_capture(
            frame.clone(),
            "s1".to_string(),
            "aoe_s1".to_string(),
            1,
            (40, 4),
            None,
        );
        env.view.preview_cache.ensure_parsed();
        let total_lines = env
            .view
            .preview_cache
            .parsed_text
            .as_ref()
            .unwrap()
            .lines
            .len();
        env.view.preview_area = PANE;
        env.view.preview_text_view = PreviewTextView {
            pane: PANE,
            first_line: 0,
            total_lines,
        };
        env.view.hyperlink_cells.lock().unwrap().clear();
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 60, 10));
        env.view.paint_preview_links(&mut buf);
        let underlined =
            |row: u16| (0..60).any(|col| buf[(col, row)].modifier.contains(Modifier::UNDERLINED));

        assert_eq!(underlined(PANE.y), live, "{prefixes:?}: underline");
        assert_eq!(
            env.view.preview_link_at(PANE.x + 4, PANE.y).as_deref(),
            live.then_some(VSCODE),
            "{prefixes:?}: click target"
        );
        assert_eq!(
            !env.view.hyperlink_cells.lock().unwrap().is_empty(),
            live,
            "{prefixes:?}: passed to the host terminal"
        );
        assert!(
            !underlined(PANE.y + 1),
            "{prefixes:?}: javascript underline"
        );
        assert_eq!(env.view.preview_link_at(PANE.x + 4, PANE.y + 1), None);
    }
}

#[cfg(unix)]
#[test]
fn each_pending_opener_is_kept_until_its_own_exit_arrives() {
    use std::os::unix::process::ExitStatusExt;
    let mut env = create_test_env_empty();
    let (first_tx, first_rx) = std::sync::mpsc::channel();
    let (second_tx, second_rx) = std::sync::mpsc::channel();
    env.view
        .opener_exits
        .push(("vscode://file/a".to_string(), first_rx));
    env.view
        .opener_exits
        .push(("vscode://file/b".to_string(), second_rx));
    assert!(!env.view.tick_opener(), "nothing yet");
    assert_eq!(env.view.opener_exits.len(), 2);
    second_tx
        .send(std::process::ExitStatus::from_raw(0))
        .unwrap();
    assert!(!env.view.tick_opener(), "a success leaves the opened flash");
    assert_eq!(env.view.opener_exits.len(), 1);
    assert_eq!(env.view.opener_exits[0].0, "vscode://file/a");
    drop(first_tx);
    assert!(!env.view.tick_opener());
    assert!(env.view.opener_exits.is_empty());
}

#[cfg(unix)]
#[test]
fn every_opener_failure_ready_before_a_poll_is_reported() {
    use crate::tui::home::overlays::opener_failure_feedback;
    use std::os::unix::process::ExitStatusExt;
    let mut env = create_test_env_empty();
    for (url, code) in [("vscode://file/a", 1), ("vscode://file/b", 2)] {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(std::process::ExitStatus::from_raw(code << 8))
            .unwrap();
        env.view.opener_exits.push((url.to_string(), rx));
    }
    let failed = env.view.take_failed_openers();
    assert!(env.view.opener_exits.is_empty());
    let urls: Vec<&str> = failed.iter().map(|(url, _)| url.as_str()).collect();
    assert_eq!(urls, ["vscode://file/a", "vscode://file/b"]);
    assert_eq!(
        opener_failure_feedback(&failed),
        Some((
            "vscode://file/a\nvscode://file/b".to_string(),
            "nothing opened 2 links; copied vscode://file/a vscode://file/b".to_string()
        ))
    );
    let status = failed[0].1;
    assert_eq!(
        opener_failure_feedback(&failed[..1]),
        Some((
            "vscode://file/a".to_string(),
            format!("nothing opened it ({status}); copied vscode://file/a")
        ))
    );
    assert_eq!(opener_failure_feedback(&[]), None);
}

#[test]
#[serial]
fn the_link_policy_reloads_on_its_own() {
    // `refresh_link_policy` is what the main loop runs during live-send, when the
    // full config reload waits.
    use crate::session::config::update_config;
    let mut env = create_test_env_empty();
    update_config(|c| c.session.preview_link_prefixes = vec!["vscode://file/".to_string()])
        .unwrap();
    assert!(env.view.refresh_link_policy());
    assert_eq!(env.view.preview_link_prefixes, ["vscode://file/"]);
    assert!(crate::session::preview_link_openable("vscode://file/x"));
    assert!(!env.view.refresh_link_policy(), "unchanged");
    update_config(|c| c.session.preview_link_prefixes = Vec::new()).unwrap();
    assert!(env.view.refresh_link_policy());
    assert!(env.view.preview_link_prefixes.is_empty());
    assert!(!crate::session::preview_link_openable("vscode://file/x"));
}

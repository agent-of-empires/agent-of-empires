//! Regression coverage for #2301: the list and preview panes must meet
//! on a single shared border in every layout (DESIGN.md invariant). The
//! stacked layout used to draw the list's BOTTOM and preview's TOP as
//! two adjacent horizontal borders, producing a visible doubled seam.

use super::*;
use crate::tui::responsive;
use crate::tui::styles::load_theme;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Row indices whose glyphs are dominated by the horizontal box-drawing
/// char used by ratatui's `BorderType::Rounded`. Includes both outer box
/// borders and any internal horizontal dividers; the caller relies on
/// adjacency to detect a doubled seam, not on the rows being borders
/// exclusively.
fn horizontal_dense_rows(buf: &ratatui::buffer::Buffer) -> Vec<u16> {
    const HORIZONTAL: &str = "─";
    let mut rows = Vec::new();
    for y in 0..buf.area.height {
        let mut count = 0u16;
        for x in 0..buf.area.width {
            if buf[(x, y)].symbol() == HORIZONTAL {
                count += 1;
            }
        }
        // Half the row width is well above the noise floor (title text,
        // status glyphs, sort indicator) yet still catches the sparse
        // horizontal chars in a narrow preview.
        if count >= buf.area.width / 2 {
            rows.push(y);
        }
    }
    rows
}

fn render_home(env: &mut TestEnv, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let theme = load_theme("empire");
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

#[test]
#[serial]
fn stacked_layout_has_single_horizontal_seam() {
    let mut env = create_test_env_with_sessions(2);
    const { assert!(60 < responsive::STACKED_BREAKPOINT) };
    let buf = render_home(&mut env, 60, 40);

    let border_rows = horizontal_dense_rows(&buf);
    assert!(
        border_rows.len() >= 3,
        "expected at least list-top, shared-seam, and preview-bottom rows; got {border_rows:?}"
    );
    for pair in border_rows.windows(2) {
        assert!(
            pair[1] - pair[0] > 1,
            "doubled seam detected: horizontal borders on adjacent rows {} and {} \
             (issue #2301). All horizontal-border rows: {:?}",
            pair[0],
            pair[1],
            border_rows,
        );
    }
}

#[test]
#[serial]
fn side_by_side_layout_preserves_horizontal_borders() {
    let mut env = create_test_env_with_sessions(2);
    const { assert!(120 >= responsive::STACKED_BREAKPOINT) };
    let buf = render_home(&mut env, 120, 40);

    let border_rows = horizontal_dense_rows(&buf);
    assert!(
        !border_rows.is_empty(),
        "side-by-side must still draw horizontal borders; got {border_rows:?}"
    );
    // The single-shared-separator invariant is a global rule, not a
    // stacked-only rule: if the enum threading ever grows an asymmetric
    // path that re-doubles a seam in side-by-side, this catches it.
    for pair in border_rows.windows(2) {
        assert!(
            pair[1] - pair[0] > 1,
            "unexpected horizontal seam doubling in side-by-side between rows {} and {}; \
             all border rows: {:?}",
            pair[0],
            pair[1],
            border_rows,
        );
    }
}

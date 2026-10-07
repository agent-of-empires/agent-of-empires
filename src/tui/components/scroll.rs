//! Scroll calculation for lists with "N more above/below" indicators.

pub struct ScrollLayout {
    pub scroll_offset: usize,
    pub list_visible: usize,
    pub has_more_above: bool,
    pub has_more_below: bool,
}

/// Calculate scroll offset and visible item count for a list that shows
/// "[N more above]" / "[N more below]" indicator lines when items overflow.
///
/// The indicators themselves consume 1 line each, reducing the space available
/// for actual items. This function handles the resulting dependency correctly
/// and suppresses indicators when `visible_height <= 1`.
pub fn calculate_scroll(total: usize, cursor: usize, visible_height: usize) -> ScrollLayout {
    let scroll_offset = if total <= visible_height || visible_height == 0 {
        0
    } else {
        let first_page = visible_height.saturating_sub(1);
        if cursor < first_page {
            0
        } else {
            let mid_page = visible_height.saturating_sub(2).max(1);
            let raw_offset = cursor + 1 - mid_page;
            let last_page = visible_height.saturating_sub(1);
            let max_offset = total.saturating_sub(last_page);
            raw_offset.min(max_offset)
        }
    };

    let has_more_above = scroll_offset > 0;
    let items_from_offset = total.saturating_sub(scroll_offset);

    // When visible_height <= 1, suppress indicators entirely to ensure at
    // least the selected item is shown.
    let (mut list_visible, mut has_more_above, mut has_more_below) = if visible_height <= 1 {
        (items_from_offset.min(visible_height), false, false)
    } else {
        let available = if has_more_above {
            visible_height - 1
        } else {
            visible_height
        };
        if items_from_offset > available {
            (available.saturating_sub(1), has_more_above, true)
        } else {
            (items_from_offset.min(available), has_more_above, false)
        }
    };

    // When visible_height is very small (e.g., 2), both indicators can
    // consume all available space. Drop indicators to guarantee at least
    // 1 item is visible.
    if list_visible == 0 && total > 0 && visible_height > 0 {
        if has_more_below {
            has_more_below = false;
            list_visible = 1;
        }
        if list_visible == 0 && has_more_above {
            has_more_above = false;
            list_visible = 1;
        }
    }

    ScrollLayout {
        scroll_offset,
        list_visible,
        has_more_above,
        has_more_below,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculate_scroll_cases() {
        // (total, cursor, height) -> (offset, visible, more above, more below).
        // Each indicator costs a line; height <= 1 suppresses both, and at
        // height 2 an indicator yields so at least one item shows.
        let cases = [
            ((3, 0, 10), (0, 3, false, false)),
            ((10, 0, 5), (0, 4, false, true)),
            ((10, 5, 5), (3, 3, true, true)),
            ((10, 9, 5), (6, 4, true, false)),
            ((5, 3, 1), (3, 1, false, false)),
            ((5, 0, 0), (0, 0, false, false)),
            ((0, 0, 10), (0, 0, false, false)),
            // Off-by-one regression: item[6] is hidden, so "more below" shows.
            ((7, 4, 5), (2, 3, true, true)),
            ((10, 5, 2), (5, 1, true, false)),
        ];
        for ((total, cursor, height), want) in cases {
            let s = calculate_scroll(total, cursor, height);
            assert_eq!(
                (
                    s.scroll_offset,
                    s.list_visible,
                    s.has_more_above,
                    s.has_more_below
                ),
                want,
                "total={total} cursor={cursor} height={height}"
            );
        }
    }
}

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
    fn all_items_fit() {
        let s = calculate_scroll(3, 0, 10);
        assert_eq!(s.scroll_offset, 0);
        assert_eq!(s.list_visible, 3);
        assert!(!s.has_more_above);
        assert!(!s.has_more_below);
    }

    #[test]
    fn cursor_at_top_with_overflow() {
        let s = calculate_scroll(10, 0, 5);
        assert_eq!(s.scroll_offset, 0);
        assert!(!s.has_more_above);
        assert!(s.has_more_below);
        // 5 lines - 1 for below indicator = 4 visible items
        assert_eq!(s.list_visible, 4);
    }

    #[test]
    fn cursor_in_middle() {
        let s = calculate_scroll(10, 5, 5);
        assert!(s.scroll_offset > 0);
        assert!(s.has_more_above);
        assert!(s.has_more_below);
        // 5 lines - 1 above - 1 below = 3 visible items
        assert_eq!(s.list_visible, 3);
    }

    #[test]
    fn cursor_at_bottom() {
        let s = calculate_scroll(10, 9, 5);
        assert!(s.scroll_offset > 0);
        assert!(s.has_more_above);
        assert!(!s.has_more_below);
        // 5 lines - 1 above = 4 visible items
        assert_eq!(s.list_visible, 4);
    }

    #[test]
    fn visible_height_one_suppresses_indicators() {
        let s = calculate_scroll(5, 3, 1);
        assert_eq!(s.list_visible, 1);
        assert!(!s.has_more_above);
        assert!(!s.has_more_below);
    }

    #[test]
    fn visible_height_zero() {
        let s = calculate_scroll(5, 0, 0);
        assert_eq!(s.scroll_offset, 0);
        assert_eq!(s.list_visible, 0);
        assert!(!s.has_more_above);
        assert!(!s.has_more_below);
    }

    #[test]
    fn empty_list() {
        let s = calculate_scroll(0, 0, 10);
        assert_eq!(s.scroll_offset, 0);
        assert_eq!(s.list_visible, 0);
        assert!(!s.has_more_above);
        assert!(!s.has_more_below);
    }

    #[test]
    fn off_by_one_regression() {
        // total=7, visible_height=5, cursor=4: item[6] must be accounted for
        let s = calculate_scroll(7, 4, 5);
        let shown = s.scroll_offset + s.list_visible;
        let hidden_below = 7_usize.saturating_sub(shown);
        if hidden_below > 0 {
            assert!(s.has_more_below, "hidden items must show below indicator");
        }
    }

    #[test]
    fn visible_height_two_shows_at_least_one_item() {
        // With height=2, both indicators would consume all space.
        // Must suppress indicators to show at least 1 item.
        let s = calculate_scroll(10, 5, 2);
        assert!(
            s.list_visible >= 1,
            "must show at least 1 item, got list_visible={}",
            s.list_visible
        );
    }
}

//! Pair a hunk's unified line list into side-by-side rows.

use crate::git::diff::{DiffHunk, DiffLine};
use similar::ChangeTag;

/// One row of a split diff. `left` is the old side, `right` the new side;
/// either is `None` where that side has no line for this row.
pub struct SplitRow<'a> {
    pub left: Option<&'a DiffLine>,
    pub right: Option<&'a DiffLine>,
}

fn flush<'a>(
    rows: &mut Vec<SplitRow<'a>>,
    dels: &mut Vec<&'a DiffLine>,
    adds: &mut Vec<&'a DiffLine>,
) {
    let n = dels.len().max(adds.len());
    for k in 0..n {
        rows.push(SplitRow {
            left: dels.get(k).copied(),
            right: adds.get(k).copied(),
        });
    }
    dels.clear();
    adds.clear();
}

/// Pair `hunk.lines` into side-by-side rows. A maximal run of non-equal
/// lines is one change block: its deletions and additions are zipped
/// index-by-index, surplus lines get a `None` placeholder opposite them.
pub fn build_split_rows(hunk: &DiffHunk) -> Vec<SplitRow<'_>> {
    let mut rows: Vec<SplitRow> = Vec::new();
    let mut dels: Vec<&DiffLine> = Vec::new();
    let mut adds: Vec<&DiffLine> = Vec::new();

    for line in &hunk.lines {
        match line.tag {
            ChangeTag::Equal => {
                flush(&mut rows, &mut dels, &mut adds);
                rows.push(SplitRow {
                    left: Some(line),
                    right: Some(line),
                });
            }
            ChangeTag::Delete => dels.push(line),
            ChangeTag::Insert => adds.push(line),
        }
    }
    flush(&mut rows, &mut dels, &mut adds);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{DiffHunk, DiffLine};

    fn dl(tag: ChangeTag, old: Option<usize>, new: Option<usize>, content: &str) -> DiffLine {
        DiffLine {
            tag,
            old_line_num: old,
            new_line_num: new,
            content: content.to_string(),
        }
    }

    fn hunk(lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            lines,
        }
    }

    #[test]
    fn build_split_rows_pairs_changes_and_pads_the_shorter_side() {
        use ChangeTag::{Delete, Equal, Insert};
        // Context on both sides; a delete pairs with the insert beside it; an
        // uneven block pads the shorter side with None.
        let h = hunk(vec![
            dl(Equal, Some(1), Some(1), "ctx"),
            dl(Delete, Some(2), None, "old"),
            dl(Insert, None, Some(2), "new a"),
            dl(Insert, None, Some(3), "new b"),
        ]);
        let rows: Vec<(Option<&str>, Option<&str>)> = build_split_rows(&h)
            .iter()
            .map(|r| {
                (
                    r.left.map(|l| l.content.as_str()),
                    r.right.map(|l| l.content.as_str()),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (Some("ctx"), Some("ctx")),
                (Some("old"), Some("new a")),
                (None, Some("new b")),
            ]
        );
    }
}

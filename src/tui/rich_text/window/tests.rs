use super::*;
use crate::tui::rich_text::{disk::DiskSource, glyph, Sink};
use pulldown_cmark::disk::Spool;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Paragraph, Widget},
};
use std::sync::{atomic::AtomicBool, Arc};

#[test]
fn evicted_windows_preserve_graphemes_copy_and_repeated_line_anchors() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let store = Store::new(Arc::clone(&cancellation)).unwrap();
    let spool = Spool::new(1, cancellation).unwrap();
    let mut document = Document::new(DiskSource {
        store: &store,
        spool: &spool,
    })
    .unwrap();
    let red = Style::default().fg(Color::Red);
    let blue = Style::default().fg(Color::Blue);
    let cluster = format!("e{}", "\u{301}".repeat(9000));
    store
        .with_transaction(|_| {
            document.prefix(Some(("› ", "  ", blue)))?;
            document.blank_lines(70_001)?;
            document.prefix(None)?;
            document.text(spool.append(b"b\x01")?, red)?;
            document.text(spool.append("ﾞ".as_bytes())?, red)?;
            document.end_line()?;
            document.text(spool.append(b"e")?, red)?;
            document.text(spool.append(&cluster.as_bytes()[1..])?, blue)?;
            document.end_line()?;
            document.text(
                spool.append(format!("{}👩‍👩‍👧‍👧TAIL", "a".repeat(8191)).as_bytes())?,
                blue,
            )?;
            document.end_line()?;
            glyph::index(&document, || true)
        })
        .unwrap();
    document.seal().unwrap();
    for width in [1, 2, 80, 2] {
        let index = store
            .with_transaction(|_| Index::build(&document, width, || true))
            .unwrap();
        let line = index
            .locate(Anchor {
                line: 70_000,
                byte: 0,
            })
            .unwrap();
        assert_eq!(index.row(line).unwrap().line, 70_000);
        let hidden = index
            .locate(Anchor {
                line: 70_001,
                byte: 0,
            })
            .unwrap();
        if width == 2 {
            let text = index
                .window(&document, hidden..hidden + 1, || true)
                .unwrap();
            let rect = Rect::new(0, 0, 2, 1);
            let mut buffer = Buffer::empty(rect);
            Paragraph::new(text).render(rect, &mut buffer);
            assert_eq!(buffer[(0, 0)].symbol(), "b");
            assert_eq!(buffer[(1, 0)].symbol(), "ﾞ");
            let mut selected = Vec::new();
            assert!(index
                .copy_selection(
                    &document,
                    Selection {
                        start: (0, hidden),
                        end: (1, hidden)
                    },
                    &mut selected,
                    || true
                )
                .unwrap());
            assert_eq!(selected, "bﾞ".as_bytes());
        }
        let row = index
            .locate(Anchor {
                line: 70_002,
                byte: 0,
            })
            .unwrap();
        let text = index.window(&document, row..row + 1, || true).unwrap();
        let span = text.lines[0].spans.first().unwrap();
        assert_eq!(span.content, cluster);
        assert_eq!(span.style, red);
        let last = index.row(index.len() - 1).unwrap();
        assert_eq!(last.line, 70_003);
    }
    let mut copied = Vec::new();
    copy_all(&document, &mut copied, || true).unwrap();
    let copied = String::from_utf8(copied).unwrap();
    assert!(copied.starts_with("› \n  \n"));
    assert!(copied.contains(&format!("b\x01ﾞ\n{cluster}\n")));
    assert!(copied.ends_with("👩‍👩‍👧‍👧TAIL"));
    assert_eq!(
        copy_all(&document, &mut io::sink(), || false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Interrupted
    );
}

#[test]
fn retained_tabs_keep_source_bytes_across_wrapped_continuations_and_eviction() {
    let cancellation = Arc::new(AtomicBool::new(false));
    let store = Store::new(Arc::clone(&cancellation)).unwrap();
    let spool = Spool::new(1, cancellation).unwrap();
    let mut document = Document::new(DiskSource {
        store: &store,
        spool: &spool,
    })
    .unwrap();
    store
        .with_transaction(|_| {
            document.text(spool.append(b"a\tZ")?, Style::default())?;
            document.end_line()?;
            document.prefix(Some(("› ", "  ", Style::default())))?;
            document.text(spool.append("界\tQ".as_bytes())?, Style::default())?;
            document.end_line()?;
            glyph::index(&document, || true)
        })
        .unwrap();
    document.seal().unwrap();
    for width in [2, 1, 12, 2] {
        let index = store
            .with_transaction(|_| Index::build(&document, width, || true))
            .unwrap();
        let painted = index.window(&document, 0..index.len(), || true).unwrap();
        assert!(painted
            .lines
            .iter()
            .all(|line| !line.to_string().contains('\t')));
        if width == 2 {
            assert_eq!(painted.lines[0].to_string(), "a ");
            assert_eq!(painted.lines[1].to_string(), "  ");
            assert_eq!(painted.lines[2].to_string(), "Z");
            assert_eq!(index.locate(Anchor { line: 0, byte: 1 }).unwrap(), 0);
            let mut copied = Vec::new();
            assert!(index
                .copy_selection(
                    &document,
                    Selection {
                        start: (0, 0),
                        end: (1, 2),
                    },
                    &mut copied,
                    || true
                )
                .unwrap());
            assert_eq!(copied, b"a\t\n\nZ");
            let mut continuation = Vec::new();
            assert!(!index
                .copy_selection(
                    &document,
                    Selection {
                        start: (0, 1),
                        end: (1, 1),
                    },
                    &mut continuation,
                    || true
                )
                .unwrap());
            assert!(continuation.is_empty());
        }
        if width == 12 {
            assert_eq!(painted.lines[0].to_string(), "a   Z");
            assert_eq!(painted.lines[1].to_string(), "› 界    Q");
        }
    }
    let mut all = Vec::new();
    copy_all(&document, &mut all, || true).unwrap();
    assert_eq!(all, "a\tZ\n› 界\tQ".as_bytes());
}

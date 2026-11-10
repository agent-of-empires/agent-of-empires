# Third-party notices

Agent of Empires is MIT licensed (see `LICENSE`). It also contains a small
amount of code derived from third-party projects under their own terms, listed
here as those licenses require.

## herdr

- Project: <https://github.com/herdrdev/herdr>
- License: Apache License 2.0, full text in [`licenses/Apache-2.0.txt`](licenses/Apache-2.0.txt)
- Used in: `src/tui/hyperlink.rs`

The OSC 8 hyperlink state machine in `src/tui/hyperlink.rs` is derived from
herdr's `src/protocol/render_ansi.rs`: emitting a sequence only when the target
changes between cells, resetting to a known hyperlink state at the start of
every frame, and stripping control bytes from a target before writing it.

Modifications: herdr blits whole frames from its own wire format, in which each
cell carries an index into a per-frame hyperlink table, and it owns the output
writer outright. The version here wraps ratatui's `CrosstermBackend` and
resolves a cell's target through a side map keyed by position, because aoe's
grid comes from `vt100`, which does not model hyperlinks per cell. The
surrounding backend, the map, and the tests are original.

## pulldown-cmark 0.13.6

- Project: <https://github.com/raphlinus/pulldown-cmark>
- License: MIT, full text in [`vendor/pulldown-cmark/LICENSE`](vendor/pulldown-cmark/LICENSE)
- Used in: `vendor/pulldown-cmark`

Modifications: shared memory-backed and disk-backed input, parser storage,
reference dictionaries, and text normalization preserve the parser's grammar.
The disk backend shares the application's SQLite dependency.

The disk-backed Unicode folding table derives from Sean McArthur's
`unicase` work. Its full MIT copyright and permission notice is shipped in
[`licenses/unicase-MIT.txt`](licenses/unicase-MIT.txt).

## unicode-width 0.2.2

- Project: <https://github.com/unicode-rs/unicode-width>
- License: MIT or Apache-2.0, used under MIT; full text in
  [`vendor/unicode-width/LICENSE-MIT`](vendor/unicode-width/LICENSE-MIT)
- Copyright: [`vendor/unicode-width/COPYRIGHT`](vendor/unicode-width/COPYRIGHT)
- Used in: `vendor/unicode-width`

Modifications: expose the existing reverse width calculation as an incremental
accumulator shared by contiguous strings and disk-backed grapheme clusters.

## ansi-to-tui 8.0.1

- Project: <https://github.com/ratatui/ansi-to-tui>
- License: MIT, full text in [`licenses/ansi-to-tui-MIT.txt`](licenses/ansi-to-tui-MIT.txt)
- Used in: `src/tui/rich_text/ansi.rs`

Modifications: the escape grammar and style transitions operate on contiguous
or paged byte sources and emit through the shared rich-text sink.

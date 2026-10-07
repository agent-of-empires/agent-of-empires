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

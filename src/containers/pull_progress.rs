//! Progress of a running image pull, read from the runtime's output as it is written.

use std::collections::HashSet;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PullPhase {
    Downloading,
    Unpacking,
}

/// One layer's status line from a runtime's pull output.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LayerUpdate {
    pub(crate) id: String,
    pub(crate) done: bool,
}

/// Non-TTY `docker pull` prints `<12-hex id>: <status>` on stdout as each layer changes
/// (`tests/fixtures/pull-progress`). Done means downloaded, so the unpacking phase covers the
/// `Pull complete` lines that follow the last download. Unrecognised lines are `None`.
///
/// Docker's classic image store prints only `Already exists` for a cached layer, with no
/// `Pulling fs layer` (moby v24 `distribution/xfer/download.go`), so there the counts cover
/// only the layers the pull fetches.
///
/// Assumes every layer is announced before any finishes, as both captures show; no Docker store
/// orders this (each layer's announce and fetch run concurrently), so a later announcement would
/// find the phase already latched at Unpacking.
pub(crate) fn docker_layer(line: &str) -> Option<LayerUpdate> {
    let (id, status) = line.split_once(": ")?;
    if id.len() != 12 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let done = match status.trim_end() {
        "Pulling fs layer" => false,
        "Download complete" | "Already exists" | "Pull complete" => true,
        _ => return None,
    };
    Some(LayerUpdate {
        id: id.to_string(),
        done,
    })
}

/// A running pull's progress. The layer counts only grow, and the phase only moves forward.
/// Only announced layers count: Docker also marks config and manifest blobs done.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    phase: Option<PullPhase>,
    known: HashSet<String>,
    done: HashSet<String>,
    /// Done before announced: Docker's containerd store can report a cached layer before its
    /// `Pulling fs layer` (moby v29.5.2 `daemon/containerd/image_pull.go:149` vs `:198`).
    done_unannounced: HashSet<String>,
}

/// What `image-status` reports for a running pull; counts are absent until a layer line parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ProgressSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<PullPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers_done: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers_known: Option<u32>,
}

impl Progress {
    pub(crate) fn apply(&mut self, LayerUpdate { id, done }: LayerUpdate) {
        // Decided by the output, never by when a status read happens. This assumes Docker
        // announces every layer before any finishes (see `docker_layer`), so a done line that
        // finishes the known set is the last; an announcement never moves the phase.
        let phase = if !done {
            if self.done_unannounced.remove(&id) {
                self.done.insert(id.clone());
            }
            self.known.insert(id);
            PullPhase::Downloading
        } else {
            if self.known.contains(&id) {
                self.done.insert(id);
            } else {
                self.done_unannounced.insert(id);
            }
            if self.done.len() == self.known.len() && !self.known.is_empty() {
                PullPhase::Unpacking
            } else {
                PullPhase::Downloading
            }
        };
        self.phase = self.phase.max(Some(phase));
    }

    pub(crate) fn snapshot(&self) -> ProgressSnapshot {
        let count = |set: &HashSet<String>| {
            (!self.known.is_empty()).then(|| u32::try_from(set.len()).unwrap_or(u32::MAX))
        };
        ProgressSnapshot {
            phase: self.phase,
            layers_done: count(&self.done),
            layers_known: count(&self.known),
        }
    }
}

/// Longer than any progress line a parser reads; a longer line is dropped, not buffered.
const MAX_LINE: usize = 4096;

/// Splits one stream's chunks into lines. `\r` also ends a line, for runtimes that redraw.
#[derive(Debug, Default)]
pub(crate) struct LineBuffer {
    partial: Vec<u8>,
    /// Set once a line passes [`MAX_LINE`]; its bytes are skipped through the next delimiter.
    overflowed: bool,
}

impl LineBuffer {
    pub(crate) fn push(&mut self, bytes: &[u8], mut on_line: impl FnMut(&str)) {
        for &byte in bytes {
            if byte == b'\n' || byte == b'\r' {
                if !self.partial.is_empty() {
                    on_line(&String::from_utf8_lossy(&self.partial));
                }
                self.partial.clear();
                self.overflowed = false;
            } else if self.partial.len() == MAX_LINE {
                self.partial.clear();
                self.overflowed = true;
            } else if !self.overflowed {
                self.partial.push(byte);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_line_is_dropped_through_its_delimiter_and_later_lines_still_arrive() {
        let mut buffer = LineBuffer::default();
        let mut lines = Vec::new();
        // Split so the overflow and the delimiter arrive in different chunks.
        let huge = vec![b'x'; 4 * MAX_LINE];
        for chunk in huge.chunks(1000) {
            buffer.push(chunk, |line| lines.push(line.to_string()));
            assert!(
                buffer.partial.len() <= MAX_LINE,
                "the partial line is bounded"
            );
        }
        buffer.push(b"tail of the long line\nabc: Pull complete\n", |line| {
            lines.push(line.to_string())
        });
        assert_eq!(lines, ["abc: Pull complete"]);
    }

    fn layer(id: &str, done: bool) -> LayerUpdate {
        LayerUpdate {
            id: id.into(),
            done,
        }
    }

    /// A layer reported done before its announcement counts once announced; an id never
    /// announced stays uncounted, and the late announcement alone doesn't start unpacking.
    #[test]
    fn a_layer_done_before_its_announcement_counts_once_announced() {
        use PullPhase::*;
        let snap = |phase, done, known| ProgressSnapshot {
            phase: Some(phase),
            layers_done: Some(done),
            layers_known: Some(known),
        };
        let cases: Vec<(&str, Vec<LayerUpdate>, ProgressSnapshot)> = vec![
            (
                "cached layer reported early",
                vec![layer("a", false), layer("b", true), layer("b", false)],
                snap(Downloading, 1, 2),
            ),
            (
                "repeated early report",
                vec![layer("b", true), layer("b", true), layer("b", false)],
                snap(Downloading, 1, 1),
            ),
            (
                "its own later done line",
                vec![layer("b", true), layer("b", false), layer("b", true)],
                snap(Unpacking, 1, 1),
            ),
            (
                "never announced",
                vec![layer("a", false), layer("c", true)],
                snap(Downloading, 0, 1),
            ),
        ];
        for (case, events, expected) in cases {
            let mut progress = Progress::default();
            for event in events {
                progress.apply(event);
                assert!(progress.done.is_subset(&progress.known), "{case}");
            }
            assert_eq!(progress.snapshot(), expected, "{case}");
        }
    }

    #[test]
    fn counts_only_grow_and_the_phase_only_moves_forward() {
        use PullPhase::*;
        let snap = |phase, done, known| ProgressSnapshot {
            phase,
            layers_done: done,
            layers_known: known,
        };
        // Each step applies its events, then reads.
        let steps: Vec<(Vec<LayerUpdate>, ProgressSnapshot)> = vec![
            (vec![], snap(None, None, None)),
            (vec![layer("z", true)], snap(Some(Downloading), None, None)),
            (
                vec![layer("a", false), layer("b", false), layer("a", false)],
                snap(Some(Downloading), Some(0), Some(2)),
            ),
            // A done id that was never announced is not a layer, and a repeat counts once.
            (
                vec![layer("c", true), layer("a", true), layer("a", true)],
                snap(Some(Downloading), Some(1), Some(2)),
            ),
            (
                vec![layer("b", true), layer("b", false)],
                snap(Some(Unpacking), Some(2), Some(2)),
            ),
            // A layer found after every known one finished grows the count, not back the phase.
            (
                vec![layer("d", false)],
                snap(Some(Unpacking), Some(2), Some(3)),
            ),
        ];
        let mut progress = Progress::default();
        for (i, (events, expected)) in steps.into_iter().enumerate() {
            for event in events {
                progress.apply(event);
            }
            assert_eq!(progress.snapshot(), expected, "step {i}");
        }

        // A read between lines changes nothing, so both give the same answer. A layer announced
        // after the known set finished still latches Unpacking, because the phase never goes
        // back. The captured transcripts announce every layer before any finishes (the test
        // below); that order is assumed, not guaranteed (see `docker_layer`).
        let lines = || [layer("x", false), layer("x", true), layer("y", false)];
        let (mut polled, mut unpolled) = (Progress::default(), Progress::default());
        for update in lines() {
            polled.apply(update);
            polled.snapshot();
        }
        for update in lines() {
            unpolled.apply(update);
        }
        let expected = snap(Some(Unpacking), Some(1), Some(2));
        assert_eq!(polled.snapshot(), expected, "polled");
        assert_eq!(unpolled.snapshot(), expected, "unpolled");
    }

    #[test]
    fn lines_split_across_chunks_and_on_carriage_returns() {
        let mut buffer = LineBuffer::default();
        let mut lines = Vec::new();
        for chunk in [&b"one\ntw"[..], b"o\r\rthr", b"ee\n", b"tail"] {
            buffer.push(chunk, |line| lines.push(line.to_string()));
        }
        assert_eq!(lines, ["one", "two", "three"], "a partial line waits");
    }

    #[test]
    fn each_docker_line_reads_as_its_layer_or_none() {
        let cases: &[(&str, Option<(&str, bool)>)] = &[
            (
                "6458fc480f83: Pulling fs layer",
                Some(("6458fc480f83", false)),
            ),
            (
                "6458fc480f83: Download complete",
                Some(("6458fc480f83", true)),
            ),
            ("2bdb40ec27b2: Already exists", Some(("2bdb40ec27b2", true))),
            ("6458fc480f83: Pull complete", Some(("6458fc480f83", true))),
            ("6458fc480f83: Waiting", None),
            ("3.12-bookworm: Pulling from library/python", None),
            ("6458fc480f8: Pull complete", None),
            ("6458FC480F83: Pull complete", None),
            (
                "Status: Downloaded newer image for python:3.12-bookworm",
                None,
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(
                docker_layer(line),
                expected.map(|(id, done)| layer(id, done)),
                "{line:?}"
            );
        }
    }

    const CANCEL: &str =
        include_str!("../../tests/fixtures/pull-progress/docker-29.5.2-cancel.txt");
    const RETRY: &str = include_str!("../../tests/fixtures/pull-progress/docker-29.5.2-retry.txt");

    fn transcript(fixture: &str) -> impl Iterator<Item = &str> {
        fixture.lines().filter(|line| !line.starts_with('#'))
    }

    fn replay(fixture: &str, mut each: impl FnMut(&str, &mut Progress)) -> Progress {
        let mut progress = Progress::default();
        for line in transcript(fixture) {
            if let Some(update) = docker_layer(line) {
                progress.apply(update);
            }
            assert!(progress.done.is_subset(&progress.known), "{line}");
            each(line, &mut progress);
        }
        progress
    }

    /// The unpacking latch is safe only if no layer is announced after one has finished.
    #[test]
    fn docker_announces_every_layer_before_any_finishes() {
        for (name, fixture) in [("cancel", CANCEL), ("retry", RETRY)] {
            let mut announced = HashSet::new();
            let mut first_done: Option<&str> = None;
            for line in transcript(fixture) {
                match docker_layer(line) {
                    Some(LayerUpdate { id, done: false }) => {
                        assert!(
                            first_done.is_none(),
                            "{name}: {line:?} announced after {first_done:?}"
                        );
                        announced.insert(id);
                    }
                    Some(LayerUpdate { id, done: true }) if announced.contains(&id) => {
                        first_done.get_or_insert(line);
                    }
                    _ => {}
                }
            }
            assert!(
                !announced.is_empty() && first_done.is_some(),
                "{name}: the transcript announces and finishes layers"
            );
        }
    }

    #[test]
    fn docker_transcripts_count_layers_unpack_after_the_last_download_and_resume() {
        use PullPhase::*;
        let cancel = replay(CANCEL, |_, _| {});
        let at_kill = cancel.snapshot();
        assert_eq!(
            (at_kill.phase, at_kill.layers_done, at_kill.layers_known),
            (Some(Downloading), Some(1), Some(7)),
            "one of seven layers had downloaded when the pull was killed"
        );

        let mut first_done = None;
        let mut unpacking_lines = Vec::new();
        let retry = replay(RETRY, |line, progress| {
            let snapshot = progress.snapshot();
            if first_done.is_none() && snapshot.layers_done > Some(0) {
                first_done = Some((line.to_string(), snapshot.layers_done));
                assert!(progress.done.is_superset(&cancel.done), "{line}");
            }
            if snapshot.phase == Some(Unpacking) {
                unpacking_lines.push(line.to_string());
            }
        });
        let (line, done) = first_done.expect("the retry counts a layer");
        assert!(line.ends_with("Already exists"), "{line}");
        assert!(done >= at_kill.layers_done, "{line}");
        // Unpacking starts at the last download, so the last four extractions show it.
        assert_eq!(
            unpacking_lines.first().map(String::as_str),
            Some("a31a98848e2c: Download complete")
        );
        assert_eq!(
            unpacking_lines
                .iter()
                .filter(|line| line.ends_with("Pull complete"))
                .count(),
            4,
            "{unpacking_lines:?}"
        );
        let end = retry.snapshot();
        assert_eq!(
            (end.phase, end.layers_done, end.layers_known),
            (Some(Unpacking), Some(7), Some(7))
        );
        // Blobs Docker reports done but never announces are not layers.
        for (name, progress) in [("cancel", &cancel), ("retry", &retry)] {
            for blob in ["8ae3f2b88215", "8d4d29ee957b"] {
                assert!(!progress.known.contains(blob), "{name}: {blob}");
            }
        }
    }
}

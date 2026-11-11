//! Idempotency and adapter-sequence ordering, applied before the reducer.

use std::collections::{BTreeMap, HashSet, VecDeque};

/// Remembered idempotency keys per session.
pub const MAX_SEEN_KEYS: usize = 4096;

/// Tailer-emitted frames waiting for their anchor.
pub const MAX_HELD: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("more than {MAX_HELD} frames are waiting on an adapter sequence")]
pub struct HoldOverflow;

/// What an arriving frame carries, as far as ordering is concerned.
pub struct Arrival<'a> {
    /// `None` for a frame with no `_meta.aoe`.
    pub key: Option<&'a str>,
    pub after_adapter_sequence: Option<u64>,
}

/// Adapter frames are numbered from 1 in arrival order. A frame carrying
/// `after_adapter_sequence` is tailer-emitted and takes no number.
#[derive(Debug)]
pub struct ArrivalOrder<T> {
    adapter_frames: u64,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    held: BTreeMap<u64, Vec<T>>,
    held_len: usize,
}

impl<T> Default for ArrivalOrder<T> {
    fn default() -> Self {
        Self {
            adapter_frames: 0,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            held: BTreeMap::new(),
            held_len: 0,
        }
    }
}

impl<T> ArrivalOrder<T> {
    /// A new native session starts a fresh key space, and frames held for the
    /// old one must not reach it. The adapter frame count is unchanged: the
    /// adapter process is the same.
    pub fn session_reset(&mut self) {
        self.seen.clear();
        self.seen_order.clear();
        self.held.clear();
        self.held_len = 0;
    }

    /// Numbers an adapter frame core is not delivering, so the count keeps up
    /// with the proxy's. Frames its number releases are discarded.
    pub fn count_frame(&mut self) {
        self.adapter_frames += 1;
        let frames = self.adapter_frames;
        let kept = self.held.split_off(&(frames + 1));
        self.held_len = kept.values().map(Vec::len).sum();
        self.held = kept;
    }

    /// Frames still waiting on an anchor, as `(anchor, frame)`.
    pub fn drain_held(&mut self) -> Vec<(u64, T)> {
        self.held_len = 0;
        std::mem::take(&mut self.held)
            .into_iter()
            .flat_map(|(anchor, items)| items.into_iter().map(move |item| (anchor, item)))
            .collect()
    }

    /// Whether another anchored frame would overflow the hold buffer.
    pub fn held_full(&self) -> bool {
        self.held_len >= MAX_HELD
    }

    /// The frames to deliver now, in order: a duplicate delivers nothing, a
    /// frame whose anchor has not arrived is held, and an adapter frame is
    /// followed by every held frame its number releases. A duplicate adapter
    /// frame still takes its number; a duplicate anchored frame takes none.
    pub fn arrive(&mut self, arrival: Arrival<'_>, item: T) -> Result<Vec<T>, HoldOverflow> {
        let duplicate = arrival.key.is_some_and(|key| !self.remember(key));
        match arrival.after_adapter_sequence {
            Some(_) if duplicate => Ok(Vec::new()),
            Some(anchor) if anchor > self.adapter_frames => {
                if self.held_len >= MAX_HELD {
                    return Err(HoldOverflow);
                }
                self.held.entry(anchor).or_default().push(item);
                self.held_len += 1;
                Ok(Vec::new())
            }
            Some(_) => Ok(vec![item]),
            None => {
                self.adapter_frames += 1;
                let mut out = if duplicate { Vec::new() } else { vec![item] };
                out.extend(self.release_due());
                Ok(out)
            }
        }
    }

    /// Numbers `n` adapter frames that core never saw, for example old-session
    /// frames dropped at a reset, and returns the held frames they release.
    pub fn skip_frames(&mut self, n: u64) -> Vec<T> {
        self.adapter_frames += n;
        self.release_due()
    }

    fn release_due(&mut self) -> Vec<T> {
        let due: Vec<u64> = self
            .held
            .range(..=self.adapter_frames)
            .map(|(anchor, _)| *anchor)
            .collect();
        let mut out = Vec::new();
        for anchor in due {
            if let Some(items) = self.held.remove(&anchor) {
                self.held_len -= items.len();
                out.extend(items);
            }
        }
        out
    }

    /// `false` for a key already seen.
    fn remember(&mut self, key: &str) -> bool {
        if !self.seen.insert(key.to_string()) {
            return false;
        }
        self.seen_order.push_back(key.to_string());
        if self.seen_order.len() > MAX_SEEN_KEYS {
            if let Some(oldest) = self.seen_order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Arrival<'static> {
        Arrival {
            key: None,
            after_adapter_sequence: None,
        }
    }

    fn keyed(key: &str) -> Arrival<'_> {
        Arrival {
            key: Some(key),
            after_adapter_sequence: None,
        }
    }

    fn after(key: &str, n: u64) -> Arrival<'_> {
        Arrival {
            key: Some(key),
            after_adapter_sequence: Some(n),
        }
    }

    #[test]
    fn duplicate_key_is_a_no_op() {
        let mut o = ArrivalOrder::default();
        assert_eq!(o.arrive(keyed("k"), "first").unwrap(), ["first"]);
        assert!(o.arrive(keyed("k"), "again").unwrap().is_empty());
        assert_eq!(o.arrive(plain(), "plain").unwrap(), ["plain"]);
    }

    #[test]
    fn out_of_order_event_waits_for_its_anchor() {
        let mut o = ArrivalOrder::default();
        assert_eq!(o.arrive(plain(), "f1").unwrap(), ["f1"]);
        assert!(o.arrive(after("t1", 3), "t1").unwrap().is_empty());
        assert!(o.arrive(after("t2", 2), "t2").unwrap().is_empty());
        assert_eq!(o.arrive(plain(), "f2").unwrap(), ["f2", "t2"]);
        assert_eq!(o.arrive(plain(), "f3").unwrap(), ["f3", "t1"]);
        // The anchor has passed, so it delivers at once and takes no number.
        assert_eq!(o.arrive(after("t3", 1), "t3").unwrap(), ["t3"]);
        assert_eq!(o.arrive(plain(), "f4").unwrap(), ["f4"]);
    }

    #[test]
    fn a_duplicate_adapter_frame_still_takes_its_number() {
        let mut o = ArrivalOrder::default();
        assert_eq!(o.arrive(keyed("k"), "f1").unwrap(), ["f1"]);
        assert!(o.arrive(keyed("k"), "dup").unwrap().is_empty());
        // The duplicate was frame 2, so a tailer anchored there is not held.
        assert_eq!(o.arrive(after("t", 2), "t").unwrap(), ["t"]);

        // A tailer held for the duplicate's number is released by it.
        let mut o = ArrivalOrder::default();
        o.arrive(keyed("k"), "f1").unwrap();
        assert!(o.arrive(after("t", 2), "t").unwrap().is_empty());
        assert_eq!(o.arrive(keyed("k"), "dup").unwrap(), ["t"]);

        // An anchored duplicate takes no number.
        let mut o = ArrivalOrder::default();
        o.arrive(plain(), "f1").unwrap();
        o.arrive(after("t", 1), "t").unwrap();
        assert!(o.arrive(after("t", 1), "again").unwrap().is_empty());
        assert!(
            o.arrive(after("n", 2), "n").unwrap().is_empty(),
            "still frame 1"
        );
    }

    #[test]
    fn tailer_frames_take_no_sequence_number() {
        let mut o = ArrivalOrder::default();
        o.arrive(plain(), "f1").unwrap();
        o.arrive(after("t1", 1), "t1").unwrap();
        o.arrive(after("t2", 1), "t2").unwrap();
        // Frame 2 is the next adapter frame, despite two tailer frames.
        assert!(o.arrive(after("t3", 2), "t3").unwrap().is_empty());
        assert_eq!(o.arrive(plain(), "f2").unwrap(), ["f2", "t3"]);
    }

    #[test]
    fn session_reset_drops_held_frames_and_keys_but_not_the_frame_count() {
        let mut o = ArrivalOrder::default();
        o.arrive(keyed("k"), "f1").unwrap();
        o.arrive(plain(), "f2").unwrap();
        assert!(o.arrive(after("t", 5), "t").unwrap().is_empty());
        o.session_reset();
        assert_eq!(
            o.arrive(keyed("k"), "again").unwrap(),
            ["again"],
            "keys forgotten"
        );
        // Frames 3 and 4 never release the old held frame; the count carried on.
        assert_eq!(o.arrive(plain(), "f4").unwrap(), ["f4"]);
        assert_eq!(o.arrive(after("n", 3), "n").unwrap(), ["n"], "count kept");
        assert!(!o.held_full());
    }

    #[test]
    fn count_frame_numbers_a_frame_without_delivering_it() {
        let mut o = ArrivalOrder::default();
        assert!(o.arrive(after("t", 2), "t").unwrap().is_empty());
        o.count_frame();
        o.count_frame();
        assert!(o.drain_held().is_empty(), "released frames are discarded");
        assert_eq!(o.arrive(after("n", 2), "n").unwrap(), ["n"], "count kept");
    }

    #[test]
    fn skipped_frames_take_their_numbers_and_release_held_ones() {
        let mut o = ArrivalOrder::default();
        assert!(o.arrive(plain(), "f1").unwrap().len() == 1);
        assert!(o.arrive(after("t", 3), "t").unwrap().is_empty());
        assert_eq!(o.skip_frames(2), ["t"]);
        assert_eq!(o.arrive(after("n", 3), "n").unwrap(), ["n"], "count kept");
    }

    #[test]
    fn hold_and_key_memory_are_bounded() {
        let mut o = ArrivalOrder::default();
        for i in 0..MAX_HELD {
            o.arrive(after(&format!("k{i}"), u64::MAX), i).unwrap();
        }
        assert_eq!(o.arrive(after("over", u64::MAX), 0), Err(HoldOverflow));

        let mut o = ArrivalOrder::default();
        for i in 0..=MAX_SEEN_KEYS {
            o.arrive(keyed(&format!("k{i}")), i).unwrap();
        }
        assert_eq!(
            o.arrive(keyed("k0"), 0).unwrap(),
            [0],
            "oldest key forgotten"
        );
    }
}

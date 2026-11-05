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

    /// The frames to deliver now, in order: a duplicate yields none, a frame
    /// whose anchor has not arrived is held, and an adapter frame is followed
    /// by every held frame its number releases.
    pub fn arrive(&mut self, arrival: Arrival<'_>, item: T) -> Result<Vec<T>, HoldOverflow> {
        if let Some(key) = arrival.key {
            if !self.remember(key) {
                return Ok(Vec::new());
            }
        }
        match arrival.after_adapter_sequence {
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
                let mut out = vec![item];
                let due: Vec<u64> = self
                    .held
                    .range(..=self.adapter_frames)
                    .map(|(anchor, _)| *anchor)
                    .collect();
                for anchor in due {
                    if let Some(items) = self.held.remove(&anchor) {
                        self.held_len -= items.len();
                        out.extend(items);
                    }
                }
                Ok(out)
            }
        }
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

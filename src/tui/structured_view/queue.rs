//! Server-owned prompt-queue mirror for the TUI structured view.
//!
//! The prompt queue's source of truth is the daemon
//! (`/api/sessions/{id}/queue`, see
//! `docs/development/server-side-prompt-queue.md`): a follow-up queued behind a
//! busy turn survives a client reload and drains server-side even with no
//! client attached. This module is the TUI's read model of that queue, a
//! snapshot refreshed from the daemon at the turn edge and on (re)connect, plus
//! optimistic edits that the next refresh reconciles. The batching and
//! `/clear`-boundary split policy that used to live here moved server-side
//! (`session_service::queue_drain_batch`), so the TUI no longer drains locally.

use crate::acp::state::QueuedPromptEntry;

/// Local mirror of the daemon-owned prompt queue, ordered by ascending `seq`.
#[derive(Debug, Default)]
pub struct QueueMirror {
    items: Vec<QueuedPromptEntry>,
}

impl QueueMirror {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Text of the entry at `index` (0 = oldest / front), or `None` when out
    /// of range. Used by the composer's ArrowUp/ArrowDown recall to load a
    /// queued prompt back for editing.
    pub fn text_at(&self, index: usize) -> Option<&str> {
        self.items.get(index).map(|e| e.text.as_str())
    }

    /// Stable server id of the entry at `index`, for edit-by-id, or `None`
    /// when out of range.
    pub fn id_at(&self, index: usize) -> Option<&str> {
        self.items.get(index).map(|e| e.id.as_str())
    }

    /// Position of the entry with `id`, or `None` if it is not in the mirror.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|e| e.id == id)
    }

    /// Replace the whole mirror with a fresh daemon snapshot, keeping the
    /// ascending-`seq` order the queue drains in.
    pub fn set_snapshot(&mut self, mut entries: Vec<QueuedPromptEntry>) {
        entries.sort_by_key(|e| e.seq);
        self.items = entries;
    }

    /// Optimistically replace a queued entry's text after an edit POST; a
    /// no-op if the id already drained out of the mirror.
    pub fn set_text(&mut self, id: &str, text: &str) {
        if let Some(slot) = self.items.iter_mut().find(|e| e.id == id) {
            slot.text = text.to_string();
        }
    }

    /// Drop every entry (after a clear POST succeeds).
    pub fn clear(&mut self) {
        self.items.clear();
    }

    #[cfg(test)]
    pub fn entries(&self) -> &[QueuedPromptEntry] {
        &self.items
    }

    /// Append a synthetic entry for tests, assigning a stable id/seq so
    /// recall-by-id and snapshot reconciliation can be exercised without a
    /// daemon.
    #[cfg(test)]
    pub fn push(&mut self, text: String) {
        let seq = self.items.len() as u64;
        self.items.push(QueuedPromptEntry {
            id: format!("test-{seq}"),
            seq,
            text,
            attachments: Vec::new(),
            created_at: String::new(),
            origin_device: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror(items: &[&str]) -> QueueMirror {
        let mut q = QueueMirror::default();
        for it in items {
            q.push((*it).to_string());
        }
        q
    }

    #[test]
    fn text_and_id_accessors_bound_check() {
        let q = mirror(&["a", "b"]);
        assert_eq!(q.len(), 2);
        assert_eq!(q.text_at(0), Some("a"));
        assert_eq!(q.text_at(1), Some("b"));
        assert_eq!(q.text_at(2), None);
        assert_eq!(q.id_at(0), Some("test-0"));
        assert_eq!(q.index_of("test-1"), Some(1));
        assert_eq!(q.index_of("nope"), None);
    }

    #[test]
    fn set_snapshot_sorts_by_seq() {
        let mut q = QueueMirror::default();
        let entry = |id: &str, seq: u64, text: &str| QueuedPromptEntry {
            id: id.into(),
            seq,
            text: text.into(),
            attachments: Vec::new(),
            created_at: String::new(),
            origin_device: None,
        };
        q.set_snapshot(vec![entry("b", 5, "second"), entry("a", 2, "first")]);
        assert_eq!(q.text_at(0), Some("first"));
        assert_eq!(q.text_at(1), Some("second"));
    }

    #[test]
    fn set_text_edits_by_id_or_no_ops() {
        let mut q = mirror(&["a", "b"]);
        q.set_text("test-1", "B");
        assert_eq!(q.text_at(1), Some("B"));
        q.set_text("gone", "x"); // no-op, already drained
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn clear_empties_the_mirror() {
        let mut q = mirror(&["x", "y"]);
        q.clear();
        assert!(q.is_empty());
    }
}

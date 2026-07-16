//! Client-side prompt queue for the TUI structured view composer.
//!
//! Mirrors the web composer's queue (`web/src/hooks/useAcp.ts`): the
//! user can keep typing while a turn is in flight, the prompts park here,
//! and they drain when the agent goes idle. Like the web, this is pure
//! local state, never persisted and never round-tripped through the
//! daemon. The view layer owns the busy-detection and the actual POST;
//! this module is the pure data structure plus the drain-batching policy,
//! so it can be unit-tested without a terminal or a daemon.

/// FIFO of queued prompt strings awaiting a drain.
#[derive(Debug, Default)]
pub struct PromptQueue {
    items: Vec<String>,
}

impl PromptQueue {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.items.iter()
    }

    /// Append a prompt to the back of the queue.
    pub fn push(&mut self, text: String) {
        self.items.push(text);
    }

    /// Borrow the queued entry at `index` (0 = oldest / front), or `None`
    /// when out of range. Used by the composer's ArrowUp/ArrowDown recall
    /// to load a queued prompt back for editing.
    pub fn get(&self, index: usize) -> Option<&String> {
        self.items.get(index)
    }

    /// Replace the entry at `index` in place, preserving its queue
    /// position. Returns `false` when the index is out of range, e.g. the
    /// browsed entry drained between recall and submit; the caller then
    /// treats the edited text as a fresh prompt.
    pub fn replace(&mut self, index: usize, text: String) -> bool {
        match self.items.get_mut(index) {
            Some(slot) => {
                *slot = text;
                true
            }
            None => false,
        }
    }

    /// Drop the whole queue (the user hit the clear-queue hotkey).
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Remove the first `n` entries. Called only after the drain POST for
    /// those entries succeeds, so a failed send leaves the queue intact.
    pub fn drop_front(&mut self, n: usize) {
        let n = n.min(self.items.len());
        self.items.drain(..n);
    }

    /// Peek the next batch to drain, returning the prompt text to POST
    /// and the number of queued entries it consumes. Does NOT mutate the
    /// queue: the caller removes the consumed entries via [`drop_front`]
    /// only once the POST succeeds, so a network failure never silently
    /// drops prompts. `None` when the queue is empty.
    ///
    /// The leading run of non-boundary entries is joined with a blank
    /// line into one follow-up. A leading clear-command entry (`/clear` /
    /// `/new`) fires alone so it is never glued to adjacent prose, which
    /// would corrupt slash-command parsing (matches the web's clear-alias
    /// sub-batching, #1356).
    pub fn next_batch(&self) -> Option<(String, usize)> {
        let head = self.items.first()?;
        if is_clear_boundary(head) {
            return Some((head.clone(), 1));
        }
        let run: Vec<&String> = self
            .items
            .iter()
            .take_while(|entry| !is_clear_boundary(entry))
            .collect();
        let count = run.len();
        let text = run
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n\n");
        Some((text, count))
    }
}

/// Whether a queued entry is a context-clearing command that must drain
/// alone rather than be joined into a combined batch. The TUI structured view can
/// attach to any agent, so this is the union of the per-agent clear
/// aliases (`/clear` for claude, `/new` for codex / opencode); the daemon
/// `/about` surface does not carry the active agent's alias list, so the
/// union is the safe v1 boundary. Leading whitespace is tolerated and a
/// trailing argument (`/clear --hard`) still counts.
fn is_clear_boundary(text: &str) -> bool {
    let trimmed = text.trim_start();
    for alias in ["/clear", "/new"] {
        if trimmed == alias {
            return true;
        }
        if let Some(rest) = trimmed.strip_prefix(alias) {
            if rest.starts_with(char::is_whitespace) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(items: &[&str]) -> PromptQueue {
        let mut q = PromptQueue::default();
        for it in items {
            q.push((*it).to_string());
        }
        q
    }

    #[test]
    fn empty_queue_has_no_batch() {
        let q = PromptQueue::default();
        assert!(q.is_empty());
        assert_eq!(q.next_batch(), None);
    }

    #[test]
    fn batch_joins_the_whole_queue_with_blank_lines() {
        let q = queue(&["alpha", "beta", "gamma"]);
        let (text, count) = q.next_batch().expect("batch");
        assert_eq!(text, "alpha\n\nbeta\n\ngamma");
        assert_eq!(count, 3);
    }

    #[test]
    fn batch_fires_a_leading_clear_alias_alone() {
        let q = queue(&["/clear", "do the thing", "and this"]);
        let (text, count) = q.next_batch().expect("batch");
        assert_eq!(text, "/clear");
        assert_eq!(count, 1);
    }

    #[test]
    fn batch_stops_at_a_clear_boundary() {
        let q = queue(&["one", "two", "/new", "three"]);
        let (text, count) = q.next_batch().expect("batch");
        assert_eq!(text, "one\n\ntwo");
        assert_eq!(count, 2);
    }

    #[test]
    fn clear_boundary_tolerates_whitespace_and_arguments() {
        assert!(is_clear_boundary("/clear"));
        assert!(is_clear_boundary("  /clear  "));
        assert!(is_clear_boundary("/clear --hard"));
        assert!(is_clear_boundary("/new fresh"));
        assert!(!is_clear_boundary("/cleart"));
        assert!(!is_clear_boundary("clear"));
        assert!(!is_clear_boundary("tell me about /clear"));
        assert!(!is_clear_boundary(""));
    }

    #[test]
    fn drop_front_removes_consumed_entries_only() {
        let mut q = queue(&["a", "b", "c"]);
        q.drop_front(2);
        let remaining: Vec<&String> = q.iter().collect();
        assert_eq!(remaining, vec!["c"]);
    }

    #[test]
    fn drop_front_saturates_at_len() {
        let mut q = queue(&["only"]);
        q.drop_front(5);
        assert!(q.is_empty());
    }

    #[test]
    fn clear_empties_the_queue() {
        let mut q = queue(&["x", "y"]);
        q.clear();
        assert!(q.is_empty());
    }

    #[test]
    fn get_borrows_by_index_or_none() {
        let q = queue(&["a", "b"]);
        assert_eq!(q.get(0).map(String::as_str), Some("a"));
        assert_eq!(q.get(1).map(String::as_str), Some("b"));
        assert_eq!(q.get(2), None);
    }

    #[test]
    fn replace_edits_in_place_and_reports_hit() {
        let mut q = queue(&["a", "b", "c"]);
        assert!(q.replace(1, "B".to_string()));
        let items: Vec<&String> = q.iter().collect();
        assert_eq!(items, vec!["a", "B", "c"]);
    }

    #[test]
    fn replace_out_of_range_is_a_miss() {
        let mut q = queue(&["only"]);
        assert!(!q.replace(3, "x".to_string()));
        let items: Vec<&String> = q.iter().collect();
        assert_eq!(items, vec!["only"]);
    }
}

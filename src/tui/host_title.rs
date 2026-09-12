//! Host terminal tab title (OSC 0) for the TUI dashboard (#3444).

use std::sync::atomic::{AtomicBool, Ordering};

/// Title restored when the TUI exits, and used when nothing is selected
/// or the user has opted out.
pub const FALLBACK_TITLE: &str = "aoe";

/// Set when this process has written OSC 0, so TerminalGuard can restore
/// [`FALLBACK_TITLE`] on exit without touching tabs we never named.
static DID_EMIT: AtomicBool = AtomicBool::new(false);

pub fn note_emitted() {
    DID_EMIT.store(true, Ordering::Relaxed);
}

pub fn take_emitted() -> bool {
    DID_EMIT.swap(false, Ordering::Relaxed)
}

/// Strip control bytes so a session title cannot inject OSC/CSI into the
/// host stream. Matches the OSC 8 URI sanitizer in [`super::hyperlink`].
pub fn sanitize_title(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Tab string for a selected session: `aoe: {title}`. Empty or missing
/// titles fall back to [`FALLBACK_TITLE`].
pub fn format_tab_title(session_title: Option<&str>) -> String {
    match session_title.map(sanitize_title) {
        Some(title) if !title.is_empty() => format!("{FALLBACK_TITLE}: {title}"),
        _ => FALLBACK_TITLE.to_string(),
    }
}

/// Last OSC 0 payload written, so an unchanged selection does not re-emit.
#[derive(Debug, Default)]
pub struct HostTitleTracker {
    last: Option<String>,
}

impl HostTitleTracker {
    /// Title to write, or `None` if the host already has it.
    ///
    /// Disabled and never written: stay hands-off so we do not overwrite
    /// the terminal's own naming. Disabled after a write: restore
    /// [`FALLBACK_TITLE`] once. After [`Self::invalidate`], the next
    /// enabled sync re-emits even if the string is unchanged (needed
    /// after `tmux attach`, which may have overwritten the host title).
    pub fn sync(&mut self, enabled: bool, session_title: Option<&str>) -> Option<String> {
        let desired = if enabled {
            format_tab_title(session_title)
        } else if self.last.is_some() {
            FALLBACK_TITLE.to_string()
        } else {
            return None;
        };
        if self.last.as_deref() == Some(desired.as_str()) {
            return None;
        }
        self.last = enabled.then(|| desired.clone());
        Some(desired)
    }

    pub fn invalidate(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_session_title_with_aoe_prefix() {
        assert_eq!(format_tab_title(Some("fix auth")), "aoe: fix auth");
    }

    #[test]
    fn empty_or_missing_title_is_bare_aoe() {
        assert_eq!(format_tab_title(None), "aoe");
        assert_eq!(format_tab_title(Some("")), "aoe");
        assert_eq!(format_tab_title(Some("   ")), "aoe");
    }

    #[test]
    fn strips_control_bytes_from_titles() {
        assert_eq!(sanitize_title("fix\x1b]0;pwn\x07 auth"), "fix]0;pwn auth");
        assert_eq!(format_tab_title(Some("ok\nline")), "aoe: okline");
    }

    #[test]
    fn tracker_skips_unchanged_titles() {
        let mut t = HostTitleTracker::default();
        assert_eq!(t.sync(true, Some("one")), Some("aoe: one".to_string()));
        assert_eq!(t.sync(true, Some("one")), None);
        assert_eq!(t.sync(true, Some("two")), Some("aoe: two".to_string()));
    }

    #[test]
    fn tracker_stays_quiet_when_disabled_from_the_start() {
        let mut t = HostTitleTracker::default();
        assert_eq!(t.sync(false, Some("one")), None);
        assert_eq!(t.sync(false, Some("two")), None);
    }

    #[test]
    fn tracker_restores_fallback_once_when_toggled_off() {
        let mut t = HostTitleTracker::default();
        assert_eq!(t.sync(true, Some("one")), Some("aoe: one".to_string()));
        assert_eq!(t.sync(false, Some("one")), Some("aoe".to_string()));
        assert_eq!(t.sync(false, Some("one")), None);
    }

    #[test]
    fn invalidate_forces_a_rewrite() {
        let mut t = HostTitleTracker::default();
        assert_eq!(t.sync(true, Some("one")), Some("aoe: one".to_string()));
        t.invalidate();
        assert_eq!(t.sync(true, Some("one")), Some("aoe: one".to_string()));
    }

    #[test]
    fn group_or_no_selection_uses_fallback_while_enabled() {
        let mut t = HostTitleTracker::default();
        assert_eq!(t.sync(true, None), Some("aoe".to_string()));
        assert_eq!(t.sync(true, None), None);
    }

    #[test]
    fn take_emitted_is_one_shot() {
        let _ = take_emitted();
        assert!(!take_emitted());
        note_emitted();
        assert!(take_emitted());
        assert!(!take_emitted());
    }
}

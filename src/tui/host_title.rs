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
    fn format_tab_title_prefixes_and_strips_control_bytes() {
        for (title, want) in [
            (Some("fix auth"), "aoe: fix auth"),
            (None, "aoe"),
            (Some(""), "aoe"),
            (Some("   "), "aoe"),
            (Some("ok\nline"), "aoe: okline"),
        ] {
            assert_eq!(format_tab_title(title), want, "{title:?}");
        }
        assert_eq!(sanitize_title("fix\x1b]0;pwn\x07 auth"), "fix]0;pwn auth");
    }

    #[test]
    fn tracker_writes_only_changes() {
        // Each step: (enabled, selected title, expected write). `None` as the
        // title is a group row or no selection, which uses the fallback.
        type Step<'a> = (bool, Option<&'a str>, Option<&'a str>);
        let cases: &[(&str, &[Step])] = &[
            (
                "skips unchanged titles",
                &[
                    (true, Some("one"), Some("aoe: one")),
                    (true, Some("one"), None),
                    (true, Some("two"), Some("aoe: two")),
                ],
            ),
            (
                "quiet when disabled from the start",
                &[(false, Some("one"), None), (false, Some("two"), None)],
            ),
            (
                "restores the fallback once when toggled off",
                &[
                    (true, Some("one"), Some("aoe: one")),
                    (false, Some("one"), Some("aoe")),
                    (false, Some("one"), None),
                ],
            ),
            (
                "no selection uses the fallback while enabled",
                &[(true, None, Some("aoe")), (true, None, None)],
            ),
        ];
        for (name, steps) in cases {
            let mut t = HostTitleTracker::default();
            for (enabled, title, want) in *steps {
                assert_eq!(t.sync(*enabled, *title), want.map(str::to_string), "{name}");
            }
        }
        let mut t = HostTitleTracker::default();
        assert_eq!(t.sync(true, Some("one")), Some("aoe: one".to_string()));
        t.invalidate();
        assert_eq!(
            t.sync(true, Some("one")),
            Some("aoe: one".to_string()),
            "invalidate forces a rewrite"
        );
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

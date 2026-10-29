//! Small crate-internal utilities shared across modules.

use std::time::{SystemTime, UNIX_EPOCH};

/// Whether `url` is a plain web URL, the only kind aoe hands to a browser.
///
/// Deliberately narrow: agent output, plugin UI state and pane text are all
/// untrusted, and `javascript:`, `file:` and `data:` must never reach an
/// opener. Scheme comparison is case-insensitive because a URL's scheme is.
/// Mirrors the web `isExternalHttpUrl`.
pub(crate) fn is_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Schemes no `session.preview_link_prefixes` entry opens: their handlers run
/// code or read local data.
const NEVER_OPENED: &[&str] = &["file:", "javascript:", "data:", "vbscript:"];

/// Whether a preview link to `url` may open: an [`is_http_url`] address, or one
/// starting with a `session.preview_link_prefixes` entry, compared
/// case-insensitively. An entry counts only in `scheme://authority` form, and
/// [`NEVER_OPENED`] schemes stay closed whatever is listed.
pub(crate) fn is_openable_link(url: &str, extra_prefixes: &[String]) -> bool {
    if is_http_url(url) {
        return true;
    }
    if NEVER_OPENED.iter().any(|s| starts_with_ignore_case(url, s)) {
        return false;
    }
    extra_prefixes
        .iter()
        .map(|p| p.trim())
        .any(|p| names_an_authority(p) && starts_with_ignore_case(url, p))
}

/// Whether `s` is an RFC 3986 scheme: a letter, then letters, digits, `+`,
/// `-` or `.`.
pub(crate) fn is_uri_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// `scheme://authority/`: an entry names one complete authority, so it opens
/// neither every handler under a scheme nor `vscode://file-other/` for
/// `vscode://file`.
fn names_an_authority(prefix: &str) -> bool {
    prefix.split_once("://").is_some_and(|(scheme, rest)| {
        is_uri_scheme(scheme)
            && rest
                .split_once('/')
                .is_some_and(|(authority, _)| !authority.is_empty())
    })
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// Whether `url` is safe for a plugin-supplied UI link: an [`is_http_url`]
/// address, or a same-origin relative path.
///
/// A plugin cannot know aoe's own host, so a link back into aoe (e.g. a
/// session) must be relative. A leading `//` is rejected because browsers
/// resolve it as a scheme-relative URL to a different host, not a path. A
/// backslash or embedded tab/CR/LF is also rejected: browsers normalize `\`
/// to `/` for special schemes and strip tab/CR/LF anywhere in the string, so
/// e.g. `/\evil.com` or `/\n/evil.com` would otherwise pass this check but
/// resolve to a different origin. Mirrors the web `isAllowedHref`.
pub(crate) fn is_allowed_href(url: &str) -> bool {
    is_http_url(url)
        || (url.starts_with('/')
            && !url.starts_with("//")
            && !url.contains(['\\', '\t', '\r', '\n']))
}

/// `path` with a leading `home` replaced by `~`, only when `path` is `home` or
/// lies under it; a sibling that merely shares a string prefix is unchanged.
pub(crate) fn collapse_home(path: &str, home: &str) -> String {
    let home = match home.trim_end_matches('/') {
        "" => home,
        trimmed => trimmed,
    };
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// [`collapse_home`] against the user's home directory, for display.
pub(crate) fn collapse_tilde(path: &str) -> String {
    match dirs::home_dir() {
        Some(home) => collapse_home(path, &home.to_string_lossy()),
        None => path.to_string(),
    }
}

/// Current Unix time in whole seconds, saturating to 0 if the clock is before
/// the epoch (which should never happen on a sane system).
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whole milliseconds between `t` and the Unix epoch, saturating to 0 if `t`
/// predates the epoch.
pub(crate) fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Current Unix time in whole milliseconds. Wall-clock (not a per-process
/// monotonic), so values are comparable across processes; saturating to 0 if
/// the clock is before the epoch.
pub(crate) fn now_ms() -> u64 {
    system_time_to_ms(SystemTime::now())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimePathRemedy {
    Advice(&'static str),
    Command {
        template: &'static str,
        manual: &'static str,
    },
}

/// Format a refused directory without terminal controls or lossy shell commands.
pub(crate) fn runtime_path_refusal(
    path: &std::path::Path,
    mode: u32,
    cause: &str,
    remedy: RuntimePathRemedy,
) -> String {
    fn append_path(output: &mut String, template: &str, path: impl std::fmt::Display) {
        use std::fmt::Write;
        let mut parts = template.split("{path}");
        output.push_str(parts.next().unwrap_or_default());
        for part in parts {
            write!(output, "{path}{part}").unwrap();
        }
    }

    let mut message = format!(
        "Unsafe runtime directory: {path:?} is {cause} (mode {:04o}). ",
        mode & 0o7777
    );
    match remedy {
        RuntimePathRemedy::Advice(advice) => {
            message.push_str("Resolve this by: ");
            append_path(&mut message, advice, format_args!("{path:?}"));
        }
        RuntimePathRemedy::Command { template, manual } => {
            let text = path.to_str().filter(|text| {
                !text.contains(|c: char| {
                    // Bidi_Control characters are not covered by is_control().
                    c.is_control()
                        || matches!(
                            c,
                            '`' | '\u{061c}'
                                | '\u{200e}'..='\u{200f}'
                                | '\u{202a}'..='\u{202e}'
                                | '\u{2066}'..='\u{2069}'
                        )
                })
            });
            if let Some(text) = text {
                message.push_str("Resolve this by: `");
                append_path(
                    &mut message,
                    template,
                    crate::session::environment::shell_escape(text),
                );
                message.push_str("`, then retry.");
            } else {
                message.push_str(manual);
                message.push_str(
                    " manually, then retry. No copyable shell command is available for this path.",
                );
            }
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn collapse_home_requires_a_separator_after_home() {
        for (path, expect) in [
            ("/home/u", "~"),
            ("/home/u/projects/app", "~/projects/app"),
            ("/home/u/projects/", "~/projects/"),
            ("/home/uextra/not/home", "/home/uextra/not/home"),
            ("/tmp/elsewhere", "/tmp/elsewhere"),
            ("relative/path", "relative/path"),
        ] {
            assert_eq!(collapse_home(path, "/home/u"), expect, "{path}");
            assert_eq!(
                collapse_home(path, "/home/u/"),
                expect,
                "{path} (trailing /)"
            );
        }
    }

    #[test]
    fn system_time_to_ms_converts_and_saturates() {
        assert_eq!(system_time_to_ms(UNIX_EPOCH), 0);
        assert_eq!(
            system_time_to_ms(UNIX_EPOCH + Duration::from_millis(1_500)),
            1_500
        );
        assert_eq!(system_time_to_ms(UNIX_EPOCH - Duration::from_secs(1)), 0);
        let t = SystemTime::now();
        let secs = t.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(system_time_to_ms(t) / 1_000, secs);
    }

    #[test]
    fn is_openable_link_opens_only_web_urls_unless_a_prefix_allows_more() {
        let none: [String; 0] = [];
        assert!(is_openable_link("https://example.com", &none));
        for url in [
            "vscode://file/Users/u/a.md",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            assert!(!is_openable_link(url, &none), "{url} with no prefixes");
        }

        let allowed = [" vscode://file/ ".to_string(), String::new()];
        for url in [
            "vscode://file/Users/u/notes/a.md",
            "vscode://file/Users/u/src/main.rs:12:4",
            "VSCode://File/Users/u/a.md",
            "https://example.com",
        ] {
            assert!(is_openable_link(url, &allowed), "{url}");
        }
        for url in [
            "vscode://vscode.git/clone?url=x",
            "vscode://file-other/path",
            "vscode-insiders://file/Users/u/a.md",
            "file:///Users/u/a.md",
            "javascript:alert(1)",
        ] {
            assert!(!is_openable_link(url, &allowed), "{url}");
        }

        // Entries without a complete authority, and never-opened schemes, are ignored.
        let ignored = [
            "vscode:",
            "vscode://",
            "vscode://file",
            "file://",
            "javascript:",
            "1x://y/",
        ]
        .map(String::from);
        for url in [
            "vscode://vscode.git/clone?url=x",
            "vscode://file/Users/u/a.md",
            "vscode://file-other/path",
            "file:///Users/u/a.command",
            "javascript:alert(1)",
            "1x://y/z",
        ] {
            assert!(!is_openable_link(url, &ignored), "{url}");
        }
    }

    #[test]
    fn is_allowed_href_accepts_only_http_and_same_origin_paths() {
        for url in ["https://example.com", "http://example.com", "/session/xyz"] {
            assert!(is_allowed_href(url), "{url}");
        }
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,evil",
            "//evil.com",
            "//evil.com/path",
            "/\\evil.com",
            "/\t/evil.com",
            "/\r/evil.com",
            "/\n/evil.com",
        ] {
            assert!(!is_allowed_href(url), "{url:?}");
        }
    }
}

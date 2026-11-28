//! The one line the update bar shows. Drawing picks it with [`UpdateBar::pick`] and records
//! it as a [`DrawnBar`]; the bar's keys act on that record, so they only touch the line on screen.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::bindings::{self, ActionId};
use crate::containers::image_pull::PullFailureReason;
use crate::containers::image_update::ImageUpdate;
use crate::containers::pull_progress::{ProgressSnapshot, PullPhase};
use crate::update::UpdateInfo;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum UpdateBar<'a> {
    /// Ctrl+E's prompt, over a running pull.
    CancelPrompt,
    /// A toast; Ctrl+X hides it.
    Status(&'a str),
    /// A sandbox image pull this TUI watched, running or ended.
    Pull(&'a PullLine),
    /// The Daemon's live sandbox line; there is nothing to dismiss.
    Live(&'a str),
    AppUpdate(&'a UpdateInfo),
    ImageUpdate(&'a ImageUpdate),
}

impl<'a> UpdateBar<'a> {
    /// Status first, then a pull, the live line, the AoE update, and the sandbox image, which
    /// stays hidden while a pull runs or its outcome waits, even a hidden one, so its pull key
    /// can't re-arm into a no-op.
    pub(crate) fn pick(
        status: Option<&'a str>,
        pull: Option<&'a PullLine>,
        live: Option<&'a str>,
        app_update: Option<&'a UpdateInfo>,
        image_update: Option<&'a ImageUpdate>,
        pull_in_flight: bool,
    ) -> Option<Self> {
        if let Some(s) = status {
            return Some(Self::Status(s));
        }
        if let Some(line) = pull {
            return Some(Self::Pull(line));
        }
        if let Some(s) = live {
            return Some(Self::Live(s));
        }
        if let Some(info) = app_update {
            return Some(Self::AppUpdate(info));
        }
        image_update
            .filter(|_| !pull_in_flight)
            .map(Self::ImageUpdate)
    }
}

/// The line last drawn, owned so it outlives the frame. A key acts on it only while the
/// state it names is still current; otherwise the key is spent without effect.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DrawnBar {
    CancelPrompt,
    Status(String),
    Pull(DrawnPull),
    Live,
    AppUpdate { version: String },
    ImageUpdate { digest: String },
}

/// A drawn pull line and its id: two outcomes can draw alike, and a key acts only on the one drawn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DrawnPull {
    pub(crate) line: PullLine,
    pub(crate) id: u64,
}

impl DrawnBar {
    /// `pull_id` is the id of the pull line `bar` shows, if it shows one.
    pub(crate) fn drawn(bar: UpdateBar<'_>, pull_id: u64) -> Self {
        match bar {
            UpdateBar::CancelPrompt => Self::CancelPrompt,
            UpdateBar::Status(s) => Self::Status(s.to_string()),
            UpdateBar::Pull(line) => Self::Pull(DrawnPull {
                line: line.clone(),
                id: pull_id,
            }),
            UpdateBar::Live(_) => Self::Live,
            UpdateBar::AppUpdate(info) => Self::AppUpdate {
                version: info.latest_version.clone(),
            },
            UpdateBar::ImageUpdate(update) => Self::ImageUpdate {
                digest: update.remote_digest.clone(),
            },
        }
    }
}

/// A sandbox image pull this TUI watched, as the update bar shows it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PullLine {
    Running(ProgressSnapshot),
    Done,
    Cancelled,
    Failed(PullFailureReason),
    /// The Daemon refuses pulls here; `command` pulls by hand.
    /// `command` is what to run instead, when the TUI knows it.
    Gated {
        command: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PullTone {
    Brand,
    Success,
    Error,
    Plain,
}

impl PullLine {
    pub(crate) fn tone(&self) -> PullTone {
        match self {
            Self::Running(_) => PullTone::Brand,
            Self::Done => PullTone::Success,
            Self::Failed(_) => PullTone::Error,
            Self::Cancelled | Self::Gated { .. } => PullTone::Plain,
        }
    }

    /// The widest form that fits `width` columns. A running pull drops its meter and its
    /// reassurance before its keys; an outcome shortens its message, never its keys.
    pub(crate) fn text(&self, width: u16, strict: bool, spinner: &str) -> String {
        let width = usize::from(width);
        let label = |id| bindings::label(id, strict);
        let hide = label(ActionId::DismissUpdateBanner);
        let update = label(ActionId::Update);
        match self {
            Self::Running(snapshot) => {
                let verb = match snapshot.phase {
                    Some(PullPhase::Unpacking) => "Unpacking",
                    _ => "Downloading",
                };
                let head = format!(" {spinner} {verb} sandbox image…");
                let keys = format!(
                    "  [{hide}] hide  [{}] cancel",
                    label(ActionId::CancelImagePull)
                );
                let safe = "Sessions keep running";
                let (full, short) = match counts(snapshot) {
                    Some((done, known)) => (
                        format!(
                            "{head} {} {done} of {known} layers · {safe}{keys}",
                            meter(done, known)
                        ),
                        format!("{head} {done}/{known}"),
                    ),
                    None => (format!("{head} · {safe}{keys}"), head),
                };
                if full.width() <= width {
                    full
                } else {
                    shorten(&short, &keys, width)
                }
            }
            Self::Done => {
                const SHORTEST: &str = " ✓ Image updated.";
                [
                    " ✓ Sandbox image updated. Applies to new Sessions.",
                    " ✓ Sandbox image updated.",
                    SHORTEST,
                ]
                .into_iter()
                .find(|text| text.width() <= width)
                .map_or_else(|| cut(SHORTEST, width), str::to_string)
            }
            Self::Cancelled => shorten(
                " Update cancelled. Sessions keep the current image.",
                &format!("  [{update}] update  [{hide}] dismiss"),
                width,
            ),
            Self::Failed(reason) => {
                let keys = format!("  [{update}] try again  [{hide}] dismiss");
                let full = format!(" ✕ {}", reason.user_text());
                if full.width() + keys.width() <= width {
                    full + &keys
                } else {
                    shorten(
                        &format!(" ✕ Pull failed: {}", short_reason(*reason)),
                        &keys,
                        width,
                    )
                }
            }
            Self::Gated { command } => {
                gated(command.as_deref(), &format!("  [{hide}] dismiss"), width)
            }
        }
    }
}

/// A failure's reason in a form that fits 80 columns beside the keys.
fn short_reason(reason: PullFailureReason) -> &'static str {
    match reason {
        PullFailureReason::Timeout => "timed out after 10 min",
        PullFailureReason::Network => "no network",
        PullFailureReason::DiskSpace => "disk full",
        PullFailureReason::Auth => "sign in to the registry",
        PullFailureReason::Other => "see aoe logs",
    }
}

/// A hidden running pull, at the end of the key line.
pub(crate) fn hidden_pull_badge(snapshot: &ProgressSnapshot, spinner: &str) -> String {
    match counts(snapshot) {
        Some((done, known)) => format!("{spinner} Image {done}/{known}"),
        None => format!("{spinner} Image"),
    }
}

fn counts(snapshot: &ProgressSnapshot) -> Option<(u32, u32)> {
    match (snapshot.layers_done, snapshot.layers_known) {
        (Some(done), Some(known)) if known > 0 => Some((done, known)),
        _ => None,
    }
}

/// Six cells, rounded to the nearest. In `u64`, so the Daemon's counts can't overflow it.
fn meter(done: u32, known: u32) -> String {
    const CELLS: u64 = 6;
    let (done, known) = (u64::from(done.min(known)), u64::from(known));
    let filled = ((done * CELLS * 2 + known) / (known * 2)) as usize;
    "▰".repeat(filled) + &"▱".repeat(CELLS as usize - filled)
}

/// The manual command is the line's only remedy, so it stays whole while it fits: the
/// prefix shortens first, then the dismiss hint goes (its key still works).
fn gated(command: Option<&str>, keys: &str, width: usize) -> String {
    let (long, short) = match command {
        Some(command) => (
            format!(" Sandbox image pulls are off here. Run: {command}"),
            format!(" Pulls are off. Run: {command}"),
        ),
        None => (
            " Sandbox image pulls are off on this Daemon.".to_string(),
            " Pulls are off here.".to_string(),
        ),
    };
    for (message, keys) in [(&long, keys), (&short, keys), (&short, "")] {
        if message.width() + keys.width() <= width {
            return format!("{message}{keys}");
        }
    }
    cut(&short, width)
}

/// `message` then `keys`, cutting the message short with an ellipsis so the keys still fit.
fn shorten(message: &str, keys: &str, width: usize) -> String {
    if message.width() + keys.width() <= width {
        return format!("{message}{keys}");
    }
    if keys.width() + 1 > width {
        // Not even the keys fit, so the line is cut as a whole.
        return cut(&format!("{message}{keys}"), width);
    }
    format!("{}{keys}", cut(message, width - keys.width()))
}

/// `text` within `width` columns, ending in an ellipsis when cut.
fn cut(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let Some(mut room) = width.checked_sub(1) else {
        return String::new();
    };
    let mut kept = String::new();
    for c in text.chars() {
        let Some(left) = room.checked_sub(c.width().unwrap_or(0)) else {
            break;
        };
        room = left;
        kept.push(c);
    }
    kept + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_first_line_that_applies() {
        let info = UpdateInfo {
            available: true,
            current_version: "1.0.0".into(),
            latest_version: "1.1.0".into(),
        };
        let image = ImageUpdate {
            image: "img".into(),
            remote_digest: "sha256:abc".into(),
        };
        let failed = PullLine::Failed(PullFailureReason::Network);
        let (s, p, l, a, i) = (
            Some("copied"),
            Some(&failed),
            Some("moving store"),
            Some(&info),
            Some(&image),
        );
        let cases = [
            ((s, p, l, a, i, true), Some(UpdateBar::Status("copied"))),
            ((None, p, l, a, i, true), Some(UpdateBar::Pull(&failed))),
            (
                (None, None, l, a, i, false),
                Some(UpdateBar::Live("moving store")),
            ),
            (
                (None, None, None, a, i, false),
                Some(UpdateBar::AppUpdate(&info)),
            ),
            (
                (None, None, None, a, i, true),
                Some(UpdateBar::AppUpdate(&info)),
            ),
            (
                (None, None, None, None, i, false),
                Some(UpdateBar::ImageUpdate(&image)),
            ),
            // A hidden pull draws nothing, yet still keeps the image line away.
            ((None, None, None, None, i, true), None),
            (
                (None, None, l, None, i, false),
                Some(UpdateBar::Live("moving store")),
            ),
            ((None, None, None, None, None, false), None),
        ];
        for ((status, pull, live, app, img, pulling), want) in cases {
            assert_eq!(
                UpdateBar::pick(status, pull, live, app, img, pulling),
                want,
                "status={status:?} pull={pull:?} live={live:?} app={} image={} pulling={pulling}",
                app.is_some(),
                img.is_some()
            );
        }
    }

    #[test]
    fn the_pull_line_fits_its_width_and_keeps_its_keys() {
        let snapshot = |phase, done, known| ProgressSnapshot {
            phase,
            layers_done: done,
            layers_known: known,
        };
        let downloading = snapshot(Some(PullPhase::Downloading), Some(3), Some(7));
        let unpacking = snapshot(Some(PullPhase::Unpacking), Some(7), Some(7));
        let gated = PullLine::Gated {
            command: Some("docker pull ghcr.io/example/sandbox:latest".into()),
        };
        let default_gated = PullLine::Gated {
            command: Some("docker pull ghcr.io/agent-of-empires/aoe-sandbox:1.19.0".into()),
        };
        let unknown_gated = PullLine::Gated { command: None };
        for strict in [false, true] {
            let label = |id| bindings::label(id, strict);
            let hide = format!("[{}] hide", label(ActionId::DismissUpdateBanner));
            let cancel = format!("[{}] cancel", label(ActionId::CancelImagePull));
            let dismiss = format!("[{}] dismiss", label(ActionId::DismissUpdateBanner));
            let retry = format!("[{}] try again", label(ActionId::Update));
            let update = format!("[{}] update", label(ActionId::Update));
            // (line, width, the text it shows, the text it drops)
            let cases: Vec<(PullLine, u16, Vec<&str>, Vec<&str>)> = vec![
                (
                    PullLine::Running(downloading),
                    120,
                    vec!["⠹ Downloading sandbox image…", "▰▰▰▱▱▱ 3 of 7 layers", "Sessions keep running", hide.as_str(), cancel.as_str()],
                    vec![],
                ),
                (
                    PullLine::Running(downloading),
                    80,
                    vec!["Downloading sandbox image… 3/7", hide.as_str(), cancel.as_str()],
                    vec!["▰", "Sessions keep running"],
                ),
                // Too narrow for the short form: the heading gives way to an ellipsis, never the keys.
                (
                    PullLine::Running(downloading),
                    50,
                    vec!["⠹ Download", "…", hide.as_str(), cancel.as_str()],
                    vec!["3/7"],
                ),
                (
                    PullLine::Running(ProgressSnapshot::default()),
                    50,
                    vec!["…", hide.as_str(), cancel.as_str()],
                    vec!["Sessions keep running"],
                ),
                (
                    PullLine::Running(unpacking),
                    120,
                    vec!["Unpacking sandbox image…", "▰▰▰▰▰▰ 7 of 7 layers"],
                    vec!["▱"],
                ),
                // A phase a newer Daemon reports reads as the default verb.
                (
                    PullLine::Running(snapshot(Some(PullPhase::Unknown), Some(3), Some(7))),
                    120,
                    vec!["Downloading sandbox image…", "3 of 7 layers"],
                    vec!["Unpacking"],
                ),
                (
                    PullLine::Running(ProgressSnapshot::default()),
                    120,
                    vec!["Downloading sandbox image… · Sessions keep running", hide.as_str(), cancel.as_str()],
                    vec!["layers"],
                ),
                (
                    PullLine::Running(ProgressSnapshot::default()),
                    70,
                    vec!["Downloading sandbox image…", hide.as_str(), cancel.as_str()],
                    vec!["Sessions keep running"],
                ),
                (
                    PullLine::Done,
                    80,
                    vec!["✓ Sandbox image updated. Applies to new Sessions."],
                    vec!["["],
                ),
                // Too narrow for the whole sentence: the first one, never cut mid-word.
                (
                    PullLine::Done,
                    40,
                    vec!["✓ Sandbox image updated."],
                    vec!["Applies", "…"],
                ),
                (
                    PullLine::Done,
                    24,
                    vec!["✓ Image updated."],
                    vec!["Sandbox", "…"],
                ),
                (
                    PullLine::Cancelled,
                    120,
                    vec!["Update cancelled. Sessions keep the current image.", update.as_str(), dismiss.as_str()],
                    vec![],
                ),
                (
                    PullLine::Failed(PullFailureReason::Auth),
                    120,
                    vec!["✕ Couldn't download the sandbox image: the registry needs you to sign in.", retry.as_str(), dismiss.as_str()],
                    vec!["…"],
                ),
                // Too narrow for the sentence: the reason stays, in short.
                (
                    PullLine::Failed(PullFailureReason::Auth),
                    80,
                    vec!["✕ Pull failed: sign in to the registry", retry.as_str(), dismiss.as_str()],
                    vec!["…"],
                ),
                (
                    PullLine::Failed(PullFailureReason::Timeout),
                    80,
                    vec!["✕ Pull failed: timed out after 10 min", retry.as_str(), dismiss.as_str()],
                    vec!["…"],
                ),
                (
                    gated.clone(),
                    120,
                    vec!["Sandbox image pulls are off here. Run: docker pull ghcr.io/example/sandbox:latest", dismiss.as_str()],
                    vec![],
                ),
                // The default image's command stays whole: the prefix shortens, then the hint goes.
                (
                    default_gated.clone(),
                    100,
                    vec![" Pulls are off. Run: docker pull ghcr.io/agent-of-empires/aoe-sandbox:1.19.0", dismiss.as_str()],
                    vec!["…"],
                ),
                (
                    default_gated.clone(),
                    80,
                    vec![" Pulls are off. Run: docker pull ghcr.io/agent-of-empires/aoe-sandbox:1.19.0"],
                    vec!["…", dismiss.as_str()],
                ),
                // No command known: the line says only that pulls are off.
                (
                    unknown_gated.clone(),
                    80,
                    vec!["Sandbox image pulls are off on this Daemon.", dismiss.as_str()],
                    vec!["Run:"],
                ),
                (unknown_gated.clone(), 40, vec!["Pulls are off here."], vec!["Run:"]),
                // Only a command wider than the row is cut.
                (default_gated.clone(), 40, vec![" Pulls are off. Run: docker pull", "…"], vec![]),
                // Narrower than the keys: the whole line is cut to the width.
                (PullLine::Cancelled, 12, vec!["…"], vec![update.as_str()]),
                (PullLine::Done, 12, vec!["✓ Image", "…"], vec!["updated."]),
                (PullLine::Running(downloading), 3, vec!["…"], vec![]),
                (PullLine::Cancelled, 0, vec![], vec!["…"]),
            ];
            for (line, width, shows, drops) in cases {
                let text = line.text(width, strict, "⠹");
                let case = format!("strict={strict} width={width} {line:?}: {text:?}");
                assert!(text.width() <= usize::from(width), "{case}");
                for part in shows {
                    assert!(text.contains(part), "{case} lacks {part:?}");
                }
                for part in drops {
                    assert!(!text.contains(part), "{case} keeps {part:?}");
                }
            }
        }
        assert_eq!(hidden_pull_badge(&downloading, "⠹"), "⠹ Image 3/7");
        assert_eq!(
            hidden_pull_badge(&ProgressSnapshot::default(), "⠹"),
            "⠹ Image"
        );
        assert_eq!(meter(0, 7), "▱▱▱▱▱▱");
        assert_eq!(meter(1, 2), "▰▰▰▱▱▱");
        assert_eq!(meter(u32::MAX, u32::MAX), "▰▰▰▰▰▰");
        assert_eq!(meter(u32::MAX / 2, u32::MAX), "▰▰▰▱▱▱");
    }
}

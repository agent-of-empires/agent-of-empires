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
    Status(String),
    Pull(PullLine),
    Live,
    AppUpdate { version: String },
    ImageUpdate { digest: String },
}

impl From<UpdateBar<'_>> for DrawnBar {
    fn from(bar: UpdateBar<'_>) -> Self {
        match bar {
            UpdateBar::Status(s) => Self::Status(s.to_string()),
            UpdateBar::Pull(line) => Self::Pull(line.clone()),
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
    Gated {
        command: String,
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
                let forms = match counts(snapshot) {
                    Some((done, known)) => [
                        format!(
                            "{head} {} {done} of {known} layers · {safe}{keys}",
                            meter(done, known)
                        ),
                        format!("{head} {done}/{known}{keys}"),
                    ],
                    None => [format!("{head} · {safe}{keys}"), format!("{head}{keys}")],
                };
                let [full, short] = forms;
                if full.width() <= width {
                    full
                } else {
                    short
                }
            }
            Self::Done => " ✓ Sandbox image updated. Applies to new Sessions.".to_string(),
            Self::Cancelled => shorten(
                " Update cancelled. Sessions keep the current image.",
                &format!("  [{update}] update  [{hide}] dismiss"),
                width,
            ),
            Self::Failed(reason) => shorten(
                &format!(" ✕ {}", reason.user_text()),
                &format!("  [{update}] try again  [{hide}] dismiss"),
                width,
            ),
            Self::Gated { command } => shorten(
                &format!(" Sandbox image pulls are off here. Run: {command}"),
                &format!("  [{hide}] dismiss"),
                width,
            ),
        }
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

/// Six cells, rounded to the nearest.
fn meter(done: u32, known: u32) -> String {
    const CELLS: u32 = 6;
    let filled = ((done.min(known) * CELLS * 2 + known) / (known * 2)) as usize;
    "▰".repeat(filled) + &"▱".repeat(CELLS as usize - filled)
}

/// `message` then `keys`, cutting the message short with an ellipsis so the keys still fit.
fn shorten(message: &str, keys: &str, width: usize) -> String {
    if message.width() + keys.width() <= width {
        return format!("{message}{keys}");
    }
    let mut room = width.saturating_sub(keys.width() + 1);
    let mut kept = String::new();
    for c in message.chars() {
        let Some(left) = room.checked_sub(c.width().unwrap_or(0)) else {
            break;
        };
        room = left;
        kept.push(c);
    }
    format!("{kept}…{keys}")
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
            command: "docker pull ghcr.io/example/sandbox:latest".into(),
        };
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
                (
                    PullLine::Running(unpacking),
                    120,
                    vec!["Unpacking sandbox image…", "▰▰▰▰▰▰ 7 of 7 layers"],
                    vec!["▱"],
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
                (
                    PullLine::Failed(PullFailureReason::Auth),
                    80,
                    vec!["✕ Couldn't download", retry.as_str(), dismiss.as_str(), "…"],
                    vec!["sign in"],
                ),
                (
                    gated.clone(),
                    120,
                    vec!["Sandbox image pulls are off here. Run: docker pull ghcr.io/example/sandbox:latest", dismiss.as_str()],
                    vec![],
                ),
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
    }
}

//! The one line the update bar shows. Drawing and the bar's keys both go through
//! [`UpdateBar::pick`], so a key only ever acts on the line on screen.

use crate::containers::image_update::ImageUpdate;
use crate::update::UpdateInfo;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum UpdateBar<'a> {
    /// A toast or a pull's progress; Ctrl+X hides it.
    Status(&'a str),
    /// The Daemon's live sandbox line; there is nothing to dismiss.
    Live(&'a str),
    AppUpdate(&'a UpdateInfo),
    ImageUpdate(&'a ImageUpdate),
}

impl<'a> UpdateBar<'a> {
    /// Status first, then the live line, the AoE update, and the sandbox image, which stays
    /// hidden while its own pull runs so its pull key can't re-arm into a no-op.
    pub(crate) fn pick(
        status: Option<&'a str>,
        live: Option<&'a str>,
        app_update: Option<&'a UpdateInfo>,
        image_update: Option<&'a ImageUpdate>,
        pull_in_flight: bool,
    ) -> Option<Self> {
        if let Some(s) = status {
            return Some(Self::Status(s));
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
        let (s, l, a, i) = (
            Some("pulling img…"),
            Some("moving store"),
            Some(&info),
            Some(&image),
        );
        let cases = [
            ((s, l, a, i, true), Some(UpdateBar::Status("pulling img…"))),
            (
                (None, l, a, i, false),
                Some(UpdateBar::Live("moving store")),
            ),
            ((None, None, a, i, false), Some(UpdateBar::AppUpdate(&info))),
            ((None, None, a, i, true), Some(UpdateBar::AppUpdate(&info))),
            (
                (None, None, None, i, false),
                Some(UpdateBar::ImageUpdate(&image)),
            ),
            ((None, None, None, i, true), None),
            (
                (None, l, None, i, false),
                Some(UpdateBar::Live("moving store")),
            ),
            ((None, None, None, None, false), None),
        ];
        for ((status, live, app, img, pulling), want) in cases {
            assert_eq!(
                UpdateBar::pick(status, live, app, img, pulling),
                want,
                "status={status:?} live={live:?} app={} image={} pulling={pulling}",
                app.is_some(),
                img.is_some()
            );
        }
    }
}

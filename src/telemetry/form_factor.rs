//! Coarse client form-factor classification for the seen ping (issue #1883).
//!
//! The `seen` ping reports that the web dashboard / acp was opened. The
//! snapshot's `os` / `arch` describe the daemon host, not the device the user
//! is looking at, so a phone PWA talking to a Mac daemon was indistinguishable
//! from a desktop tab. The frontend derives one of a **closed set** of coarse
//! classes and sends it; everything outside the set is rejected, never stored,
//! so no user-agent string, screen size, or device model can ride in.
//!
//! The set is deliberately flat (`desktop` / `desktop_pwa` / `mobile` /
//! `mobile_pwa`) rather than two fields, so it maps to a single allowlisted
//! identifier-keyed map at the snapshot and gateway boundary and preserves the
//! joint pwa-by-form-factor distribution.

/// A coarse, allowlisted client class. The only values the daemon will record;
/// anything else is rejected at the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WebClientFormFactor {
    Desktop,
    DesktopPwa,
    Mobile,
    MobilePwa,
}

impl WebClientFormFactor {
    /// Stable wire key. Lowercase identifier so it satisfies the gateway's
    /// map-key allowlist (`^[a-z][a-z0-9_]{0,63}$`) and is safe as a snapshot
    /// map key.
    pub fn key(self) -> &'static str {
        match self {
            WebClientFormFactor::Desktop => "desktop",
            WebClientFormFactor::DesktopPwa => "desktop_pwa",
            WebClientFormFactor::Mobile => "mobile",
            WebClientFormFactor::MobilePwa => "mobile_pwa",
        }
    }

    /// Every class, for iterating the closed set (snapshot map assembly, tests).
    pub const ALL: [WebClientFormFactor; 4] = [
        WebClientFormFactor::Desktop,
        WebClientFormFactor::DesktopPwa,
        WebClientFormFactor::Mobile,
        WebClientFormFactor::MobilePwa,
    ];
}

/// Parse an incoming form-factor string against the closed allowlist. Returns
/// `None` for anything outside the set, so the endpoint can reject it the way
/// it already rejects an unknown `surface` rather than coercing or storing it.
pub fn parse(value: &str) -> Option<WebClientFormFactor> {
    match value {
        "desktop" => Some(WebClientFormFactor::Desktop),
        "desktop_pwa" => Some(WebClientFormFactor::DesktopPwa),
        "mobile" => Some(WebClientFormFactor::Mobile),
        "mobile_pwa" => Some(WebClientFormFactor::MobilePwa),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_allowlisted_class_round_trip() {
        for ff in WebClientFormFactor::ALL {
            assert_eq!(parse(ff.key()), Some(ff), "round-trip failed for {ff:?}");
        }
    }

    #[test]
    fn rejects_anything_outside_the_closed_set() {
        // Empty, unknown labels, a user-agent string, a screen size, and case
        // / separator variants must all be rejected, never coerced.
        for bad in [
            "",
            "tablet",
            "DESKTOP",
            "mobile-pwa",
            "Mozilla/5.0 (iPhone)",
            "1920x1080",
            "desktop_pwa ",
        ] {
            assert_eq!(parse(bad), None, "`{bad}` should not parse");
        }
    }
}

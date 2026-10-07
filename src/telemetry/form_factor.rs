//! Closed set of coarse web client classes; anything else is rejected, never stored.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WebClientFormFactor {
    Desktop,
    DesktopPwa,
    Mobile,
    MobilePwa,
}

impl WebClientFormFactor {
    /// Must satisfy the gateway map-key allowlist `^[a-z][a-z0-9_]{0,63}$`.
    pub fn key(self) -> &'static str {
        match self {
            WebClientFormFactor::Desktop => "desktop",
            WebClientFormFactor::DesktopPwa => "desktop_pwa",
            WebClientFormFactor::Mobile => "mobile",
            WebClientFormFactor::MobilePwa => "mobile_pwa",
        }
    }

    pub const ALL: [WebClientFormFactor; 4] = [
        WebClientFormFactor::Desktop,
        WebClientFormFactor::DesktopPwa,
        WebClientFormFactor::Mobile,
        WebClientFormFactor::MobilePwa,
    ];
}

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
    fn parse_accepts_only_the_closed_set() {
        for ff in WebClientFormFactor::ALL {
            assert_eq!(parse(ff.key()), Some(ff), "round-trip failed for {ff:?}");
        }
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

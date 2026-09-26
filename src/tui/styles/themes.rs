//! Built-in themes and the `Theme` palette struct.

use std::time::Duration;

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use super::palette::color_to_palette;

/// Whether a theme renders against a dark or light surface. Drives the web
/// surface-ramp direction and the fallback syntax highlighter theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeAppearance {
    Dark,
    Light,
}

/// Per-theme syntax-highlighter metadata, in `[syntax]` so renderer knobs stay
/// out of the flat semantic color fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSyntax {
    /// Shiki theme module the web loads. `None` falls back by appearance:
    /// `github-dark` for dark themes, `github-light` for light ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shiki_theme: Option<String>,
}

impl ThemeSyntax {
    fn is_default(&self) -> bool {
        self.shiki_theme.is_none()
    }
}

/// Convert the configured decay duration (minutes) into a `Duration`. `0` gives
/// `Duration::ZERO`, the documented opt-out: every Idle row renders with the
/// static idle look the moment its Stop hook fires.
pub fn idle_decay_window(minutes: u64) -> Duration {
    Duration::from_secs(minutes.saturating_mul(60))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    // Background and borders
    #[serde(with = "hex_color")]
    pub background: Color,
    #[serde(with = "hex_color")]
    pub border: Color,
    #[serde(with = "hex_color")]
    pub terminal_border: Color,
    #[serde(with = "hex_color")]
    pub selection: Color,
    #[serde(with = "hex_color")]
    pub session_selection: Color,

    // Text colors
    #[serde(with = "hex_color")]
    pub title: Color,
    #[serde(with = "hex_color")]
    pub text: Color,
    #[serde(with = "hex_color")]
    pub dimmed: Color,
    #[serde(with = "hex_color")]
    pub hint: Color,

    // Status colors
    #[serde(with = "hex_color")]
    pub running: Color,
    #[serde(with = "hex_color")]
    pub waiting: Color,
    /// Color for a session inside the idle decay window. Held constant for the
    /// whole window so the breathe rattle's pulse stays consistent, then snaps to
    /// `idle`. Sits between `waiting` and `idle` on the attention scale.
    #[serde(with = "hex_color")]
    pub fresh_idle: Color,
    #[serde(with = "hex_color")]
    pub idle: Color,
    /// Color for a session carrying an unread marker, applied to resting rows in
    /// place of the decaying idle color so unread work stands out without being as
    /// loud as Waiting/Error. Gated on `session.unread_indicator`. A TOML omitting
    /// this inherits that theme's own `accent` via `fill_unread_from_accent`.
    #[serde(with = "hex_color")]
    pub unread: Color,
    #[serde(with = "hex_color")]
    pub error: Color,
    #[serde(with = "hex_color")]
    pub terminal_active: Color,

    // UI elements
    #[serde(with = "hex_color")]
    pub group: Color,
    #[serde(with = "hex_color")]
    pub search: Color,
    #[serde(with = "hex_color")]
    pub accent: Color,

    #[serde(with = "hex_color")]
    pub diff_add: Color,
    #[serde(with = "hex_color")]
    pub diff_delete: Color,
    #[serde(with = "hex_color")]
    pub diff_modified: Color,
    #[serde(with = "hex_color")]
    pub diff_header: Color,

    #[serde(with = "hex_color")]
    pub help_key: Color,

    #[serde(with = "hex_color")]
    pub branch: Color,
    #[serde(with = "hex_color")]
    pub sandbox: Color,

    /// Whether the theme is dark or light; absent, the resolver classifies it from
    /// `background` luminance. Per-field `#[serde(default)]` so a partial custom
    /// TOML deserializes to `None` rather than inheriting Empire's `Dark`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appearance: Option<ThemeAppearance>,

    /// Renderer-specific syntax-highlighter overrides. Lives in nested
    /// `[syntax]`; empty by default so custom TOMLs without it round-trip
    /// without a stray empty section.
    #[serde(default, skip_serializing_if = "ThemeSyntax::is_default")]
    pub syntax: ThemeSyntax,
}

#[derive(Debug, Deserialize)]
struct RawThemeDefaults {
    #[serde(with = "hex_color")]
    background: Color,
    #[serde(with = "hex_color")]
    border: Color,
    #[serde(with = "hex_color")]
    terminal_border: Color,
    #[serde(with = "hex_color")]
    selection: Color,
    #[serde(with = "hex_color")]
    session_selection: Color,
    #[serde(with = "hex_color")]
    title: Color,
    #[serde(with = "hex_color")]
    text: Color,
    #[serde(with = "hex_color")]
    dimmed: Color,
    #[serde(with = "hex_color")]
    hint: Color,
    #[serde(with = "hex_color")]
    running: Color,
    #[serde(with = "hex_color")]
    waiting: Color,
    #[serde(with = "hex_color")]
    fresh_idle: Color,
    #[serde(with = "hex_color")]
    idle: Color,
    #[serde(with = "hex_color")]
    unread: Color,
    #[serde(with = "hex_color")]
    error: Color,
    #[serde(with = "hex_color")]
    terminal_active: Color,
    #[serde(with = "hex_color")]
    group: Color,
    #[serde(with = "hex_color")]
    search: Color,
    #[serde(with = "hex_color")]
    accent: Color,
    #[serde(with = "hex_color")]
    diff_add: Color,
    #[serde(with = "hex_color")]
    diff_delete: Color,
    #[serde(with = "hex_color")]
    diff_modified: Color,
    #[serde(with = "hex_color")]
    diff_header: Color,
    #[serde(with = "hex_color")]
    help_key: Color,
    #[serde(with = "hex_color")]
    branch: Color,
    #[serde(with = "hex_color")]
    sandbox: Color,
}

impl From<RawThemeDefaults> for Theme {
    fn from(raw: RawThemeDefaults) -> Self {
        Self {
            background: raw.background,
            border: raw.border,
            terminal_border: raw.terminal_border,
            selection: raw.selection,
            session_selection: raw.session_selection,
            title: raw.title,
            text: raw.text,
            dimmed: raw.dimmed,
            hint: raw.hint,
            running: raw.running,
            waiting: raw.waiting,
            fresh_idle: raw.fresh_idle,
            idle: raw.idle,
            unread: raw.unread,
            error: raw.error,
            terminal_active: raw.terminal_active,
            group: raw.group,
            search: raw.search,
            accent: raw.accent,
            diff_add: raw.diff_add,
            diff_delete: raw.diff_delete,
            diff_modified: raw.diff_modified,
            diff_header: raw.diff_header,
            help_key: raw.help_key,
            branch: raw.branch,
            sandbox: raw.sandbox,
            appearance: None,
            syntax: ThemeSyntax { shiki_theme: None },
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        // Serde calls `Theme::default()` while deserializing partial custom TOMLs,
        // so this must not call `load_theme` or parse `Theme` itself. Parsing the
        // Empire builtin through a raw no-default shape keeps empire.toml the
        // single source for fallback colors.
        let raw: RawThemeDefaults =
            toml::from_str(include_str!("../../../themes/builtin/empire.toml"))
                .expect("embedded empire theme defaults must parse");
        raw.into()
    }
}

impl Theme {
    /// Color for an Idle session, given the time since it went Idle and the
    /// configured decay window: `fresh_idle` inside the window, `idle` past it (or
    /// when age and window are unusable). The pulse holds a constant color; a
    /// continuous lerp under the breathe rattle reads as noisy.
    pub fn idle_color_at_age(&self, age: Option<Duration>, window: Duration) -> Color {
        let Some(age) = age else {
            return self.idle;
        };
        if window.is_zero() || age >= window {
            return self.idle;
        }
        self.fresh_idle
    }

    /// Color for a dormant session: a structured-view worker auto-stopped for
    /// inactivity and resumable. A dim amber, `fresh_idle` pulled halfway toward
    /// `dimmed`, so it reads as parked rather than urgent and stays distinct from a
    /// deliberate Stop. Derived rather than stored, so it needs no per-theme
    /// definition and stays out of the `color_fields_mut` drift guard (#2250).
    pub fn dormant(&self) -> Color {
        blend(self.fresh_idle, self.dimmed, 0.5)
    }
}

/// Linear RGB blend of `a` and `b` at `t` (0.0 = all `a`). Falls back to `a` for
/// a non-RGB terminal color, which theme colors never are. Mirrors the private
/// `mix` in `resolved.rs`, kept local to avoid a backwards dependency.
fn blend(a: Color, b: Color, t: f32) -> Color {
    let rgb = |c: Color| match c {
        Color::Rgb(r, g, bl) => Some((r, g, bl)),
        _ => None,
    };
    let (Some((ar, ag, ab)), Some((br, bg, bb))) = (rgb(a), rgb(b)) else {
        return a;
    };
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t).round() as u8;
    Color::Rgb(lerp(ar, br), lerp(ag, bg), lerp(ab, bb))
}

impl Theme {
    /// Mutable references to every `Color` field, in declaration order: the
    /// authoritative list shared by `downsample_to_palette` and the structural
    /// guard test. Non-color metadata must not be added here.
    pub fn color_fields_mut(&mut self) -> [&mut Color; 26] {
        [
            &mut self.background,
            &mut self.border,
            &mut self.terminal_border,
            &mut self.selection,
            &mut self.session_selection,
            &mut self.title,
            &mut self.text,
            &mut self.dimmed,
            &mut self.hint,
            &mut self.running,
            &mut self.waiting,
            &mut self.fresh_idle,
            &mut self.idle,
            &mut self.unread,
            &mut self.error,
            &mut self.terminal_active,
            &mut self.group,
            &mut self.search,
            &mut self.accent,
            &mut self.diff_add,
            &mut self.diff_delete,
            &mut self.diff_modified,
            &mut self.diff_header,
            &mut self.help_key,
            &mut self.branch,
            &mut self.sandbox,
        ]
    }

    /// Read-only counterpart to `color_fields_mut`.
    pub fn color_fields(&self) -> [Color; 26] {
        [
            self.background,
            self.border,
            self.terminal_border,
            self.selection,
            self.session_selection,
            self.title,
            self.text,
            self.dimmed,
            self.hint,
            self.running,
            self.waiting,
            self.fresh_idle,
            self.idle,
            self.unread,
            self.error,
            self.terminal_active,
            self.group,
            self.search,
            self.accent,
            self.diff_add,
            self.diff_delete,
            self.diff_modified,
            self.diff_header,
            self.help_key,
            self.branch,
            self.sandbox,
        ]
    }

    /// Convert every `Color::Rgb` field to the nearest xterm-256 index, in place.
    /// Idempotent. For transports that mangle 24-bit RGB but handle 256-palette.
    pub fn downsample_to_palette(&mut self) {
        for field in self.color_fields_mut() {
            *field = color_to_palette(*field);
        }
    }
}

/// Serde helper for Color as hex string (#rrggbb)
pub(super) mod hex_color {
    use ratatui::style::Color;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(color: &Color, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *color {
            Color::Rgb(r, g, b) => {
                serializer.serialize_str(&format!("#{:02x}{:02x}{:02x}", r, g, b))
            }
            _ => serializer.serialize_str("#000000"),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Color, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = String::deserialize(deserializer)?;
        parse_hex_color(&s).map_err(serde::de::Error::custom)
    }

    pub fn parse_hex_color(s: &str) -> Result<Color, String> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        if !hex.is_ascii() || hex.len() != 6 {
            return Err(format!(
                "invalid hex color '{}': expected 6 hex digits (e.g. #ff0000)",
                s
            ));
        }
        let r =
            u8::from_str_radix(&hex[0..2], 16).map_err(|_| format!("invalid hex color '{}'", s))?;
        let g =
            u8::from_str_radix(&hex[2..4], 16).map_err(|_| format!("invalid hex color '{}'", s))?;
        let b =
            u8::from_str_radix(&hex[4..6], 16).map_err(|_| format!("invalid hex color '{}'", s))?;
        Ok(Color::Rgb(r, g, b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::styles::{builtin_theme_names, load_theme};

    #[test]
    fn downsample_to_palette_converts_all_fields() {
        // Structural guard: every Color field listed in `color_fields_mut` must
        // survive downsampling without an Rgb left behind. The test is only as
        // strong as that list, which is accepted because it is the single source of
        // truth for what counts as a color field (the `default_matches_empire_toml`
        // drift guard consumes it too), so drifting means forgetting two spots.
        let mut theme = load_theme("empire");
        theme.downsample_to_palette();
        for color in theme.color_fields() {
            assert!(
                !matches!(color, Color::Rgb(_, _, _)),
                "Rgb still present after downsample: {:?}",
                color
            );
        }
    }

    #[test]
    fn hex_color_parse() {
        assert_eq!(
            hex_color::parse_hex_color("#ff0000").unwrap(),
            Color::Rgb(255, 0, 0)
        );
        assert_eq!(
            hex_color::parse_hex_color("#00ff00").unwrap(),
            Color::Rgb(0, 255, 0)
        );
        assert_eq!(
            hex_color::parse_hex_color("#0000ff").unwrap(),
            Color::Rgb(0, 0, 255)
        );
        assert_eq!(
            hex_color::parse_hex_color("#0f172a").unwrap(),
            Color::Rgb(15, 23, 42)
        );
        // Without # prefix
        assert_eq!(
            hex_color::parse_hex_color("fbbf24").unwrap(),
            Color::Rgb(251, 191, 36)
        );
        assert!(hex_color::parse_hex_color("#fff").is_err());
        assert!(hex_color::parse_hex_color("#gggggg").is_err());
        assert!(hex_color::parse_hex_color("").is_err());
        // Multi-byte UTF-8 that happens to be 6 bytes must not panic
        assert!(hex_color::parse_hex_color("\u{00e9}\u{00e9}\u{00e9}").is_err());
        assert!(hex_color::parse_hex_color("#\u{00e9}\u{00e9}\u{00e9}").is_err());
    }

    #[test]
    fn idle_color_at_age_boundaries() {
        let theme = load_theme("empire");
        let window = idle_decay_window(20);
        // No timestamp = decayed.
        assert_eq!(theme.idle_color_at_age(None, window), theme.idle);
        // Zero age = fresh.
        assert_eq!(
            theme.idle_color_at_age(Some(Duration::ZERO), window),
            theme.fresh_idle
        );
        // Inside the window = fresh.
        assert_eq!(
            theme.idle_color_at_age(Some(window / 2), window),
            theme.fresh_idle
        );
        // At the boundary clamps to decayed (age >= window).
        assert_eq!(theme.idle_color_at_age(Some(window), window), theme.idle);
        // Past the window = decayed.
        assert_eq!(
            theme.idle_color_at_age(Some(window + Duration::from_secs(60)), window),
            theme.idle
        );
        // window = 0 is the documented opt-out: every Idle row renders
        // as fully decayed regardless of age.
        assert_eq!(
            theme.idle_color_at_age(Some(Duration::from_secs(1)), Duration::ZERO),
            theme.idle
        );
        assert_eq!(
            theme.idle_color_at_age(Some(Duration::from_secs(1_000_000)), Duration::ZERO),
            theme.idle
        );
    }

    #[test]
    fn theme_attention_hierarchy_holds() {
        // Visual hierarchy: Waiting grabs the most attention, fresh-idle one rung
        // dimmer, decayed idle blends in. On dark backgrounds more attention means
        // higher perceived luminance; on light backgrounds, lower. The comparison
        // direction comes off the theme's own background; Rec. 601 is good enough
        // for a pairwise sanity check. A custom theme with a mid-tone background
        // could fall on the wrong side of the split, which is why this guards only
        // the builtins in `BUILTIN_THEMES`.
        fn luminance(c: Color) -> f32 {
            match c {
                Color::Rgb(r, g, b) => 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32,
                _ => 0.0,
            }
        }
        for name in builtin_theme_names() {
            let theme = load_theme(name);
            let bg = luminance(theme.background);
            let dark_bg = bg < 128.0;
            let cmp = |label_a, a, label_b, b| {
                if dark_bg {
                    assert!(
                        a > b,
                        "{name} (dark bg): {label_a} luminance {a:.1} should exceed {label_b} {b:.1}"
                    );
                } else {
                    assert!(
                        a < b,
                        "{name} (light bg): {label_a} luminance {a:.1} should be below {label_b} {b:.1}"
                    );
                }
            };
            let w = luminance(theme.waiting);
            let f = luminance(theme.fresh_idle);
            let i = luminance(theme.idle);
            let u = luminance(theme.unread);
            // Waiting beats fresh-idle.
            cmp("waiting", w, "fresh_idle", f);
            // Fresh-idle beats fully-decayed idle.
            cmp("fresh_idle", f, "idle", i);
            // Unread sits between waiting and idle (#2088); its relationship to
            // fresh_idle is intentionally unconstrained, so themes may tie them.
            cmp("waiting", w, "unread", u);
            cmp("unread", u, "idle", i);
        }
    }
}

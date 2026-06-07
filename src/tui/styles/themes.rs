//! Built-in themes and the `Theme` palette struct.

use std::time::Duration;

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use super::palette::color_to_palette;

/// Whether a theme renders against a dark or light surface. Drives
/// web-side surface ramp derivation (dark themes lighten from
/// background, light themes darken from background) and selects the
/// fallback syntax highlighter theme when none is specified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeAppearance {
    Dark,
    Light,
}

/// Per-theme syntax-highlighter metadata. Lives in `[syntax]` in the
/// TOML so renderer-specific knobs don't pollute the flat semantic
/// color fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSyntax {
    /// Name of the Shiki theme module to load on the web (`github-dark`,
    /// `dracula`, `catppuccin-latte`, etc.). `None` falls back by
    /// appearance: dark themes get `github-dark`, light themes get
    /// `github-light`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shiki_theme: Option<String>,
}

impl ThemeSyntax {
    fn is_default(&self) -> bool {
        self.shiki_theme.is_none()
    }
}

/// Convert the user-configured decay duration (minutes) into a `Duration`.
/// `0` returns `Duration::ZERO`, which the freshness logic treats as
/// "fully decayed immediately" — a documented opt-out: every Idle row
/// renders with the static idle look the moment its Stop hook fires.
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
    /// Color for a session within the idle decay window (just transitioned
    /// to Idle, hasn't aged out yet). Held constant for the full window so
    /// the breathe rattle's pulse stays visually consistent, then snaps to
    /// `idle` once the window expires. Should sit between `waiting`
    /// (brightest, "needs you NOW") and `idle` (dimmest, "no rush") on the
    /// theme's perceived-attention scale.
    #[serde(with = "hex_color")]
    pub fresh_idle: Color,
    #[serde(with = "hex_color")]
    pub idle: Color,
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

    /// Whether the theme is dark or light. Optional; when absent the
    /// resolver classifies the theme from `background` luminance. Use
    /// per-field `#[serde(default)]` so a partial custom TOML that
    /// omits this field deserializes to `None` rather than inheriting
    /// Empire's `Dark`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appearance: Option<ThemeAppearance>,

    /// Renderer-specific syntax-highlighter overrides. Lives in nested
    /// `[syntax]`; empty by default so custom TOMLs without it round-trip
    /// without a stray empty section.
    #[serde(default, skip_serializing_if = "ThemeSyntax::is_default")]
    pub syntax: ThemeSyntax,
}

impl Default for Theme {
    fn default() -> Self {
        // Hardcoded Empire palette used only as serde's per-field
        // fallback for partial custom TOMLs. Must NOT round-trip
        // through `load_theme("empire")` + `toml::from_str`: serde's
        // container-level `#[serde(default)]` calls Theme::default()
        // every time it deserializes a Theme (to seed the struct
        // before overwriting present fields), so parsing any TOML
        // would re-enter Default which would re-parse Empire which
        // would re-enter Default... a self-referential deadlock. The
        // values mirror `themes/builtin/empire.toml`; if Empire's
        // palette ever drifts, sync this manually (or replace with a
        // build-time codegen step).
        Self {
            background: Color::Rgb(0x0f, 0x17, 0x2a),
            border: Color::Rgb(0x33, 0x41, 0x55),
            terminal_border: Color::Rgb(0x0d, 0x94, 0x88),
            selection: Color::Rgb(0x26, 0x32, 0x4b),
            session_selection: Color::Rgb(0x37, 0x41, 0x5c),
            title: Color::Rgb(0xfb, 0xbf, 0x24),
            text: Color::Rgb(0xcb, 0xd5, 0xe1),
            dimmed: Color::Rgb(0x64, 0x74, 0x8b),
            hint: Color::Rgb(0x94, 0xa3, 0xb8),
            running: Color::Rgb(0x22, 0xc5, 0x5e),
            waiting: Color::Rgb(0xfb, 0xbf, 0x24),
            fresh_idle: Color::Rgb(0xf5, 0x9e, 0x0b),
            idle: Color::Rgb(0x64, 0x74, 0x8b),
            error: Color::Rgb(0xef, 0x44, 0x44),
            terminal_active: Color::Rgb(0x0d, 0x94, 0x88),
            group: Color::Rgb(0xcb, 0xd5, 0xe1),
            search: Color::Rgb(0xfb, 0xbf, 0x24),
            accent: Color::Rgb(0xd9, 0x77, 0x06),
            diff_add: Color::Rgb(0x22, 0xc5, 0x5e),
            diff_delete: Color::Rgb(0xef, 0x44, 0x44),
            diff_modified: Color::Rgb(0xfb, 0xbf, 0x24),
            diff_header: Color::Rgb(0x0d, 0x94, 0x88),
            help_key: Color::Rgb(0xd9, 0x77, 0x06),
            branch: Color::Rgb(0x0d, 0x94, 0x88),
            sandbox: Color::Rgb(0x94, 0xa3, 0xb8),
            // appearance and syntax default to None / empty here,
            // but the per-field `#[serde(default)]` attributes on
            // those fields take precedence over this container
            // default, so partial custom TOMLs that omit them still
            // resolve to None rather than inheriting Empire's values
            // (covered by `partial_custom_theme_does_not_inherit_metadata`).
            appearance: None,
            syntax: ThemeSyntax { shiki_theme: None },
        }
    }
}

impl Theme {
    /// Color for an Idle session, given the elapsed time since it
    /// transitioned to Idle and the user-configured decay window.
    ///
    /// Two-state binary: `fresh_idle` while age is inside the window,
    /// `idle` once past it (or when age/window aren't usable: `None` age,
    /// zero window). The pulse phase deliberately holds a constant color
    /// — a continuous lerp under the breathe rattle reads as noisy. If we
    /// ever want a gradient back, add an interpolator and call it here.
    pub fn idle_color_at_age(&self, age: Option<Duration>, window: Duration) -> Color {
        let Some(age) = age else {
            return self.idle;
        };
        if window.is_zero() || age >= window {
            return self.idle;
        }
        self.fresh_idle
    }
}

impl Theme {
    /// Mutable references to every `Color` field, in declaration order. The
    /// single authoritative list shared by `downsample_to_palette` and the
    /// structural guard test. New `Color` fields added to `Theme` must be
    /// added here too; non-color metadata (appearance, syntax, etc.) must not.
    pub fn color_fields_mut(&mut self) -> [&mut Color; 25] {
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
    pub fn color_fields(&self) -> [Color; 25] {
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

    /// Convert every `Color::Rgb` field to the nearest xterm-256 palette index
    /// (`Color::Indexed`). In-place. Idempotent: already-Indexed / named /
    /// Reset colors are untouched. Use when the downstream transport mangles
    /// 24-bit RGB escapes but handles 256-palette fine (e.g. Termius mosh).
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
        // Structural guard: every Color field listed in `color_fields_mut`
        // must survive downsampling without an Rgb left behind.
        //
        // Tradeoff vs the pre-PR version: that one cross-checked against
        // the serialized field count, so a Color field added to Theme but
        // missing from `downsample_to_palette` failed loud. Here the test
        // is only as strong as `color_fields_mut`: if a new Color field is
        // added to Theme but not to `color_fields_mut`, the downsample
        // silently misses it and this test still passes. We accept that
        // because `color_fields_mut` is the single source of truth for
        // "what counts as a color field" (both downsample and the
        // `default_matches_empire_toml` drift guard consume it), so the
        // only way to drift is to forget two spots at once instead of one.
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
    fn test_hex_color_parse() {
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
    }

    #[test]
    fn test_hex_color_parse_invalid() {
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
    }

    #[test]
    fn idle_color_at_age_zero_window_disables_freshness() {
        // window = 0 is the documented opt-out: every Idle row renders
        // as fully decayed regardless of age. No pulse, no fresh tint.
        let theme = load_theme("empire");
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
        // Visual hierarchy: Waiting is the most attention-grabbing state;
        // fresh-idle sits one rung dimmer; decayed idle blends in. On dark
        // backgrounds "more attention" means HIGHER perceived luminance;
        // on light backgrounds it means LOWER (the warm hues read against
        // the bright surface). The check picks the comparison direction
        // off the theme's own background. Rec. 601 is good enough for a
        // pairwise sanity check, not a formal contrast metric.
        //
        // Heuristic limit: a custom user theme with a mid-tone background
        // (luminance near the 128 cutoff) could fall on the wrong side of
        // the dark/light split and fail this assertion in surprising
        // ways. That's intentional, the test guards every built-in
        // registered in `BUILTIN_THEMES`, not arbitrary user themes loaded
        // from `~/.config/hmp/themes/*.toml`. If a custom-theme
        // contributor needs to bypass this, they should pick `fresh_idle`
        // themselves rather than rely on the test to validate it.
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
            // Waiting beats fresh-idle.
            cmp("waiting", w, "fresh_idle", f);
            // Fresh-idle beats fully-decayed idle.
            cmp("fresh_idle", f, "idle", i);
        }
    }
}

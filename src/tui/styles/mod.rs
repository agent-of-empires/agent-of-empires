//! TUI theme and styling: builtin TOML embedding, custom theme discovery, and
//! load/serialize glue. `themes` holds the `Theme` struct and its Empire-mirror
//! `Default`; `palette` does the 24-bit to xterm-256 downsampling. The public
//! surface is re-exported here so callers keep `crate::tui::styles::*`.

mod contrast;
mod palette;
mod resolved;
mod themes;

pub use contrast::has_min_contrast;
pub use resolved::{resolve_theme, ResolvedTheme};
pub use themes::ThemeAppearance;
pub use themes::{idle_decay_window, Theme};

use std::path::PathBuf;
use tracing::{debug, warn};

/// One built-in theme, its `source` the TOML body embedded via `include_str!`.
/// Adding a builtin is `themes/builtin/X.toml` plus one entry here.
pub struct BuiltinTheme {
    pub name: &'static str,
    pub source: &'static str,
}

pub const BUILTIN_THEMES: &[BuiltinTheme] = &[
    BuiltinTheme {
        name: "zinc",
        source: include_str!("../../../themes/builtin/zinc.toml"),
    },
    BuiltinTheme {
        name: "empire",
        source: include_str!("../../../themes/builtin/empire.toml"),
    },
    BuiltinTheme {
        name: "phosphor",
        source: include_str!("../../../themes/builtin/phosphor.toml"),
    },
    BuiltinTheme {
        name: "tokyo-night-storm",
        source: include_str!("../../../themes/builtin/tokyo-night-storm.toml"),
    },
    BuiltinTheme {
        name: "catppuccin-latte",
        source: include_str!("../../../themes/builtin/catppuccin-latte.toml"),
    },
    BuiltinTheme {
        name: "dracula",
        source: include_str!("../../../themes/builtin/dracula.toml"),
    },
    BuiltinTheme {
        name: "rose-pine",
        source: include_str!("../../../themes/builtin/rose-pine.toml"),
    },
    BuiltinTheme {
        name: "deep-ocean",
        source: include_str!("../../../themes/builtin/deep-ocean.toml"),
    },
];

pub fn builtin_theme_names() -> impl Iterator<Item = &'static str> {
    BUILTIN_THEMES.iter().map(|b| b.name)
}

pub fn is_builtin_theme(name: &str) -> bool {
    BUILTIN_THEMES.iter().any(|b| b.name == name)
}

pub fn custom_themes_dir() -> Option<PathBuf> {
    crate::session::get_app_dir().ok().map(|d| d.join("themes"))
}

/// Discover custom theme names from the themes directory.
/// Returns (name, path) pairs sorted alphabetically.
pub fn discover_custom_themes() -> Vec<(String, PathBuf)> {
    let dir = match custom_themes_dir() {
        Some(d) if d.is_dir() => d,
        _ => return Vec::new(),
    };

    let mut themes = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                let name = stem.to_string();
                if !is_builtin_theme(&name) {
                    themes.push((name, path));
                }
            }
        }
    }

    themes.sort_by(|a, b| a.0.cmp(&b.0));
    themes
}

/// Themes contributed by active plugins, as (name, path) pairs. Layered below
/// builtins and user themes, which a plugin cannot shadow; already-claimed names
/// are filtered out.
pub fn discover_plugin_themes() -> Vec<(String, PathBuf)> {
    let mut claimed: std::collections::HashSet<String> = builtin_theme_names()
        .map(|s| s.to_string())
        .chain(discover_custom_themes().into_iter().map(|(n, _)| n))
        .collect();
    // De-dup across plugins: two contributing the same name would be
    // indistinguishable in the picker, and `load_theme` resolves only the first.
    let mut out = Vec::new();
    for (name, path) in crate::plugin::active_plugin_themes() {
        if claimed.insert(name.clone()) {
            out.push((name, path));
        }
    }
    out
}

/// Return the full list of available theme names: built-in themes first, then
/// user custom themes, then active-plugin themes.
pub fn available_themes() -> Vec<String> {
    let mut names: Vec<String> = builtin_theme_names().map(|s| s.to_string()).collect();
    for (name, _) in discover_custom_themes() {
        names.push(name);
    }
    for (name, _) in discover_plugin_themes() {
        names.push(name);
    }
    names
}

fn load_custom_theme(path: &std::path::Path) -> Option<Theme> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to read theme file {}: {}", path.display(), e);
            return None;
        }
    };

    match toml::from_str::<Theme>(&content) {
        Ok(theme) => Some(fill_unread_from_accent(&content, theme)),
        Err(e) => {
            warn!("Failed to parse theme file {}: {}", path.display(), e);
            None
        }
    }
}

/// A theme TOML that omits `unread` should inherit that theme's own `accent`,
/// not Empire's blue. The container `#[serde(default)]` seeds omitted fields from
/// Empire and serde cannot tell an omission from an explicit match, so detect the
/// omission from the raw table.
fn fill_unread_from_accent(content: &str, mut theme: Theme) -> Theme {
    let omitted = content
        .parse::<toml::Table>()
        .map(|t| !t.contains_key("unread"))
        .unwrap_or(false);
    if omitted {
        theme.unread = theme.accent;
    }
    theme
}

/// Parse a builtin's embedded TOML. These are committed and embedded at build
/// time, so a parse failure is a developer bug; the
/// `all_builtins_parse_with_expected_anchors` test guards it.
fn parse_builtin(builtin: &BuiltinTheme) -> Theme {
    let theme = toml::from_str(builtin.source)
        .unwrap_or_else(|e| panic!("builtin theme '{}' failed to parse: {}", builtin.name, e));
    // All builtins define `unread` today; keep the custom-theme fallback so a
    // future one that omits it inherits its own accent rather than Empire's.
    fill_unread_from_accent(builtin.source, theme)
}

pub fn load_theme(name: &str) -> Theme {
    if let Some(builtin) = BUILTIN_THEMES.iter().find(|b| b.name == name) {
        debug!(theme = name, source = "builtin", "loaded theme");
        return parse_builtin(builtin);
    }
    for (theme_name, path) in discover_custom_themes() {
        if theme_name == name {
            if let Some(theme) = load_custom_theme(&path) {
                debug!(
                    theme = name,
                    source = "custom",
                    path = %path.display(),
                    "loaded theme"
                );
                return theme;
            }
        }
    }
    for (theme_name, path) in discover_plugin_themes() {
        if theme_name == name {
            if let Some(theme) = load_custom_theme(&path) {
                debug!(
                    theme = name,
                    source = "plugin",
                    path = %path.display(),
                    "loaded theme"
                );
                return theme;
            }
        }
    }
    warn!("Unknown theme '{}', falling back to zinc", name);
    // Inline the default fallback rather than recursing through `load_theme`, so
    // a rename or removal of the `zinc` default panics clearly instead of looping.
    let default = BUILTIN_THEMES
        .iter()
        .find(|b| b.name == "zinc")
        .expect("'zinc' builtin missing from BUILTIN_THEMES");
    parse_builtin(default)
}

/// Load a theme and, when `palette_mode` is true, convert every `Color::Rgb`
/// field to the nearest xterm-256 `Color::Indexed`. Hex strings parse to `Rgb`,
/// so the downsample runs at the `Theme` level after parsing.
pub fn load_theme_with_mode(name: &str, palette_mode: bool) -> Theme {
    let mut theme = load_theme(name);
    if palette_mode {
        theme.downsample_to_palette();
    }
    theme
}

pub fn export_theme_toml(theme: &Theme) -> Result<String, toml::ser::Error> {
    toml::to_string_pretty(theme)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn load_theme_with_mode_picks_color_depth() {
        assert!(matches!(
            load_theme_with_mode("empire", true).title,
            Color::Indexed(_)
        ));
        assert!(matches!(
            load_theme_with_mode("empire", false).title,
            Color::Rgb(_, _, _)
        ));
    }

    #[test]
    fn omitted_unread_inherits_the_themes_own_accent() {
        let accent = Color::Rgb(0x7a, 0xa2, 0xf7);
        for (toml_str, want) in [
            ("background = \"#1a1b26\"\naccent = \"#7aa2f7\"\n", accent),
            (
                "background = \"#1a1b26\"\naccent = \"#7aa2f7\"\nunread = \"#ff0000\"\n",
                Color::Rgb(0xff, 0x00, 0x00),
            ),
        ] {
            let theme: Theme = toml::from_str(toml_str).unwrap();
            let theme = fill_unread_from_accent(toml_str, theme);
            assert_eq!(theme.accent, accent);
            assert_eq!(theme.unread, want, "{toml_str}");
        }
    }

    /// Anchor colors for the builtin themes, `(name, background, title)`.
    /// `all_builtins_parse_with_expected_anchors` walks the list and asserts every
    /// embedded TOML deserializes to the expected hex on both anchors: the minimum
    /// that catches a typo anywhere but the anchors themselves. A new builtin needs
    /// a row here.
    const BUILTIN_COLOR_ANCHORS: &[(&str, Color, Color)] = &[
        (
            "zinc",
            Color::Rgb(0x1c, 0x1c, 0x1f),
            Color::Rgb(0xfb, 0xbf, 0x24),
        ),
        (
            "empire",
            Color::Rgb(0x0f, 0x17, 0x2a),
            Color::Rgb(0xfb, 0xbf, 0x24),
        ),
        (
            "phosphor",
            Color::Rgb(0x10, 0x14, 0x12),
            Color::Rgb(0x39, 0xff, 0x14),
        ),
        (
            "tokyo-night-storm",
            Color::Rgb(0x24, 0x28, 0x3b),
            Color::Rgb(0x7a, 0xa2, 0xf7),
        ),
        (
            "catppuccin-latte",
            Color::Rgb(0xef, 0xf1, 0xf5),
            Color::Rgb(0x1e, 0x66, 0xf5),
        ),
        (
            "dracula",
            Color::Rgb(0x28, 0x2a, 0x36),
            Color::Rgb(0xbd, 0x93, 0xf9),
        ),
        (
            "rose-pine",
            Color::Rgb(0x19, 0x17, 0x24),
            Color::Rgb(0xc4, 0xa7, 0xe7),
        ),
        (
            "deep-ocean",
            Color::Rgb(0x0f, 0x11, 0x1a),
            Color::Rgb(0x84, 0xff, 0xff),
        ),
    ];

    #[test]
    fn concurrent_load_theme_does_not_deadlock() {
        if !crate::tui::isolated_test_process(
            "tui::styles::tests::concurrent_load_theme_does_not_deadlock",
            std::time::Duration::from_secs(5),
        ) {
            return;
        }
        let start = std::sync::Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    for (name, background, title) in BUILTIN_COLOR_ANCHORS {
                        let theme = load_theme(name);
                        assert_eq!(theme.background, *background, "{name}");
                        assert_eq!(theme.title, *title, "{name}");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("theme loader panicked");
        }
    }

    #[test]
    fn default_matches_empire_toml() {
        // Drift guard: every color field in `impl Default for Theme` (serde's
        // fallback for partial custom TOMLs) must match the corresponding hex in
        // `themes/builtin/empire.toml`, or a palette tweak that updates only the
        // TOML leaves partial custom TOMLs inheriting stale colors.
        let defaulted = Theme::default();
        let from_toml = load_theme("empire");
        assert_eq!(
            defaulted.color_fields().to_vec(),
            from_toml.color_fields().to_vec(),
            "Theme::default() color fields drifted from themes/builtin/empire.toml; \
             sync the hand-mirrored values in `impl Default for Theme` (themes.rs)"
        );
    }

    #[test]
    fn default_does_not_recurse_through_load_theme() {
        if !crate::tui::isolated_test_process(
            "tui::styles::tests::default_does_not_recurse_through_load_theme",
            std::time::Duration::from_secs(5),
        ) {
            return;
        }
        let d = Theme::default();
        assert_eq!(d.background, Color::Rgb(0x0f, 0x17, 0x2a));
        assert_eq!(d.title, Color::Rgb(0xfb, 0xbf, 0x24));
        for name in builtin_theme_names() {
            let _ = load_theme(name);
        }
    }

    #[test]
    fn all_builtins_parse_with_expected_anchors_and_roundtrip() {
        // Every entry in BUILTIN_THEMES must deserialize cleanly and match the
        // expected anchors; otherwise a typo surfaces only at the first runtime
        // load. Catppuccin Latte is the lone light builtin.
        let table_names: Vec<&str> = BUILTIN_COLOR_ANCHORS.iter().map(|(n, _, _)| *n).collect();
        for name in builtin_theme_names() {
            assert!(
                table_names.contains(&name),
                "builtin '{name}' missing from BUILTIN_COLOR_ANCHORS test table"
            );
        }
        for (name, expected_bg, expected_title) in BUILTIN_COLOR_ANCHORS {
            let theme = load_theme(name);
            assert_eq!(theme.background, *expected_bg, "{name} background");
            assert_eq!(theme.title, *expected_title, "{name} title");
            let expected_appearance = if *name == "catppuccin-latte" {
                ThemeAppearance::Light
            } else {
                ThemeAppearance::Dark
            };
            assert_eq!(theme.appearance, Some(expected_appearance), "{name}");
            assert!(theme.syntax.shiki_theme.is_some(), "{name} shiki_theme");
            let exported = export_theme_toml(&theme).unwrap();
            let loaded: Theme = toml::from_str(&exported).unwrap();
            assert_eq!(theme.color_fields(), loaded.color_fields(), "{name}");
        }
    }

    #[test]
    fn partial_custom_theme_takes_empire_colors_but_not_metadata() {
        // Container-level `#[serde(default)]` would have a missing `appearance`
        // fall back to Empire's `Dark`; the per-field defaults must override that
        // so absent metadata resolves to None / empty.
        let toml_str = r##"
background = "#1a1b26"
border = "#414868"
"##;
        let theme: Theme = toml::from_str(toml_str).unwrap();
        assert_eq!(theme.background, Color::Rgb(26, 27, 38));
        assert_eq!(theme.title, load_theme("empire").title);
        assert_eq!(theme.appearance, None);
        assert!(theme.syntax.shiki_theme.is_none());
    }

    #[test]
    fn unknown_theme_falls_back_to_default() {
        let theme = load_theme("nonexistent-theme");
        let default = load_theme("zinc");
        assert_eq!(theme.color_fields(), default.color_fields());
    }

    #[test]
    fn load_custom_theme_reads_valid_and_rejects_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("my-theme.toml");
        std::fs::write(&good, export_theme_toml(&load_theme("dracula")).unwrap()).unwrap();
        let loaded = load_custom_theme(&good).unwrap();
        assert_eq!(loaded.background, Color::Rgb(40, 42, 54));

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "not valid theme data").unwrap();
        assert!(load_custom_theme(&bad).is_none());
    }
}

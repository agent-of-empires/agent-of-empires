//! TUI theme and styling.
//!
//! The module is split into:
//!   - `themes`: the `Theme` struct, its `Default` (Empire mirror), and palette downsampling
//!   - `palette`: 24-bit RGB -> xterm-256 downsampling for `palette_mode`
//!   - this file: builtin TOML embedding, custom theme discovery, load/serialize glue
//!
//! Public surface is re-exported here so callers keep `crate::tui::styles::*`.

mod contrast;
mod palette;
#[cfg(feature = "serve")]
mod resolved;
mod themes;

pub use contrast::has_min_contrast;
#[cfg(feature = "serve")]
pub use resolved::{resolve_theme, ResolvedTheme};
#[cfg(any(feature = "serve", test))]
pub use themes::ThemeAppearance;
pub use themes::{idle_decay_window, Theme};

use std::path::PathBuf;
use tracing::{debug, warn};

/// One built-in theme. `source` is the TOML body embedded at compile time
/// via `include_str!`. Adding a new builtin is: drop `themes/builtin/X.toml`
/// + add one entry here.
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

/// Iterator over builtin theme names, in declared order.
pub fn builtin_theme_names() -> impl Iterator<Item = &'static str> {
    BUILTIN_THEMES.iter().map(|b| b.name)
}

/// Whether `name` refers to a builtin theme.
pub fn is_builtin_theme(name: &str) -> bool {
    BUILTIN_THEMES.iter().any(|b| b.name == name)
}

/// Return the directory where custom theme TOML files are stored.
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

/// Return the full list of available theme names: built-in themes first,
/// then custom.
pub fn available_themes() -> Vec<String> {
    let mut names: Vec<String> = builtin_theme_names().map(|s| s.to_string()).collect();
    for (name, _) in discover_custom_themes() {
        names.push(name);
    }
    names
}

/// Load a custom theme from a TOML file.
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
/// not Empire's default blue. The container `#[serde(default)]` seeds every
/// omitted field from `Theme::default()` (= Empire), and serde can't tell an
/// omitted key from one explicitly set to Empire's value, so we detect the
/// omission from the raw table and fall back to the parsed theme's accent.
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

/// Parse a builtin's embedded TOML. Builtin TOMLs are committed to the repo
/// and embedded at build time; a parse failure here is a developer bug, not
/// user input. The `all_builtins_parse_with_expected_anchors` test guards
/// against that landing in main.
fn parse_builtin(builtin: &BuiltinTheme) -> Theme {
    let theme = toml::from_str(builtin.source)
        .unwrap_or_else(|e| panic!("builtin theme '{}' failed to parse: {}", builtin.name, e));
    // All builtins define `unread`, so this is a no-op for them today, but
    // keep the same fallback as custom themes so a future builtin that omits
    // it inherits its own accent rather than Empire's.
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
    warn!("Unknown theme '{}', falling back to zinc", name);
    // Inline the default fallback rather than recursing through `load_theme`,
    // so a future rename or removal of the fallback builtin would surface
    // as a clear panic here instead of looping. `zinc` is the default theme.
    let default = BUILTIN_THEMES
        .iter()
        .find(|b| b.name == "zinc")
        .expect("'zinc' builtin missing from BUILTIN_THEMES");
    parse_builtin(default)
}

/// Load a theme and, when `palette_mode` is true, convert every `Color::Rgb`
/// field to `Color::Indexed` (nearest xterm-256 index). Use this from callers
/// that have access to `ThemeConfig::color_mode`. Hex strings in the embedded
/// and custom TOMLs deserialize to `Color::Rgb`; `palette_mode` consumers need
/// xterm-256 `Color::Indexed`, so the downsample runs at the `Theme` level
/// after parsing.
pub fn load_theme_with_mode(name: &str, palette_mode: bool) -> Theme {
    let mut theme = load_theme(name);
    if palette_mode {
        theme.downsample_to_palette();
    }
    theme
}

/// Export a theme as a TOML string.
pub fn export_theme_toml(theme: &Theme) -> Result<String, toml::ser::Error> {
    toml::to_string_pretty(theme)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use std::io::Write;

    #[test]
    fn load_theme_with_mode_palette_yields_indexed() {
        let theme = load_theme_with_mode("empire", true);
        assert!(matches!(theme.title, Color::Indexed(_)));
    }

    #[test]
    fn custom_theme_without_unread_inherits_accent() {
        // A theme TOML that omits `unread` should fall back to that theme's
        // own accent, not Empire's default blue.
        let toml_str = "background = \"#1a1b26\"\naccent = \"#7aa2f7\"\n";
        let theme: Theme = toml::from_str(toml_str).unwrap();
        let theme = fill_unread_from_accent(toml_str, theme);
        assert_eq!(theme.accent, Color::Rgb(0x7a, 0xa2, 0xf7));
        assert_eq!(
            theme.unread, theme.accent,
            "omitted unread field should fall back to accent"
        );
    }

    #[test]
    fn custom_theme_with_explicit_unread_preserves_it() {
        // An explicit `unread` must win over the accent fallback.
        let toml_str = "background = \"#1a1b26\"\naccent = \"#7aa2f7\"\nunread = \"#ff0000\"\n";
        let theme: Theme = toml::from_str(toml_str).unwrap();
        let theme = fill_unread_from_accent(toml_str, theme);
        assert_eq!(theme.unread, Color::Rgb(0xff, 0x00, 0x00));
        assert_ne!(theme.unread, theme.accent);
    }

    #[test]
    fn load_theme_with_mode_truecolor_yields_rgb() {
        let theme = load_theme_with_mode("empire", false);
        assert!(matches!(theme.title, Color::Rgb(_, _, _)));
    }

    /// Anchor colors for the builtin themes. Each row is
    /// `(name, background, title)`. The structural test
    /// `all_builtins_parse_with_expected_anchors` walks the list and asserts
    /// every embedded TOML deserializes to the expected hex on both anchors.
    /// Two anchors per theme is the minimum that catches a typo anywhere
    /// other than the anchors themselves; cross-field rendering tests cover
    /// the rest. Adding a new builtin requires one row here.
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
        // Belt-and-braces: even with the fixed Default impl, exercise
        // the load path from many threads at once. If some future
        // change reintroduces a self-referential lock in the load
        // path, the watchdog timeout catches it.
        use std::sync::{Arc, Mutex};
        use std::thread;
        use std::time::{Duration, Instant};

        let started = Instant::now();
        let names: Vec<&'static str> = builtin_theme_names().collect();
        let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let names = names.clone();
            let errors = Arc::clone(&errors);
            handles.push(thread::spawn(move || {
                for name in names {
                    let result = std::panic::catch_unwind(|| load_theme(name));
                    if result.is_err() {
                        errors.lock().unwrap().push(name.to_string());
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }
        let elapsed = started.elapsed();
        let errs = errors.lock().unwrap();
        assert!(
            errs.is_empty(),
            "themes panicked under concurrent load: {:?}",
            *errs
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "16 threads x 6 themes took {:?}; likely a lock contention regression",
            elapsed
        );
    }

    #[test]
    fn default_matches_empire_toml() {
        // Drift guard: every color field in `impl Default for Theme`
        // (which serde's container `#[serde(default)]` uses as the
        // fallback for partial custom TOMLs) must match the corresponding
        // hex in `themes/builtin/empire.toml`. A future Empire palette
        // tweak that updates the TOML but not the hand-mirrored Default
        // would otherwise leave partial custom TOMLs inheriting stale
        // colors silently. Per the review on PR #1197.
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
        // Regression for the OnceLock-via-load_theme deadlock the
        // first cut of this work shipped: serde's container-level
        // `#[serde(default)]` calls Theme::default() to seed before
        // overwriting present fields, so if Default ran load_theme
        // (which runs toml::from_str which calls Default which runs
        // load_theme...) every theme load deadlocked. Default must
        // build the struct inline.
        //
        // Asserting the timing alone would be flaky in CI; instead
        // confirm Default returns the Empire palette directly and
        // that parsing every builtin completes within a tight wall
        // clock (toml::from_str on ~500B is microseconds, not
        // seconds).
        use std::time::{Duration, Instant};
        let started = Instant::now();
        let d = Theme::default();
        assert_eq!(d.background, Color::Rgb(0x0f, 0x17, 0x2a));
        assert_eq!(d.title, Color::Rgb(0xfb, 0xbf, 0x24));
        for name in builtin_theme_names() {
            let _ = load_theme(name);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "loading all builtins took {:?}; serde default likely re-entered load_theme",
            elapsed
        );
    }

    #[test]
    fn all_builtins_parse_with_expected_anchors() {
        // Mandatory parse-all guard: every entry in BUILTIN_THEMES must
        // deserialize cleanly from its embedded TOML and match the expected
        // background and title hex. Without this, a typo in a builtin TOML
        // would only show up at first runtime load via load_theme's panic
        // message.
        for (name, expected_bg, expected_title) in BUILTIN_COLOR_ANCHORS {
            let theme = load_theme(name);
            assert_eq!(
                theme.background, *expected_bg,
                "builtin theme '{}' background mismatch",
                name
            );
            assert_eq!(
                theme.title, *expected_title,
                "builtin theme '{}' title mismatch",
                name
            );
        }
        // Defensive: ensure every builtin in BUILTIN_THEMES has an entry
        // in BUILTIN_COLOR_ANCHORS so the test covers all builtins.
        let table_names: Vec<&str> = BUILTIN_COLOR_ANCHORS.iter().map(|(n, _, _)| *n).collect();
        for name in builtin_theme_names() {
            assert!(
                table_names.contains(&name),
                "builtin '{}' missing from BUILTIN_COLOR_ANCHORS test table",
                name
            );
        }
    }

    #[test]
    fn builtin_appearance_matches_palette() {
        // Catppuccin Latte is the lone light builtin; the rest are dark.
        for name in builtin_theme_names() {
            let theme = load_theme(name);
            let expected = if name == "catppuccin-latte" {
                Some(ThemeAppearance::Light)
            } else {
                Some(ThemeAppearance::Dark)
            };
            assert_eq!(
                theme.appearance, expected,
                "builtin theme '{}' appearance mismatch",
                name
            );
        }
    }

    #[test]
    fn builtin_syntax_shiki_theme_present() {
        for name in builtin_theme_names() {
            let theme = load_theme(name);
            assert!(
                theme.syntax.shiki_theme.is_some(),
                "builtin theme '{}' missing [syntax].shiki_theme",
                name
            );
        }
    }

    #[test]
    fn partial_custom_theme_does_not_inherit_metadata() {
        // Container-level #[serde(default)] would otherwise have a
        // missing `appearance` fall back to Empire's `Dark`. The
        // per-field #[serde(default)] on Option<ThemeAppearance> /
        // ThemeSyntax must override that so absent metadata resolves
        // to None / empty.
        let toml_str = r##"
background = "#1a1b26"
border = "#414868"
"##;
        let theme: Theme = toml::from_str(toml_str).unwrap();
        assert_eq!(
            theme.appearance, None,
            "partial custom TOML must not inherit Empire's appearance"
        );
        assert!(
            theme.syntax.shiki_theme.is_none(),
            "partial custom TOML must not inherit Empire's syntax.shiki_theme"
        );
    }

    #[test]
    fn unknown_theme_falls_back_to_default() {
        let theme = load_theme("nonexistent-theme");
        let default = load_theme("zinc");
        assert_eq!(
            theme.color_fields(),
            default.color_fields(),
            "fallback theme color fields drifted from default"
        );
    }

    #[test]
    fn test_builtin_themes_count() {
        assert_eq!(BUILTIN_THEMES.len(), 8);
        let names: Vec<&str> = builtin_theme_names().collect();
        assert!(names.contains(&"zinc"));
        assert!(names.contains(&"empire"));
        assert!(names.contains(&"phosphor"));
        assert!(names.contains(&"tokyo-night-storm"));
        assert!(names.contains(&"catppuccin-latte"));
        assert!(names.contains(&"dracula"));
        assert!(names.contains(&"rose-pine"));
        assert!(names.contains(&"deep-ocean"));
    }

    #[test]
    fn test_theme_serialize_roundtrip() {
        let original = load_theme("empire");
        let toml_str = export_theme_toml(&original).unwrap();
        let loaded: Theme = toml::from_str(&toml_str).unwrap();

        assert_eq!(original.background, loaded.background);
        assert_eq!(original.title, loaded.title);
        assert_eq!(original.running, loaded.running);
        assert_eq!(original.error, loaded.error);
        assert_eq!(original.diff_add, loaded.diff_add);
        assert_eq!(original.sandbox, loaded.sandbox);
    }

    #[test]
    fn test_theme_toml_format() {
        let theme = load_theme("empire");
        let toml_str = export_theme_toml(&theme).unwrap();

        assert!(toml_str.contains(r##"background = "#0f172a""##));
        assert!(toml_str.contains(r##"title = "#fbbf24""##));
        assert!(toml_str.contains(r##"running = "#22c55e""##));
    }

    #[test]
    fn test_load_custom_theme_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let theme_path = dir.path().join("my-theme.toml");
        let toml_str = export_theme_toml(&load_theme("dracula")).unwrap();
        std::fs::write(&theme_path, &toml_str).unwrap();

        let loaded = load_custom_theme(&theme_path).unwrap();
        assert_eq!(loaded.background, Color::Rgb(40, 42, 54));
        assert_eq!(loaded.title, Color::Rgb(189, 147, 249));
    }

    #[test]
    fn test_load_custom_theme_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        let theme_path = dir.path().join("bad.toml");
        std::fs::write(&theme_path, "not valid theme data").unwrap();

        assert!(load_custom_theme(&theme_path).is_none());
    }

    #[test]
    fn test_discover_custom_themes_empty() {
        // With no themes dir, should return empty
        let themes = discover_custom_themes();
        // May or may not be empty depending on test environment, just check it doesn't panic
        let _ = themes;
    }

    #[test]
    fn test_discover_custom_themes_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let themes_dir = dir.path().join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();

        // Write two valid theme files
        let dracula_toml = export_theme_toml(&load_theme("dracula")).unwrap();
        std::fs::write(themes_dir.join("my-dark.toml"), &dracula_toml).unwrap();
        std::fs::write(themes_dir.join("my-light.toml"), &dracula_toml).unwrap();
        // Write a non-toml file (should be ignored)
        std::fs::write(themes_dir.join("readme.txt"), "not a theme").unwrap();

        // Can't easily test discover_custom_themes() since it uses get_app_dir(),
        // but we can test the file parsing directly
        let loaded = load_custom_theme(&themes_dir.join("my-dark.toml"));
        assert!(loaded.is_some());
    }

    #[test]
    fn test_available_themes_includes_builtins() {
        let themes = available_themes();
        assert!(themes.len() >= 5);
        assert!(themes.contains(&"empire".to_string()));
        assert!(themes.contains(&"phosphor".to_string()));
        assert!(themes.contains(&"tokyo-night-storm".to_string()));
        assert!(themes.contains(&"catppuccin-latte".to_string()));
        assert!(themes.contains(&"dracula".to_string()));
    }

    #[test]
    fn test_all_builtin_themes_roundtrip() {
        for name in builtin_theme_names() {
            let theme = load_theme(name);
            let toml_str = export_theme_toml(&theme)
                .unwrap_or_else(|e| panic!("{} export failed: {}", name, e));
            let _loaded: Theme = toml::from_str(&toml_str)
                .unwrap_or_else(|e| panic!("{} roundtrip failed: {}", name, e));
        }
    }

    #[test]
    fn test_custom_theme_toml_parsing() {
        let toml_str = r##"
background = "#1a1b26"
border = "#414868"
terminal_border = "#7aa2f7"
selection = "#283457"
session_selection = "#414868"
title = "#c0caf5"
text = "#a9b1d6"
dimmed = "#565f89"
hint = "#565f89"
running = "#9ece6a"
waiting = "#e0af68"
idle = "#565f89"
error = "#f7768e"
terminal_active = "#7aa2f7"
group = "#7dcfff"
search = "#bb9af7"
accent = "#7aa2f7"
diff_add = "#9ece6a"
diff_delete = "#f7768e"
diff_modified = "#e0af68"
diff_header = "#7dcfff"
help_key = "#e0af68"
branch = "#7dcfff"
sandbox = "#bb9af7"
"##;
        let theme: Theme = toml::from_str(toml_str).unwrap();
        assert_eq!(theme.background, Color::Rgb(26, 27, 38));
        assert_eq!(theme.title, Color::Rgb(192, 202, 245));
        assert_eq!(theme.running, Color::Rgb(158, 206, 106));
    }

    #[test]
    fn test_custom_theme_partial_uses_defaults() {
        let toml_str = r##"
background = "#1a1b26"
border = "#414868"
"##;
        // Missing fields fall back to empire defaults (forward-compatible)
        let theme: Theme = toml::from_str(toml_str).unwrap();
        assert_eq!(theme.background, Color::Rgb(26, 27, 38));
        assert_eq!(theme.border, Color::Rgb(65, 72, 104));
        // Missing fields get empire defaults
        assert_eq!(theme.title, load_theme("empire").title);
        assert_eq!(theme.running, load_theme("empire").running);
    }

    #[test]
    fn test_builtin_name_ignored_in_custom_dir() {
        let dir = tempfile::tempdir().unwrap();

        // Simulate a custom theme file named after a builtin
        let path = dir.path().join("empire.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "").unwrap();

        // The file can be loaded directly, but discover_custom_themes
        // filters out builtin names. We test the filter logic here.
        assert!(is_builtin_theme("empire"));
    }
}

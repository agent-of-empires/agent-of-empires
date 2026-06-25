//! Server-side theme projections.
//!
//! `ResolvedTheme` is the payload the web dashboard consumes from
//! `GET /api/themes/:name` and `GET /api/theme/current`. It is derived
//! from the canonical [`Theme`] (loaded from a builtin TOML or a custom
//! TOML in `~/.agent-of-empires/themes/*.toml`) by:
//!
//! - emitting the named TUI color fields as CSS variables the web's
//!   Tailwind tokens consume (`--color-surface-900`, `--color-text-primary`,
//!   etc.),
//! - deriving lifted/recessed shades from the background luminance,
//! - deriving an ANSI 16 palette from the semantic color fields so the
//!   embedded terminal repaints under user themes without an extra schema
//!   field per theme,
//! - resolving the `[syntax].shiki_theme` selection (with appearance-based
//!   fallback when none is declared).
//!
//! `ResolvedTheme` is never persisted. It's a serialization of the
//! projection logic the web needs at runtime.

use std::collections::BTreeMap;

use ratatui::style::Color;
use serde::Serialize;
use tracing::debug;

use super::{contrast::contrast_ratio as wcag_contrast_ratio, load_theme, Theme, ThemeAppearance};

/// Source classification for a resolved theme. Frontends use this to
/// label the picker entry (e.g. "(custom)" vs the builtin name) and to
/// decide whether unknown-theme fallback paths fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolvedThemeSource {
    Builtin,
    Custom,
    /// Contributed by an active plugin's manifest.
    Plugin,
    /// The requested theme name didn't match any builtin, custom, or plugin
    /// theme; the resolver returned the `default` builtin as a safety net.
    Fallback,
}

/// CSS-variable map for one surface (web chrome or embedded terminal).
/// Wrapped so the JSON shape is `{ "cssVars": { ... } }` instead of a
/// bare map, which gives room to add per-surface metadata (e.g. a
/// `colorScheme` hint) later without breaking the wire format.
#[derive(Debug, Clone, Serialize)]
pub struct CssVarProjection {
    #[serde(rename = "cssVars")]
    pub css_vars: BTreeMap<String, String>,
}

/// Syntax-highlighter projection. Web loads the named shiki theme on
/// theme switch.
#[derive(Debug, Clone, Serialize)]
pub struct SyntaxProjection {
    #[serde(rename = "shikiTheme")]
    pub shiki_theme: String,
}

/// Full resolved theme payload for the web. JSON shape (subset):
///
/// ```json
/// {
///   "name": "empire",
///   "source": "builtin",
///   "appearance": "dark",
///   "web": { "cssVars": { "--color-surface-900": "#0f172a", ... } },
///   "terminal": { "cssVars": { "--term-bg": "#0f172a", ... } },
///   "syntax": { "shikiTheme": "github-dark" }
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedTheme {
    pub name: String,
    pub source: ResolvedThemeSource,
    pub appearance: ThemeAppearance,
    pub web: CssVarProjection,
    pub terminal: CssVarProjection,
    pub syntax: SyntaxProjection,
}

/// Resolve a theme name into the full projection. Always succeeds:
/// unknown names fall back to the `default` builtin (matching
/// `load_theme`'s behaviour) and the returned `source` reports
/// `Fallback` so the frontend can surface that.
pub fn resolve_theme(name: &str) -> ResolvedTheme {
    debug!("resolve_theme enter name={}", name);
    let theme = load_theme(name);
    debug!("resolve_theme: load_theme returned");
    let source = classify_source(name);
    debug!("resolve_theme: classify_source -> {:?}", source);
    let resolved_name = if matches!(source, ResolvedThemeSource::Fallback) {
        "zinc".to_string()
    } else {
        name.to_string()
    };
    let appearance = resolved_appearance(&theme);
    debug!("resolve_theme: appearance -> {:?}", appearance);
    let syntax = syntax_projection(&theme, appearance);
    debug!(
        "resolved theme projection name={} source={:?} appearance={:?} shiki_theme={}",
        resolved_name, source, appearance, syntax.shiki_theme
    );
    let web = web_projection(&theme, appearance);
    debug!(
        "resolve_theme: web projection done ({} vars)",
        web.css_vars.len()
    );
    let terminal = terminal_projection(&theme, appearance);
    debug!(
        "resolve_theme: terminal projection done ({} vars)",
        terminal.css_vars.len()
    );
    ResolvedTheme {
        name: resolved_name,
        source,
        appearance,
        web,
        terminal,
        syntax,
    }
}

fn classify_source(name: &str) -> ResolvedThemeSource {
    if super::is_builtin_theme(name) {
        return ResolvedThemeSource::Builtin;
    }
    if super::discover_custom_themes()
        .iter()
        .any(|(n, _)| n == name)
    {
        return ResolvedThemeSource::Custom;
    }
    if super::discover_plugin_themes()
        .iter()
        .any(|(n, _)| n == name)
    {
        return ResolvedThemeSource::Plugin;
    }
    ResolvedThemeSource::Fallback
}

fn resolved_appearance(theme: &Theme) -> ThemeAppearance {
    if let Some(a) = theme.appearance {
        return a;
    }
    if relative_luminance(theme.background) >= 0.5 {
        ThemeAppearance::Light
    } else {
        ThemeAppearance::Dark
    }
}

fn web_projection(theme: &Theme, appearance: ThemeAppearance) -> CssVarProjection {
    let mut css = BTreeMap::new();
    let bg = theme.background;

    // Named surfaces derived from background luminance. Dark themes
    // get a deeper/lighter ramp by mixing toward black/white; light
    // themes invert (recess toward grey, elevate toward white). See
    // mix() and DESIGN.md for the rationale.
    let (deeper, elevated_1, elevated_2) = match appearance {
        ThemeAppearance::Dark => (
            mix(bg, BLACK, 0.45),
            mix(bg, WHITE, 0.05),
            mix(bg, WHITE, 0.08),
        ),
        ThemeAppearance::Light => (
            mix(bg, BLACK, 0.05),
            mix(bg, WHITE, 0.35),
            mix(bg, BLACK, 0.03),
        ),
    };
    let (surface_600, surface_500) = match appearance {
        ThemeAppearance::Dark => (mix(theme.border, WHITE, 0.1), mix(theme.border, WHITE, 0.2)),
        ThemeAppearance::Light => (
            mix(theme.border, BLACK, 0.08),
            mix(theme.border, BLACK, 0.16),
        ),
    };
    css.insert("--color-surface-950".into(), hex(deeper));
    css.insert("--color-surface-900".into(), hex(bg));
    css.insert("--color-surface-850".into(), hex(elevated_1));
    css.insert("--color-surface-800".into(), hex(elevated_2));
    css.insert("--color-surface-700".into(), hex(theme.border));
    css.insert("--color-surface-600".into(), hex(surface_600));
    css.insert("--color-surface-500".into(), hex(surface_500));

    // Brand ramp anchored on theme.accent. Dark themes use Tailwind's
    // usual light-to-dark progression so brand-100 reads on brand-900
    // button bodies. Light themes invert the ramp for the same utility
    // pair; translucent brand-900 bodies composite over light surfaces,
    // so their foreground needs to come from the dark end.
    let accent = theme.accent;
    let brand_ramp = match appearance {
        ThemeAppearance::Dark => [
            mix(accent, WHITE, 0.8),
            mix(accent, WHITE, 0.65),
            mix(accent, WHITE, 0.45),
            mix(accent, WHITE, 0.2),
            accent,
            mix(accent, BLACK, 0.15),
            mix(accent, BLACK, 0.3),
            mix(accent, BLACK, 0.45),
            mix(accent, BLACK, 0.55),
        ],
        ThemeAppearance::Light => [
            mix(accent, BLACK, 0.85),
            mix(accent, BLACK, 0.7),
            mix(accent, BLACK, 0.55),
            mix(accent, BLACK, 0.35),
            accent,
            mix(accent, WHITE, 0.2),
            mix(accent, WHITE, 0.4),
            mix(accent, WHITE, 0.6),
            mix(accent, WHITE, 0.8),
        ],
    };
    for (step, color) in [100, 200, 300, 400, 500, 600, 700, 800, 900]
        .into_iter()
        .zip(brand_ramp)
    {
        css.insert(format!("--color-brand-{step}"), hex(color));
    }
    css.insert(
        "--color-text-on-brand".into(),
        hex(readable_on(brand_ramp[5])),
    );

    // Accent ramp anchored on theme.terminal_border (the existing
    // teal-style anchor used by the TUI's accent surface), so secondary
    // affordances like branch chips still read as the theme's secondary
    // hue.
    let secondary = theme.terminal_border;
    css.insert("--color-accent-500".into(), hex(secondary));
    css.insert(
        "--color-accent-600".into(),
        hex(mix(secondary, BLACK, 0.15)),
    );
    css.insert("--color-accent-700".into(), hex(mix(secondary, BLACK, 0.3)));

    // Text ramp. text-bright is the most readable; dim is the WCAG-AA
    // floor for descriptive copy (validated by the issue #1105 work).
    css.insert("--color-text-primary".into(), hex(theme.text));
    css.insert("--color-text-secondary".into(), hex(theme.hint));
    css.insert("--color-text-muted".into(), hex(theme.hint));
    css.insert("--color-text-dim".into(), hex(theme.dimmed));
    css.insert("--color-text-bright".into(), hex(theme.title));

    // Status colors map directly onto the TUI's semantic fields.
    // starting + stopped don't have TUI equivalents; derive from
    // waiting + dimmed so they still pulse against the picked theme.
    css.insert("--color-status-running".into(), hex(theme.running));
    css.insert("--color-status-waiting".into(), hex(theme.waiting));
    css.insert("--color-status-warning".into(), hex(theme.waiting));
    css.insert("--color-status-fresh-idle".into(), hex(theme.fresh_idle));
    css.insert("--color-status-idle".into(), hex(theme.idle));
    css.insert("--color-status-unread".into(), hex(theme.unread));
    css.insert("--color-status-error".into(), hex(theme.error));
    css.insert(
        "--color-status-starting".into(),
        hex(mix(theme.waiting, BLACK, 0.1)),
    );
    css.insert(
        "--color-status-stopped".into(),
        hex(mix(theme.dimmed, BLACK, 0.1)),
    );

    // Diff + extras. Diff tokens are exposed even though the current
    // diff cards use ad-hoc Tailwind classes; web can migrate to them
    // incrementally without server changes.
    css.insert("--color-diff-add".into(), hex(theme.diff_add));
    css.insert("--color-diff-delete".into(), hex(theme.diff_delete));
    css.insert("--color-diff-modified".into(), hex(theme.diff_modified));
    css.insert("--color-diff-header".into(), hex(theme.diff_header));
    css.insert("--color-selection".into(), hex(theme.selection));
    css.insert(
        "--color-session-selection".into(),
        hex(theme.session_selection),
    );
    css.insert("--color-terminal-active".into(), hex(theme.terminal_active));
    css.insert("--color-branch".into(), hex(theme.branch));
    css.insert("--color-sandbox".into(), hex(theme.sandbox));

    CssVarProjection { css_vars: css }
}

fn terminal_projection(theme: &Theme, appearance: ThemeAppearance) -> CssVarProjection {
    let mut css = BTreeMap::new();
    css.insert("--term-bg".into(), hex(theme.background));
    css.insert("--term-fg".into(), hex(theme.text));
    css.insert("--term-cursor".into(), hex(theme.accent));
    css.insert("--term-selection-bg".into(), rgba(theme.hint, 0.35));

    // Derived ANSI 16 palette. Maps semantic fields to the standard
    // ANSI slots (red=error, green=running, etc.) and lifts each by
    // ~20% mixed toward white (dark themes) or black (light themes)
    // for the bright variants. Not aesthetically perfect for every
    // user theme, but it ensures the terminal honours the picked
    // palette without forcing users to declare 16 hexes per theme.
    let lift = match appearance {
        ThemeAppearance::Dark => WHITE,
        ThemeAppearance::Light => BLACK,
    };
    let lift_amt = 0.2;

    let base = [
        ("--term-color-0", theme.background),
        ("--term-color-1", theme.error),
        ("--term-color-2", theme.running),
        ("--term-color-3", theme.waiting),
        ("--term-color-4", theme.branch),
        ("--term-color-5", theme.accent),
        ("--term-color-6", theme.terminal_active),
        ("--term-color-7", theme.text),
    ];
    for (name, color) in base {
        css.insert(name.into(), hex(color));
    }
    css.insert("--term-color-8".into(), hex(theme.dimmed));
    let bright = [
        ("--term-color-9", theme.error),
        ("--term-color-10", theme.running),
        ("--term-color-11", theme.waiting),
        ("--term-color-12", theme.branch),
        ("--term-color-13", theme.accent),
        ("--term-color-14", theme.terminal_active),
    ];
    for (name, color) in bright {
        css.insert(name.into(), hex(mix(color, lift, lift_amt)));
    }
    css.insert("--term-color-15".into(), hex(theme.title));

    CssVarProjection { css_vars: css }
}

fn syntax_projection(theme: &Theme, appearance: ThemeAppearance) -> SyntaxProjection {
    let shiki_theme = theme
        .syntax
        .shiki_theme
        .clone()
        .unwrap_or_else(|| match appearance {
            ThemeAppearance::Dark => "github-dark".to_string(),
            ThemeAppearance::Light => "github-light".to_string(),
        });
    SyntaxProjection { shiki_theme }
}

// --- color math ---

const BLACK: Color = Color::Rgb(0, 0, 0);
const WHITE: Color = Color::Rgb(255, 255, 255);

fn rgb_components(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        // Non-RGB colors should never reach the projection (the API
        // loads themes in truecolor mode, never palette). Treat as
        // black for safety.
        _ => (0, 0, 0),
    }
}

fn hex(c: Color) -> String {
    let (r, g, b) = rgb_components(c);
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

/// Linear-channel mix of two RGB colors. `t = 0` returns `a`, `t = 1`
/// returns `b`. Not gamma-correct; that's fine for chrome derivation
/// where the goal is visible separation, not perceptual uniformity.
fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (ar, ag, ab) = rgb_components(a);
    let (br, bg, bb) = rgb_components(b);
    let lerp = |x: u8, y: u8| -> u8 {
        let result = (x as f32) * (1.0 - t) + (y as f32) * t;
        result.round().clamp(0.0, 255.0) as u8
    };
    Color::Rgb(lerp(ar, br), lerp(ag, bg), lerp(ab, bb))
}

fn rgba(c: Color, alpha: f32) -> String {
    let (r, g, b) = rgb_components(c);
    format!("rgba({r}, {g}, {b}, {:.2})", alpha.clamp(0.0, 1.0))
}

fn readable_on(bg: Color) -> Color {
    if contrast_ratio(BLACK, bg) >= contrast_ratio(WHITE, bg) {
        BLACK
    } else {
        WHITE
    }
}

fn contrast_ratio(a: Color, b: Color) -> f32 {
    wcag_contrast_ratio(a, b).unwrap_or(0.0)
}

/// Rec. 601 relative luminance (0.0 to 1.0). Coarser than WCAG's
/// gamma-corrected formula but adequate for the dark/light split: only
/// custom themes with mid-tone backgrounds (around 0.5) sit near the
/// cutoff, and those are the ones that should declare `appearance`
/// explicitly anyway.
fn relative_luminance(c: Color) -> f32 {
    let (r, g, b) = rgb_components(c);
    (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    use super::*;
    use crate::tui::styles::builtin_theme_names;

    #[test]
    fn resolve_all_builtins() {
        for name in builtin_theme_names() {
            let r = resolve_theme(name);
            assert_eq!(r.name, name);
            assert_eq!(r.source, ResolvedThemeSource::Builtin);
            assert!(
                r.web.css_vars.contains_key("--color-surface-900"),
                "{name}: web projection missing surface-900"
            );
            assert!(
                r.terminal.css_vars.contains_key("--term-bg"),
                "{name}: terminal projection missing term-bg"
            );
            assert!(
                !r.syntax.shiki_theme.is_empty(),
                "{name}: shiki theme empty"
            );
        }
    }

    #[test]
    fn catppuccin_latte_resolves_as_light() {
        let r = resolve_theme("catppuccin-latte");
        assert_eq!(r.appearance, ThemeAppearance::Light);
        assert_eq!(r.syntax.shiki_theme, "catppuccin-latte");
    }

    #[test]
    fn dracula_resolves_as_dark_with_shiki_dracula() {
        let r = resolve_theme("dracula");
        assert_eq!(r.appearance, ThemeAppearance::Dark);
        assert_eq!(r.syntax.shiki_theme, "dracula");
    }

    #[test]
    fn unknown_theme_resolves_to_fallback() {
        let r = resolve_theme("does-not-exist");
        assert_eq!(r.source, ResolvedThemeSource::Fallback);
        assert_eq!(r.name, "zinc");
    }

    #[test]
    fn css_vars_are_valid_hex() {
        for name in builtin_theme_names() {
            let r = resolve_theme(name);
            for (key, value) in r.web.css_vars.iter().chain(r.terminal.css_vars.iter()) {
                if key == "--term-selection-bg" {
                    assert!(
                        value.starts_with("rgba(") && value.ends_with(')'),
                        "{name}: var {key} = {value} not rgba(...)"
                    );
                    continue;
                }
                assert!(
                    value.starts_with('#') && value.len() == 7,
                    "{name}: var {key} = {value} not a #rrggbb"
                );
                assert!(
                    value
                        .chars()
                        .skip(1)
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                    "{name}: var {key} = {value} contains non-hex or uppercase",
                );
            }
        }
    }

    #[test]
    fn web_semantic_color_utilities_have_resolved_vars() {
        let vars = web_semantic_color_vars_used_by_dashboard();
        assert!(
            vars.contains("--color-brand-100")
                && vars.contains("--color-brand-200")
                && vars.contains("--color-brand-300")
                && vars.contains("--color-brand-900")
                && vars.contains("--color-surface-500")
                && vars.contains("--color-surface-600"),
            "test fixture did not observe the known web token gaps: {vars:?}"
        );

        for name in builtin_theme_names() {
            let r = resolve_theme(name);
            for var in &vars {
                assert!(
                    r.web.css_vars.contains_key(var),
                    "{name}: web source uses {var}, but ResolvedTheme does not project it"
                );
            }
        }
    }

    #[test]
    fn light_theme_inverts_surface_ramp_direction() {
        // For a dark theme, surface-950 should be DARKER than surface-900
        // (background); for a light theme, surface-950 should be SLIGHTLY
        // DARKER than surface-900 as a recessed edge, while surface-850
        // / surface-800 lift toward white. Sanity-check the ordering by
        // luminance on Catppuccin Latte vs Empire.
        let light = resolve_theme("catppuccin-latte");
        let bg_light = parse_hex(light.web.css_vars.get("--color-surface-900").unwrap());
        let elevated_light = parse_hex(light.web.css_vars.get("--color-surface-850").unwrap());
        assert!(
            luminance_of_hex(&elevated_light) >= luminance_of_hex(&bg_light),
            "light theme: surface-850 ({elevated_light}) should be >= surface-900 ({bg_light})"
        );

        let dark = resolve_theme("empire");
        let bg_dark = parse_hex(dark.web.css_vars.get("--color-surface-900").unwrap());
        let deeper_dark = parse_hex(dark.web.css_vars.get("--color-surface-950").unwrap());
        assert!(
            luminance_of_hex(&deeper_dark) <= luminance_of_hex(&bg_dark),
            "dark theme: surface-950 ({deeper_dark}) should be <= surface-900 ({bg_dark})"
        );
    }

    #[test]
    fn brand_button_pair_keeps_contrast_in_light_theme() {
        let theme = resolve_theme("catppuccin-latte");
        let fg = color_from_hex(theme.web.css_vars.get("--color-brand-100").unwrap());
        let bg = color_from_hex(theme.web.css_vars.get("--color-brand-900").unwrap());
        let surface = color_from_hex(theme.web.css_vars.get("--color-surface-900").unwrap());
        let composited_bg = composite(bg, surface, 0.4);

        assert!(
            contrast_ratio(fg, composited_bg) >= 4.5,
            "catppuccin-latte: text-brand-100 must remain readable on bg-brand-900/40"
        );
    }

    #[test]
    fn on_brand_token_keeps_contrast_for_all_builtins() {
        for name in builtin_theme_names() {
            let theme = resolve_theme(name);
            let fg = color_from_hex(theme.web.css_vars.get("--color-text-on-brand").unwrap());
            let bg = color_from_hex(theme.web.css_vars.get("--color-brand-600").unwrap());
            assert!(
                contrast_ratio(fg, bg) >= 4.5,
                "{name}: color-text-on-brand must remain readable on brand-600"
            );
        }
    }

    #[test]
    fn on_brand_token_uses_wcag_contrast_for_custom_mid_amber() {
        let theme = Theme {
            accent: Color::Rgb(0xb6, 0x76, 0x08),
            ..Theme::default()
        };

        let projection = web_projection(&theme, ThemeAppearance::Dark);
        let fg = color_from_hex(projection.css_vars.get("--color-text-on-brand").unwrap());
        let bg = color_from_hex(projection.css_vars.get("--color-brand-600").unwrap());

        assert_eq!(fg, WHITE);
        assert!(
            contrast_ratio(BLACK, bg) < 4.5,
            "regression fixture should keep black below AA contrast"
        );
        assert!(
            contrast_ratio(fg, bg) >= 4.5,
            "custom amber text-on-brand must use the WCAG-readable foreground"
        );
    }

    fn parse_hex(s: &str) -> String {
        s.to_string()
    }
    fn luminance_of_hex(hex: &str) -> f32 {
        let s = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&s[0..2], 16).unwrap();
        let g = u8::from_str_radix(&s[2..4], 16).unwrap();
        let b = u8::from_str_radix(&s[4..6], 16).unwrap();
        relative_luminance(Color::Rgb(r, g, b))
    }

    fn color_from_hex(hex: &str) -> Color {
        let s = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&s[0..2], 16).unwrap();
        let g = u8::from_str_radix(&s[2..4], 16).unwrap();
        let b = u8::from_str_radix(&s[4..6], 16).unwrap();
        Color::Rgb(r, g, b)
    }

    fn composite(fg: Color, bg: Color, alpha: f32) -> Color {
        let (fr, fg_g, fb) = rgb_components(fg);
        let (br, bg_g, bb) = rgb_components(bg);
        Color::Rgb(
            composite_channel(fr, br, alpha),
            composite_channel(fg_g, bg_g, alpha),
            composite_channel(fb, bb, alpha),
        )
    }

    fn composite_channel(fg: u8, bg: u8, alpha: f32) -> u8 {
        ((fg as f32 * alpha) + (bg as f32 * (1.0 - alpha))).round() as u8
    }

    fn web_semantic_color_vars_used_by_dashboard() -> BTreeSet<String> {
        let mut vars = BTreeSet::new();
        let web_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("web/src");
        collect_web_semantic_color_vars(&web_src, &mut vars);
        vars
    }

    fn collect_web_semantic_color_vars(path: &Path, vars: &mut BTreeSet<String>) {
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                collect_web_semantic_color_vars(&entry.unwrap().path(), vars);
            }
            return;
        }

        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            return;
        };
        if !matches!(ext, "css" | "ts" | "tsx") {
            return;
        }

        let content = fs::read_to_string(path).unwrap();
        collect_css_var_references(&content, vars);
        collect_tailwind_color_utilities(&content, vars);
    }

    fn collect_css_var_references(content: &str, vars: &mut BTreeSet<String>) {
        let mut offset = 0;
        while let Some(pos) = content[offset..].find("--color-") {
            let start = offset + pos;
            let end = content[start..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .map_or(content.len(), |rel| start + rel);
            let name = &content[start..end];
            if semantic_color_name(name.trim_start_matches("--color-")) {
                vars.insert(name.to_string());
            }
            offset = end;
        }
    }

    fn collect_tailwind_color_utilities(content: &str, vars: &mut BTreeSet<String>) {
        for raw in content.split(|c: char| {
            !(c.is_ascii_alphanumeric() || matches!(c, '-' | ':' | '/' | '_' | '[' | ']' | '.'))
        }) {
            let utility = raw
                .rsplit(':')
                .next()
                .unwrap_or(raw)
                .trim_start_matches('!');
            for prefix in [
                "bg-",
                "text-",
                "border-",
                "ring-",
                "outline-",
                "divide-",
                "from-",
                "via-",
                "to-",
                "fill-",
                "stroke-",
                "caret-",
                "decoration-",
                "shadow-",
            ] {
                if let Some(color) = utility.strip_prefix(prefix) {
                    let color = color.split('/').next().unwrap_or(color);
                    if semantic_color_name(color) {
                        vars.insert(format!("--color-{color}"));
                    }
                }
            }
        }
    }

    fn semantic_color_name(name: &str) -> bool {
        name.starts_with("brand-")
            || name.starts_with("accent-")
            || name.starts_with("surface-")
            || name.starts_with("text-")
            || name.starts_with("status-")
            || name.starts_with("diff-")
            || matches!(
                name,
                "text-on-brand"
                    | "selection"
                    | "session-selection"
                    | "terminal-active"
                    | "branch"
                    | "sandbox"
            )
    }
}

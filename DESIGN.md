# Agent of Empires design system

This is the visual source of truth for the TUI, web dashboard, and marketing
site. Color is shared by the TUI and dashboard through theme TOML. Typography,
density, and motion differ by surface.

## Direction

AoE is a dense developer tool with an industrial, warm identity. It should feel
terminal-native and deliberately crafted, with amber or copper accents and
restrained decoration.

Avoid generic developer-tool styling: purple gradients, centered page layouts,
uniform icon-card grids, decorative motion, and excessive rounding.

## Marketing site

- Display: Satoshi, weights 600 to 900.
- Body and UI: DM Sans, weights 400 to 600.
- Code and metadata: JetBrains Mono, weights 400 to 500.
- Type scale: 11, 12, 13, 14, 16, 18, 20, 24, 32, 48, 56, 64, 80px.
- Content widths: 1200px for layouts, 720px for prose.
- Spacing uses a 4px base and the 2, 4, 8, 16, 24, 32, 48, 64px scale.
- Use left-aligned editorial layouts and asymmetric feature grids.
- Use 4, 8, and 12px radii; reserve pills for badges.

The brand palette is amber or copper against warm navy, with muted teal as a
secondary accent:

| Role | Main | Supporting |
| --- | --- | --- |
| Brand | `#d97706` | `#fbbf24`, `#b45309`, `#92400e` |
| Accent | `#0d9488` | `#14b8a6`, `#0f766e` |
| Dark surfaces | `#0f172a` | `#020617`, `#1e293b`, `#334155` |
| Light surfaces | `#f8fafc` | `#ffffff`, `#e2e8f0` |
| Success | `#22c55e` | |
| Warning | `#f59e0b` | |
| Error | `#ef4444` | |

Motion is functional: 75ms for micro feedback, 150ms for short transitions,
and 300ms for larger state changes. Use ease-out for entrance and ease-in for
exit. Do not add decorative animation.

The logo is a pair of stacked amber terminal windows. Keep its colors fixed to
the brand even when product surfaces use a different theme.

## Theme system

Themes are flat TOML palettes. Builtins live in `themes/builtin/`; user themes
live in the app directory under `themes/`. The schema and loader are in
`src/tui/styles/themes.rs`. Optional metadata selects dark or light appearance
and a Shiki syntax theme.

The TUI reads `Theme` directly. The server maps it to `ResolvedTheme`, whose CSS
variables drive dashboard surfaces, semantic status colors, syntax highlighting,
and terminal ANSI colors. Derived ramps follow background luminance and invert
for light themes. Do not add separate CSS theme blocks or hardcoded dashboard
chrome colors.

Adding a builtin theme requires its TOML file and one `BUILTIN_THEMES` entry.
Keep optional fields backwards-compatible for user themes. ANSI colors are
derived from semantic fields rather than declared separately.

The marketing site keeps the brand palette above and does not follow the user's
theme.

## TUI

- Preserve dashboard density and the status glyph system.
- Keep runtime status separate from shortcuts; Tips yield to critical hints and compact to their icon when space is tight.
- Show the model in use in the structured view status line beside usage and toggles, read from the session's `Model` config option so it follows a model switch instead of a fixed label.
- Use rounded block borders.
- Keep one character of horizontal padding in the list and preview panels.
- Adjacent list and preview panels share one separator, never a double border.
- Passive split panes only observe: use a reliable captured cursor to choose the viewport without resizing shared panes or discarding captured history.
- Dialogs remain boxed and manage their own internal spacing.
- Prefer geometric text symbols over emoji where column width must be stable.

## Web dashboard

The dashboard is a keyboard-driven utility around terminal and conversation
content. It is denser and quieter than the marketing site.

- Use self-hosted Geist Sans for headings and body copy, and Geist Mono for
  session names, paths, status glyphs, shortcuts, and data.
- Rows are 28 to 32px; buttons are 32 to 40px.
- Use 6px radii for controls and 8px for panels and dialogs. Do not add larger
  radii.
- Use semantic theme tokens such as `bg-surface-900`,
  `text-status-running`, and `border-surface-700`.
- All body text must meet WCAG AA contrast against its surface.
- The open session's sidebar row must meet WCAG's 3:1 non-text contrast
  against every surface its indicator can sit on. A surface-ramp step alone
  does not reach it, so the row uses the `session-active` token, which the
  projection lifts from the theme accent until it clears that floor.
- Prefer instant state changes or `transition-colors`. Named motion is limited
  to existing fade, slide, and terminal-cursor behavior.
- Fixed palette colors are allowed only when hue carries meaning, such as
  status, severity, syntax, user content, third-party visualizations, and the
  brand mark.
- Mobile prioritizes monitoring and conversation controls, after Muse and
  Claude's iOS app: the session list is home, and with a Session open it is a
  darker drawer that pushes the Session aside as a rounded card. Controls
  float (`float-control`: round, raised, soft shadow) over a page with no
  header rule; menus are frosted (`glass-menu`); a Session's views switch from
  the header's ⋯ so the agent keeps the screen; sheets replace persistent side
  panels. Bars at the top and bottom edges are fixed or sticky and opaque, so
  iOS 26 tints its bars from them instead of blurring.
- Sessions stay on the left. The right dock is the Session's review panel
  (Diff, Files, Background, plugins), with the terminal tabs stacked below
  them in the same dock, as in Conductor.
  Below 1100px the right dock overlays the main area as a sheet rather than
  squeezing a third column.
- On desktop the app is a near-black canvas (`surface-950`) holding cards: the
  agent pane and each dock group are `surface-900` cards with a 12px radius, a
  faint border and 8px gaps, as in Claude Code desktop. The header and sidebar
  sit on the canvas without borders. Tabs are plain text, the active one
  brighter; a count follows its label (`Diff 3`).
- The sidebar reads like Claude Code desktop's: **New session** and **Search**
  rows first, one View options menu by the Sessions label, quiet sentence-case
  section labels, inset rounded rows, and the open session as a soft filled
  pill with brighter text. The header names the open session as
  `project / title`.
- Follow the conventions of Claude Code desktop, VS Code and Conductor rather
  than inventing controls. The header toggles regions, not individual panes:
  a terminal icon, then the review panel's split button (panel icon toggles it,
  chevron opens a menu of its panes). An open toggle is tinted with the brand
  accent, not filled; tooltips name the action and its shortcut.

If a visual change requires an exception, update this document with the new
rule as part of the change.

# Web Dashboard Design System -- Agent of Empires

> Standalone design system for the web dashboard. The root DESIGN.md covers the TUI and marketing site; this document covers the browser-based app UI, which has its own visual direction.

## Product Context

- **What this is:** Browser-based dashboard for monitoring and controlling AI agent sessions
- **Classifier:** APP UI (workspace-driven, task-focused, data-dense)
- **Who it's for:** Developers managing parallel AI agents who want remote/mobile access
- **Competitors:** Conductor Build (native Mac app), Webmux (web-based tmux viewer)
- **Mood:** Clean, neutral, tool-like. Feels like Cursor or Conductor, not like a branded marketing site. The terminal is the hero; everything else stays out of the way.
- **What we avoid:** Pervasive brand colors, warm navy surfaces, orange/amber everywhere, decorative elements, AI slop patterns (purple gradients, 3-column icon grids, centered-everything layouts)

## Design Principles

1. **Terminal is the hero.** The terminal pane dominates the viewport. Everything else helps you find and interact with the right session.
2. **Density over chrome.** Show more sessions, less UI. Every pixel of border, padding, and decoration earns its space.
3. **Neutral dark.** Standard zinc grays, not warm navy or cold GitHub blue. Professional and unobtrusive.
4. **Restrained accent.** Amber (brand) and teal (accent) appear only at interaction points: terminal cursor, active indicators, status badges. Never on backgrounds or headers.
5. **Status at a glance.** Session state (running, waiting, idle, error) visible in peripheral vision. Color + shape for accessibility.
6. **Mobile is monitoring.** On mobile, you mostly watch. Sidebar becomes a session picker, terminal fills the screen.

## Typography

Geist Sans for UI text, Geist Mono for code, data, and terminal. Both from Vercel's Geist font family, designed for developer tools.

| Element                     | Font       | Size               | Weight                         |
| --------------------------- | ---------- | ------------------ | ------------------------------ |
| Header title                | Geist Mono | 12px               | 400                            |
| Session title (sidebar)     | Geist Sans | 13px               | 400                            |
| Session meta (tool, branch) | Geist Mono | 11px               | 400                            |
| Content header title        | Geist Mono | 14px               | 600                            |
| Status labels               | Geist Mono | 11px               | 400                            |
| Terminal                    | Geist Mono | 14px (12px mobile) | 400                            |
| Buttons                     | Geist Sans | 12px               | 500                            |
| Section labels              | Geist Mono | 11px               | 500, uppercase, tracking-wider |
| Body text                   | Geist Sans | 14px               | 400                            |

Font files are self-hosted in `public/fonts/` from the `geist` npm package. No external font CDN requests.

## Color

### Surfaces -- Neutral Zinc

| Token       | Hex     | Usage                                       |
| ----------- | ------- | ------------------------------------------- |
| surface-700 | #3f3f46 | Borders, dividers                           |
| surface-800 | #2c2c30 | Elevated surfaces (header, sidebar)         |
| surface-850 | #262629 | Slightly elevated (settings sidebar, nav)   |
| surface-900 | #1c1c1f | Primary background                          |
| surface-950 | #141416 | Deepest background (terminal, empty states) |

### Text

| Token          | Hex     | Usage                                   |
| -------------- | ------- | --------------------------------------- |
| text-primary   | #e4e4e7 | Primary body text (zinc-200)            |
| text-secondary | #a1a1aa | Secondary text, descriptions (zinc-400) |
| text-muted     | #71717a | Muted labels, hints (zinc-500)          |
| text-dim       | #52525b | Dimmest text, placeholders (zinc-600)   |
| text-bright    | #fafafa | Bright emphasis (zinc-50)               |

### Brand -- Amber/Copper (used sparingly)

| Token     | Hex     | Usage                                               |
| --------- | ------- | --------------------------------------------------- |
| brand-400 | #fbbf24 | Bright amber accents                                |
| brand-500 | #f59e0b | Hover states on brand elements                      |
| brand-600 | #d97706 | Terminal cursor, active sidebar border, focus rings |
| brand-700 | #b45309 | Pressed states                                      |

### Accent -- Muted Teal (used sparingly)

| Token      | Hex     | Usage                              |
| ---------- | ------- | ---------------------------------- |
| accent-500 | #14b8a6 | Bright teal                        |
| accent-600 | #0d9488 | Workspace name, diff chunk markers |
| accent-700 | #0f766e | Dark teal                          |

### Status

| Name     | Hex     | Glyph | Usage                  |
| -------- | ------- | ----- | ---------------------- |
| Running  | #22c55e | ●     | Agent actively working |
| Waiting  | #fbbf24 | ◐     | Waiting for user input |
| Idle     | #71717a | ○     | Agent idle             |
| Error    | #ef4444 | ✕     | Session error          |
| Starting | #f59e0b | ◌     | Session starting up    |
| Stopped  | #52525b | ■     | Session stopped        |

Status uses both color AND distinct glyphs for accessibility. The glyph shape should be recognizable even without color.

### Terminal Theme

| Token      | Hex                   | Notes                                               |
| ---------- | --------------------- | --------------------------------------------------- |
| background | #141416               | surface-950, deepest layer                          |
| foreground | #e4e4e7               | text-primary                                        |
| cursor     | #d97706               | Brand amber, the ONE place brand color is prominent |
| selection  | rgba(161,161,170,0.2) | Neutral zinc tint, not amber                        |
| ANSI blue  | #60a5fa               | Standard blue, not teal                             |
| ANSI cyan  | #22d3ee               | Standard cyan                                       |

## Layout

### Desktop (>1024px)

```
+------------------------------------------------------+
| [=] [icon] aoe          3 sessions  [errors] [diff]  |  <- 48px header
+--------+---------------------------------------------+
| ● Huns |  workspace/branch  ·  claude                |  <- 40px content header
|   claude|                                             |
|   2 sess|  ┌──────────────────────────────────────┐  |
|         |  │                                      │  |
| ◐ Goths|  │      [xterm.js terminal pane]        │  |
|   gemini|  │        fills remaining space         │  |
|         |  │                                      │  |
| ○ Celts|  │                                      │  |
|   claude|  └──────────────────────────────────────┘  |
+---------+--------------------------------------------+
  280px                    flex-1
```

- **Header:** 48px. Sidebar toggle, icon + "aoe" link (muted), session count, alert badges, diff toggle.
- **Sidebar:** 280px, resizable (200-480px). Two-line session items with status glyph, title, and meta line. Active item has left border in brand-600.
- **Content:** Flex-1. Content header (40px) + terminal (fills remaining).
- **Right panel:** Resizable diff + paired shell terminal, toggled by D key.

### Mobile (<768px)

Sidebar overlay. Right panel full-screen overlay. Terminal fills screen. Monitor-first.

## Components

### Session Item (Sidebar)

```
│ ● Huns                    │  <- glyph + title (13px)
│   claude · 2 sessions     │  <- meta (11px mono, dim)
```

- **Default:** transparent bg, border-l-2 border-transparent
- **Hover:** bg-surface-800/50
- **Active:** bg-surface-850, border-l-2 border-brand-600
- **Status glyph:** Text character, colored by status

### Header Branding

- Small icon (18px) + "aoe" in mono, linked to agent-of-empires.com
- Muted (text-muted), brightens on hover (text-secondary)
- Not prominent, just identifiable

### Empty States

- Terminal icon (48px, very dim)
- Primary message (14px, text-muted)
- Secondary hint (12px, text-dim)
- "No sessions" state includes CLI command in mono

### Buttons

- **Primary:** bg-brand-600 text-white, rounded-md (6px)
- **Ghost:** transparent, text-text-secondary, hover bg-surface-800
- **Size:** h-8, px-3
- **All:** cursor-pointer, 150ms transition

### Dialogs

- Rounded-lg (8px), not rounded-xl
- bg-surface-800, border-surface-700/30
- Backdrop: bg-black/60

## Spacing

- **Base unit:** 4px
- **Header height:** 48px (h-12)
- **Content header height:** 40px (h-10)
- **Sidebar width:** 280px default, 200-480px range
- **Session item padding:** px-3 py-2
- **Button height:** 32px (h-8)

## Border Radius Hierarchy

| Element                   | Radius | Class        |
| ------------------------- | ------ | ------------ |
| Buttons, inputs           | 6px    | rounded-md   |
| Dialogs                   | 8px    | rounded-lg   |
| Badges, pills             | 9999px | rounded-full |
| Sidebar, header, terminal | 0      | none         |
| Session items             | 0      | none         |

## Motion

Minimal-functional. This is a workspace tool.

- **Hover:** background-color 100ms ease
- **Selection:** instant
- **Dialog entrance:** slide-up 200ms ease-out
- **Status color change:** 300ms ease

## Anti-Patterns

1. **No pervasive brand color.** Amber appears on terminal cursor, active border, focus rings. Nowhere else.
2. **No cards in sidebar.** Sessions are list items, not cards.
3. **No colored section backgrounds.** Only the terminal gets a distinct (deeper) surface.
4. **No blue accents for interactive states.** Use neutral grays for hover/active.
5. **No pure white text.** Use text-primary (#e4e4e7), not #ffffff.
6. **No decorative elements.** Information density IS the design.
7. **No warm navy.** Zinc grays, not slate/navy.

## Decisions Log

| Date       | Decision                              | Rationale                                                                                                             |
| ---------- | ------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| 2026-04-08 | Initial web design system created     | Inherited warm navy direction from root DESIGN.md                                                                     |
| 2026-04-12 | Switched to neutral zinc grays        | Warm navy felt too branded for an app UI. User wants Conductor/Cursor feel, not a marketing aesthetic.                |
| 2026-04-12 | Geist Sans + Geist Mono               | Vercel's developer font family. More character than Inter, purpose-built for dev tools, mono variant pairs perfectly. |
| 2026-04-12 | Restrained amber accent               | Brand color was too pervasive. Now only on terminal cursor, active sidebar border, and focus rings.                   |
| 2026-04-12 | Two-line sidebar items                | Single-line cramming was hard to scan. Title + meta line creates clear hierarchy.                                     |
| 2026-04-12 | Distinct status glyphs                | Uniform dots relied entirely on color. Different shapes (●◐○✕◌■) add peripheral scannability and accessibility.       |
| 2026-04-12 | Header branding: icon + "aoe" link    | Anonymous header felt unfinished. Small, muted branding gives identity without being loud.                            |
| 2026-04-12 | Rich empty states                     | Bare text strings felt like placeholder UI. Icon + message + hint shows the app was designed with care.               |
| 2026-04-12 | Rounded-lg on dialogs, not rounded-xl | Tighter radius fits the neutral tool aesthetic. Rounded-xl felt too soft.                                             |

// Shared helpers for theme-switching story specs (#1405).
//
// Two domains:
//
//   - `seedCustomTheme(home, name, body)` writes a TOML file into the
//     isolated $HOME so that an `aoe serve` boot picks it up via
//     `discover_custom_themes` (src/tui/styles/mod.rs:86). The themes
//     directory lives at `${appDir}/themes/`, where `appDir` is
//     `${home}/.agent-of-empires-dev` (macOS/Windows) or
//     `${xdg}/agent-of-empires-dev/` (Linux). `appDirFor` in
//     `aoeServe.ts` already mirrors that resolution.
//
//   - `readThemeFromDocument(page)` snapshots the four observable
//     side-effects of `applyResolvedTheme` (web/src/lib/theme.ts:47):
//     `dataset.theme`, `dataset.themeAppearance`, the `color-scheme`
//     style, and one representative CSS variable. Used by the failure
//     and color-mode stories to assert a precise "did NOT paint" state.

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { appDirFor, resolveAoeBinary } from "./aoeServe";

const MINIMAL_THEME_TOML = `appearance = "dark"

background = "#11131c"
border = "#2b2f3f"
terminal_border = "#5fd7ff"
selection = "#2b2f3f"
session_selection = "#3c4154"
title = "#c4b5fd"
text = "#e6e8ee"
dimmed = "#7a829a"
hint = "#7a829a"
running = "#5fd7af"
waiting = "#ffd178"
fresh_idle = "#ff8a5b"
idle = "#7a829a"
error = "#ff6b6b"
terminal_active = "#5fd7ff"
group = "#5fd7ff"
search = "#fff59d"
accent = "#c4b5fd"
diff_add = "#5fd7af"
diff_delete = "#ff6b6b"
diff_modified = "#ffd178"
diff_header = "#c4b5fd"
help_key = "#c4b5fd"
branch = "#5fd7ff"
sandbox = "#c4b5fd"

[syntax]
shiki_theme = "github-dark"
`;

/** Body of a valid custom-theme TOML file. Custom themes deserialize as
 *  the full `Theme` struct (TUI palette + syntax sub-table); a partial
 *  body fails to parse and ends up filtered out by `load_custom_theme`. */
export const VALID_CUSTOM_THEME_TOML = MINIMAL_THEME_TOML;

/** Intentionally malformed TOML. Anything that doesn't satisfy the
 *  `Theme` struct deserializer counts; this body trips a parse error,
 *  so `load_custom_theme` returns `None` and the name is still listed
 *  by `discover_custom_themes` (the discovery scan only looks at the
 *  file stem, see src/tui/styles/mod.rs:97-108). */
export const MALFORMED_CUSTOM_THEME_TOML = `appearance = "dark"
this-is-not = "valid theme schema"
[syntax
shiki_theme = "missing closing bracket"
`;

export function customThemesDir(home: string, xdg: string): string {
  const appDir = appDirFor(home, xdg, resolveAoeBinary());
  return join(appDir, "themes");
}

/** Seed a `<name>.toml` file under the isolated app dir's `themes/`
 *  directory. Caller passes both `home` and the per-test XDG path; the
 *  resolution mirrors `appDirFor` so the same binary that the spec
 *  resolves will discover the file on boot. */
export function seedCustomTheme(home: string, xdg: string, name: string, body: string): string {
  const dir = customThemesDir(home, xdg);
  mkdirSync(dir, { recursive: true });
  const path = join(dir, `${name}.toml`);
  writeFileSync(path, body);
  return path;
}

export interface ThemeDocumentSnapshot {
  datasetTheme: string | undefined;
  datasetAppearance: string | undefined;
  colorScheme: string;
  surface900: string;
}

/** Snapshot the observable side-effects of `applyResolvedTheme` on the
 *  root element. Returning a strict shape (rather than asserting inline)
 *  lets the failure-mode specs compare a single object against a
 *  pre-change baseline without re-reading per-property. */
export async function readThemeFromDocument(page: Page): Promise<ThemeDocumentSnapshot> {
  return await page.evaluate(() => {
    const root = document.documentElement;
    return {
      datasetTheme: root.dataset.theme,
      datasetAppearance: root.dataset.themeAppearance,
      colorScheme: root.style.colorScheme,
      surface900: root.style.getPropertyValue("--color-surface-900").trim(),
    };
  });
}

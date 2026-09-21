// Custom theme fixtures (#1405).

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { appDirFor, resolveAoeBinary } from "./aoeServe";

/** Custom themes need the full Theme struct; partial files are filtered out. */
export const VALID_CUSTOM_THEME_TOML = `appearance = "dark"

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

/** Fails to parse, but discovery still lists the file stem. */
export const MALFORMED_CUSTOM_THEME_TOML = `appearance = "dark"
this-is-not = "valid theme schema"
[syntax
shiki_theme = "missing closing bracket"
`;

/** Write `<name>.toml` into the app dir's themes/ so the server discovers it on boot. */
export function seedCustomTheme(home: string, xdg: string, name: string, body: string): void {
  const dir = join(appDirFor(home, xdg, resolveAoeBinary()), "themes");
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, `${name}.toml`), body);
}

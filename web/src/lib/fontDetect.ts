// Client-side detection of installed monospace fonts, so the Terminal
// settings can list fonts the viewer actually has (e.g. a Nerd Font with the
// powerline/icon glyphs the bundled Geist Mono lacks). Uses the permission-
// free, cross-browser width-probe: a candidate font is installed iff a test
// string measured in `"<candidate>", <baseline>` differs from the baseline
// generic alone. queryLocalFonts() would enumerate everything, but it is
// Chromium-only and needs a permission prompt.
//
// caveat: probe only finds fonts on this curated list; the settings combobox
// stays free-text so any other installed font is still selectable by name.

const BASELINES = ["monospace", "serif", "sans-serif"] as const;
// Mixed glyphs so a font with different metrics than the baseline shows a
// measurable width delta.
const PROBE = "mmmmmmmmmmlliWQ0Ogq{}[]#@";

// Common developer + Nerd Font families. Nerd Font variants are listed under
// the names their installers register (e.g. "MesloLGS NF").
export const MONOSPACE_FONT_CANDIDATES = [
  "JetBrains Mono",
  "JetBrainsMono Nerd Font",
  "Fira Code",
  "FiraCode Nerd Font",
  "MesloLGS NF",
  "MesloLGL Nerd Font",
  "Hack",
  "Hack Nerd Font",
  "Cascadia Code",
  "CaskaydiaCove Nerd Font",
  "Source Code Pro",
  "SauceCodePro Nerd Font",
  "IBM Plex Mono",
  "Roboto Mono",
  "Ubuntu Mono",
  "DejaVu Sans Mono",
  "Menlo",
  "Monaco",
  "SF Mono",
  "Consolas",
  "Courier New",
  "Liberation Mono",
  "Inconsolata",
  "Anonymous Pro",
  "Victor Mono",
];

export function detectInstalledFonts(candidates: string[] = MONOSPACE_FONT_CANDIDATES): string[] {
  const ctx = document.createElement("canvas").getContext("2d");
  if (!ctx) return [];
  const size = 48;
  const base: Record<string, number> = {};
  for (const b of BASELINES) {
    ctx.font = `${size}px ${b}`;
    base[b] = ctx.measureText(PROBE).width;
  }
  return candidates.filter((name) =>
    BASELINES.some((b) => {
      ctx.font = `${size}px "${name}", ${b}`;
      return ctx.measureText(PROBE).width !== base[b];
    }),
  );
}

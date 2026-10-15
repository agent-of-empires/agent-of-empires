// Detect installed monospace fonts from a curated list by width probing, which needs no permission, unlike queryLocalFonts().

const BASELINES = ["monospace", "serif", "sans-serif"] as const;
// Mixed glyphs so differing metrics show a width delta.
const PROBE = "mmmmmmmmmmlliWQ0Ogq{}[]#@";

// Nerd Fonts are listed under their installed family names.
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

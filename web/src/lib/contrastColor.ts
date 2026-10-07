/** WCAG relative luminance of a `#rrggbb` hex color. */
function relativeLuminance(hex: string): number {
  const [r, g, b] = [1, 3, 5]
    .map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((c) => (c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4)));
  return 0.2126 * r! + 0.7152 * g! + 0.0722 * b!;
}

/** WCAG contrast ratio between two `#rrggbb` hex colors. */
function contrastRatio(a: string, b: string): number {
  const [lighter, darker] = [relativeLuminance(a), relativeLuminance(b)].sort((x, y) => y - x);
  return (lighter! + 0.05) / (darker! + 0.05);
}

// Pure black/white, not a near-black/near-white brand tone: picking whichever of these two has the higher
// contrast against an arbitrary background is a standard, provable WCAG bound. At the worst-case background
// luminance (~0.18, e.g. #757575), both candidates tie at sqrt(1.05/0.05) ~= 4.58:1, still above the 4.5:1 AA
// floor for normal text; any softer pair (e.g. off-white or near-black) narrows that margin and can drop below
// 4.5:1 for some background (verified: #777777 clears only 4.28-4.29:1 against #0f0f11/#fafafa).
const DARK_FOREGROUND = "#000000";
const LIGHT_FOREGROUND = "#ffffff";

/** Picks whichever of pure black/white clears WCAG contrast better against `backgroundHex`, so text stays
 *  legible against any resolved status color, including a user-supplied custom theme accent, rather than
 *  assuming every theme's accent is bright enough for one hardcoded foreground. */
export function pickContrastForeground(backgroundHex: string): string {
  const darkContrast = contrastRatio(backgroundHex, DARK_FOREGROUND);
  const lightContrast = contrastRatio(backgroundHex, LIGHT_FOREGROUND);
  return darkContrast >= lightContrast ? DARK_FOREGROUND : LIGHT_FOREGROUND;
}

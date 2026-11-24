// Detect installed fonts by width probing, which needs no permission, unlike queryLocalFonts(). It measures what the
// page can render, so a font the browser withholds from web content counts as absent.

const BASELINES = ["monospace", "serif", "sans-serif"] as const;
// Mixed glyphs so differing metrics show a width delta.
const PROBE = "mmmmmmmmmmlliWQ0Ogq{}[]#@";

export function detectInstalledFonts(candidates: readonly string[]): string[] {
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

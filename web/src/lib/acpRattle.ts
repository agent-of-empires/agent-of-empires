// Structured view working indicator: braille spinner frames plus empire-themed verbs that rotate during long turns.

export const SPINNER_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] as const;

export const SPINNER_INTERVAL_MS = 80;

export const VERB_INTERVAL_MS = 18_000;

/** Bash tool titles carry the whole command line, so long titles are clamped. */
export const TOOL_LABEL_MAX = 24;

export const WORKING_VERBS: readonly string[] = [
  "Conscripting villagers",
  "Marshalling forces",
  "Forging banners",
  "Mining gold",
  "Felling cedars",
  "Quarrying granite",
  "Hunting deer",
  "Founding outposts",
  "Recruiting heroes",
  "Drilling troops",
  "Sharpening swords",
  "Smelting iron",
  "Charting waters",
  "Provisioning ships",
  "Inscribing scrolls",
  "Tilling fields",
  "Stoking forges",
  "Hoisting banners",
  "Mustering armies",
  "Plotting strategy",
  "Brewing schemes",
  "Levying tribute",
  "Storming gates",
  "Scouting frontiers",
  "Trading at the wharf",
  "Erecting wonders",
  "Convening the council",
  "Plundering archives",
  "Decoding glyphs",
  "Annexing territory",
  "Calibrating trebuchets",
  "Negotiating treaties",
  "Surveying ruins",
  "Anointing scribes",
  "Hatching gambits",
] as const;

export const THINKING_VERBS: readonly string[] = [
  "Consulting auguries",
  "Reading entrails",
  "Pondering the map",
  "Whispering with elders",
  "Decoding prophecies",
  "Casting bones",
  "Conferring with sages",
  "Studying the stars",
  "Plotting on the war table",
  "Brewing wisdom",
  "Divining strategy",
  "Reciting from scrolls",
  "Communing with chronicles",
  "Polishing arguments",
] as const;

/** A running tool beats thinking: claude-agent-acp can leave `thinking` latched through a tool run. */
export function deriveSpinnerState(thinking: boolean, tool: string | null): "thinking" | "tool" | "working" {
  return tool ? "tool" : thinking ? "thinking" : "working";
}

/** Stable per seed, so the verb only changes when the caller bumps the seed. */
export function pickIndex(len: number, seed: number): number {
  let h = seed | 0;
  h = (h ^ (h << 13)) | 0;
  h = (h ^ (h >>> 17)) | 0;
  h = (h ^ (h << 5)) | 0;
  return Math.abs(h) % Math.max(1, len);
}

/** `seed` keeps the verb stable across re-renders until the caller bumps it. */
export function chooseVerb(state: "thinking" | "tool" | "working", seed: number, toolName?: string | null): string {
  if (state === "tool" && toolName) {
    const verbs = ["Dispatching", "Commanding", "Marshalling", "Operating", "Wielding"];
    const v = verbs[pickIndex(verbs.length, seed)];
    const label = toolName.length > TOOL_LABEL_MAX ? toolName.slice(0, TOOL_LABEL_MAX).trimEnd() : toolName;
    return `${v} ${label}…`;
  }
  if (state === "thinking") {
    return `${THINKING_VERBS[pickIndex(THINKING_VERBS.length, seed)]}…`;
  }
  return `${WORKING_VERBS[pickIndex(WORKING_VERBS.length, seed)]}…`;
}

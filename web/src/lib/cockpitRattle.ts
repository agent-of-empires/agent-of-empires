// Fun status messages for the cockpit working indicator. Themed around
// Agent of Empires' civilization-building flavor (see
// src/session/civilizations.rs and the random-title generator). The
// spinner glyph rattles through braille frames at terminal speed; the
// verb cycles every few seconds so long turns stay alive.
//
// Inspired by Claude Code's "ruminating" / "noodling" / "spelunking"
// verbs and the Rust `rattles` crate the TUI side uses for ratatui
// spinners (Cargo.toml: rattles = "0.2"; src/tui/home/render.rs:8).

/** Braille spinner frames; classic 10-step rotation. ~80ms per frame. */
export const SPINNER_FRAMES = [
  "⠋",
  "⠙",
  "⠹",
  "⠸",
  "⠼",
  "⠴",
  "⠦",
  "⠧",
  "⠇",
  "⠏",
] as const;

/** Frame interval in ms for the spinner glyph. */
export const SPINNER_INTERVAL_MS = 80;

/** Verb cycle interval — how often the working label rotates. */
export const VERB_INTERVAL_MS = 18_000;

/**
 * General "agent is working" pool. Used when there's no thinking/tool
 * sub-state to be more specific. Themed around empire building and
 * statecraft — conscripting, mining, forging, scouting, etc.
 */
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

/**
 * Thinking/reasoning pool. Drawn from when the agent emits
 * AgentThoughtChunk. More mystical/divinatory flavor since the agent
 * is "considering" rather than "doing".
 */
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

/**
 * Pick the spinner sub-state from the two flags the cockpit tracks.
 * `tool` is the more specific, I/O-bound signal: a tool in flight means
 * the agent is blocked waiting on its result, which the user needs to
 * see over the compute-bound "thinking" state. Prefer it when both are
 * set (the claude-agent-acp adapter can leave `thinking` latched true
 * through a tool run by skipping ThinkingEnded). See #1213.
 */
export function deriveSpinnerState(
  thinking: boolean,
  tool: string | null,
): "thinking" | "tool" | "working" {
  return tool ? "tool" : thinking ? "thinking" : "working";
}

/**
 * Pick a stable random index for a list. The same seed within one
 * turn keeps the verb stable; we generate a fresh seed each turn.
 */
export function pickIndex(len: number, seed: number): number {
  // Mulberry32-ish hash; deterministic, decent spread for tiny ranges.
  let h = seed | 0;
  h = (h ^ (h << 13)) | 0;
  h = (h ^ (h >>> 17)) | 0;
  h = (h ^ (h << 5)) | 0;
  return Math.abs(h) % Math.max(1, len);
}

/**
 * Choose a verb for the current state. `seed` lets callers keep the
 * verb stable across re-renders within a tick, then bump it to rotate.
 */
export function chooseVerb(
  state: "thinking" | "tool" | "working",
  seed: number,
  toolName?: string | null,
): string {
  if (state === "tool" && toolName) {
    // Keep the actual tool name but dress it up with an empire verb so
    // tool runs feel of-a-piece with the rest of the spinner.
    const verbs = [
      "Dispatching",
      "Commanding",
      "Marshalling",
      "Operating",
      "Wielding",
    ];
    const v = verbs[pickIndex(verbs.length, seed)];
    return `${v} ${toolName}…`;
  }
  if (state === "thinking") {
    return `${THINKING_VERBS[pickIndex(THINKING_VERBS.length, seed)]}…`;
  }
  return `${WORKING_VERBS[pickIndex(WORKING_VERBS.length, seed)]}…`;
}

// Age of Empires cheat-code easter eggs for the command palette. Type a full
// code into the cmd+k search and a themed toast plus a one-off visual flourish
// fires. A wink at the "Agent of Empires" name; not documented on purpose.

export type CheatEffectKind = "fly" | "confetti" | "flash" | "pulse";

export interface CheatEffect {
  kind: CheatEffectKind;
  // Sprite for fly / confetti / pulse. Unused by flash.
  emoji?: string;
  // Tint for flash. Unused by the others.
  color?: string;
  // Travel direction for fly.
  dir?: "ltr" | "rtl";
}

export interface Cheat {
  toast: string;
  effect: CheatEffect;
}

// Keys are already normalized (see normalize): lowercase, single-spaced, trimmed.
export const CHEATS: Record<string, Cheat> = {
  // Age of Empires 1
  wololo: {
    toast: "Wololo. The agent in the next worktree converts to your cause.",
    effect: { kind: "flash", color: "var(--color-terminal-active)" },
  },
  "photon man": {
    toast: "A Photon Man vaporizes your merge conflicts.",
    effect: { kind: "fly", emoji: "⚡", dir: "ltr" },
  },
  pow: {
    toast: "A baby on a tricycle with a gun. Don't ask.",
    effect: { kind: "fly", emoji: "👶", dir: "rtl" },
  },

  // Age of Empires 2
  "how do you turn this on": {
    toast: "🚗 A Cobra Car spawns in your sandbox and floors it.",
    effect: { kind: "fly", emoji: "🚗", dir: "ltr" },
  },
  "tuck tuck tuck": {
    toast: "A monster truck flattens your flaky tests.",
    effect: { kind: "fly", emoji: "🚚", dir: "rtl" },
  },
  marco: {
    toast: "Marco! Every session revealed.",
    effect: { kind: "pulse", emoji: "🗺️" },
  },
  polo: {
    toast: "Polo! Fog lifted. You see what every agent thinks. (No you don't.)",
    effect: { kind: "pulse", emoji: "🌫️" },
  },
  aegis: {
    toast: "AEGIS on. Worktrees build instantly. (cargo still takes 4 min.)",
    effect: { kind: "flash", color: "var(--color-status-waiting)" },
  },
  "rock on": {
    toast: "+1000 stone. Spent it on rate limits.",
    effect: { kind: "confetti", emoji: "🪨" },
  },
  lumberjack: {
    toast: "+1000 wood. The daemon is cozy.",
    effect: { kind: "confetti", emoji: "🪵" },
  },
  "robin hood": {
    toast: "+1000 tokens. Use them wisely.",
    effect: { kind: "confetti", emoji: "🪙" },
  },
  "cheese steak jimmy's": {
    toast: "+1000 food. Agents refuse to stop coding.",
    effect: { kind: "confetti", emoji: "🍔" },
  },
  "i love the monkey head": {
    toast: "🐵 VDML. A furious monkey reviews your PR.",
    effect: { kind: "fly", emoji: "🐵", dir: "ltr" },
  },
  "black death": {
    toast: "Black Death. All flaky tests eliminated. (They'll be back.)",
    effect: { kind: "flash", color: "var(--color-surface-800)" },
  },

  // Age of Empires 3
  "ya gotta make do with what ya got": {
    toast: "The Tommynator rolls in and crushes your tech debt.",
    effect: { kind: "fly", emoji: "🚛", dir: "rtl" },
  },
  "nova & orion": {
    toast: "+10000 XP. You leveled up to Staff Engineer.",
    effect: { kind: "confetti", emoji: "⭐" },
  },
  "medium rare please": {
    toast: "+10000 food. The grill never stops.",
    effect: { kind: "confetti", emoji: "🍖" },
  },
  "give me liberty or give me coin": {
    toast: "+10000 coin. Still can't afford Opus.",
    effect: { kind: "confetti", emoji: "🪙" },
  },
  "this is too hard": {
    toast: "You win. PR merged. (It wasn't.)",
    effect: { kind: "confetti", emoji: "🎉" },
  },
  "speed always wins": {
    toast: "Research speed x100. CI still queued.",
    effect: { kind: "flash", color: "var(--color-status-running)" },
  },
  "x marks the spot": {
    toast: "Map revealed. The bug was in your code all along.",
    effect: { kind: "pulse", emoji: "❌" },
  },
};

function normalize(input: string): string {
  return input.toLowerCase().trim().replace(/\s+/g, " ");
}

// Returns the cheat for a full-string match, or null. Substrings never match,
// so normal palette searches ("settings", "new") pass straight through.
export function matchCheat(input: string): Cheat | null {
  return CHEATS[normalize(input)] ?? null;
}

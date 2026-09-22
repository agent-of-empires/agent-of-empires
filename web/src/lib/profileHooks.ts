import type { HooksOverride } from "./types";

export type HookEventKey = "on_create" | "on_launch" | "on_destroy";

/** `override` (non-empty list), `override-empty` (disables global hooks), `inherited`, or `none`. */
export type HookSource = "override" | "override-empty" | "inherited" | "none";

export interface EffectiveHookGroup {
  key: HookEventKey;
  /** TUI-parity label (src/tui/settings/fields.rs). */
  label: string;
  source: HookSource;
  commands: string[];
}

/** TUI display order. */
const HOOK_EVENTS: ReadonlyArray<readonly [HookEventKey, string]> = [
  ["on_create", "On Create"],
  ["on_launch", "On Launch"],
  ["on_destroy", "On Destroy"],
];

/** `undefined` means inherit; non-arrays count as absent so malformed payloads degrade to inherited. */
function hookArray(src: HooksOverride | undefined, key: HookEventKey): string[] | undefined {
  const value = src?.[key];
  return Array.isArray(value) ? value : undefined;
}

/** Tri-state like Rust `Option<Vec<String>>`: absent inherits, `[]` disables, a list replaces. */
export function buildEffectiveHooks(
  profile: HooksOverride | undefined,
  global: HooksOverride | undefined,
): EffectiveHookGroup[] {
  return HOOK_EVENTS.map(([key, label]) => {
    const override = hookArray(profile, key);
    if (override !== undefined) {
      return {
        key,
        label,
        source: override.length === 0 ? "override-empty" : "override",
        commands: override,
      };
    }
    const inherited = hookArray(global, key) ?? [];
    return {
      key,
      label,
      source: inherited.length === 0 ? "none" : "inherited",
      commands: inherited,
    };
  });
}

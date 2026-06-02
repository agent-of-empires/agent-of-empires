import type { HooksOverride } from "./types";

export type HookEventKey = "on_create" | "on_launch" | "on_destroy";

/** How the effective hooks for one lifecycle event are resolved for a
 *  profile:
 *  - `override`: the profile sets a non-empty command list (runs these).
 *  - `override-empty`: the profile sets an explicit empty list, disabling
 *    the inherited global hooks for this event.
 *  - `inherited`: the profile has no override, so the global hooks run.
 *  - `none`: neither the profile nor the global config defines hooks. */
export type HookSource = "override" | "override-empty" | "inherited" | "none";

export interface EffectiveHookGroup {
  key: HookEventKey;
  /** TUI-parity label (see src/tui/settings/fields.rs build_hooks_fields). */
  label: string;
  source: HookSource;
  /** Commands that will run for this event under the profile. */
  commands: string[];
}

/** Lifecycle events in TUI display order with their parity labels. */
const HOOK_EVENTS: ReadonlyArray<readonly [HookEventKey, string]> = [
  ["on_create", "On Create"],
  ["on_launch", "On Launch"],
  ["on_destroy", "On Destroy"],
];

/** Return the array for one event if the source explicitly set it (the
 *  tri-state `Some`), else `undefined` (the tri-state `None` = inherit).
 *  Non-array values are treated as absent so a malformed payload degrades
 *  to "inherited" rather than throwing. */
function hookArray(
  src: HooksOverride | undefined,
  key: HookEventKey,
): string[] | undefined {
  const value = src?.[key];
  return Array.isArray(value) ? value : undefined;
}

/** Merge a profile's hook overrides with the global hooks into the
 *  effective per-event view rendered by HooksReadOnlyPanel. The profile
 *  override shape is tri-state (mirrors Rust `Option<Vec<String>>`):
 *  absent inherits global, an explicit `[]` disables it, a non-empty array
 *  replaces it. */
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

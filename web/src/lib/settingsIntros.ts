// Folded Settings tab intros, kept in synced web UI state so they follow the user to every Device.

import { safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";

/** The Settings tabs that open with an intro. */
export const SETTINGS_INTRO_IDS = ["worktree", "plugins", "structured-view"] as const;
export type SettingsIntroId = (typeof SETTINGS_INTRO_IDS)[number];

export function isSettingsIntroId(tab: string): tab is SettingsIntroId {
  return (SETTINGS_INTRO_IDS as readonly string[]).includes(tab);
}

/** Synced by prefix in `webUiSync`. */
export const SETTINGS_INTRO_KEY_PREFIX = "aoe-settings-intro-";
const FOLDED = "folded";
// Replay writes a value rather than removing the key: hydration only copies the keys the
// server has, so a removal would leave other Devices folded.
const UNFOLDED = "unfolded";

const introKey = (id: SettingsIntroId) => `${SETTINGS_INTRO_KEY_PREFIX}${id}`;
// A read-only dashboard can't write synced state, and hydration would put the Daemon's value
// back over its choice. It keeps its folds and replays in this unsynced key, which hydration
// never touches and which overrides the synced one.
const deviceKey = (id: SettingsIntroId) => `aoe-local-settings-intro-${id}`;

export function isSettingsIntroFolded(id: SettingsIntroId): boolean {
  return (safeGetItem(deviceKey(id)) ?? safeGetItem(introKey(id))) === FOLDED;
}

function setIntro(id: SettingsIntroId, value: string, readOnly: boolean): void {
  if (readOnly) {
    safeSetItem(deviceKey(id), value);
    return;
  }
  safeSetItem(introKey(id), value);
  // A choice made while read-only must not outlive one the Daemon now stores.
  safeRemoveItem(deviceKey(id));
}

export function foldSettingsIntro(id: SettingsIntroId, readOnly: boolean): void {
  setIntro(id, FOLDED, readOnly);
}

const INTROS_RESET_EVENT = "aoe:settings-intros-reset";

/** "Show tutorial" unfolds every intro once, here and on other Devices. */
export function resetSettingsIntros(readOnly: boolean): void {
  for (const id of SETTINGS_INTRO_IDS) setIntro(id, UNFOLDED, readOnly);
  // An open Settings view holds its folds in state, so it hears about the reset.
  window.dispatchEvent(new Event(INTROS_RESET_EVENT));
}

export function onSettingsIntrosReset(listener: () => void): () => void {
  window.addEventListener(INTROS_RESET_EVENT, listener);
  return () => window.removeEventListener(INTROS_RESET_EVENT, listener);
}

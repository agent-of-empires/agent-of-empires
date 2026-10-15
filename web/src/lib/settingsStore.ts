import { useSyncExternalStore } from "react";
import { fetchSettings, type SaveOutcome } from "./api";

type Settings = Record<string, unknown>;

export interface SettingsView {
  settings: Settings | null;
  saving: boolean;
  error: string | null;
}

/**
 * Per-profile settings kept outside any component, so an edit built after a remount sees the
 * edits still waiting on their PATCH, and saves of one profile reach the server in edit order.
 */
interface Entry {
  view: SettingsView;
  /** Bumped by every edit and every settled save; a GET started before a bump is stale. */
  gen: number;
  pending: number;
  tail: Promise<unknown>;
  /** A save may have landed without the client knowing; reconcile once the queue drains. */
  needsReconcile: boolean;
  loadSeq: number;
}

const entries = new Map<string, Entry>();
const listeners = new Set<() => void>();

function entry(profile: string): Entry {
  let e = entries.get(profile);
  if (!e) {
    e = {
      view: { settings: null, saving: false, error: null },
      gen: 0,
      pending: 0,
      tail: Promise.resolve(),
      needsReconcile: false,
      loadSeq: 0,
    };
    entries.set(profile, e);
  }
  return e;
}

function publish(e: Entry, patch: Partial<SettingsView>) {
  e.view = { ...e.view, ...patch, saving: e.pending > 0 };
  for (const l of listeners) l();
}

export function useProfileSettings(profile: string): SettingsView {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => entry(profile).view,
  );
}

/** Fetch the profile's settings; the response is dropped if the profile saw an edit, a settled save or a newer fetch since. */
export function loadSettings(profile: string): void {
  if (!profile) return;
  const e = entry(profile);
  const { gen } = e;
  const seq = ++e.loadSeq;
  const current = () => seq === e.loadSeq && gen === e.gen && e.pending === 0;
  fetchSettings(profile)
    .then((s) => {
      if (!current() || !s) return;
      e.needsReconcile = false;
      publish(e, { settings: s });
    })
    .catch(() => {
      if (current()) publish(e, { settings: null });
    });
}

/**
 * Show `value` at once, then run `send` after the saves already queued for this profile.
 * Resolves to false only when the server rejected the write; an uncertain outcome keeps the
 * edit on screen and is settled by a fetch once the queue drains.
 */
export async function saveField(
  profile: string,
  section: string,
  field: string,
  value: unknown,
  send: () => Promise<SaveOutcome>,
): Promise<boolean> {
  const e = entry(profile);
  const base = e.view.settings;
  e.gen++;
  e.pending++;
  publish(e, {
    error: null,
    settings: base && { ...base, [section]: { ...(base[section] as Settings | undefined), [field]: value } },
  });
  const request = e.tail.then(send);
  e.tail = request.catch(() => undefined);
  const outcome = await request.catch((): SaveOutcome => "uncertain");
  e.gen++;
  e.pending--;
  if (outcome !== "committed") e.needsReconcile = true;
  const error =
    outcome === "rejected"
      ? "Failed to save, please try again"
      : outcome === "uncertain"
        ? "Could not confirm the save, please check the value"
        : e.view.error;
  publish(e, { error });
  if (e.pending === 0 && e.needsReconcile) loadSettings(profile);
  return outcome !== "rejected";
}

export function resetSettingsStore(): void {
  entries.clear();
}

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
  /**
   * Edits whose outcome is unknown. A late commit can still follow any fetch, so a fetched
   * value that differs from the intent never replaces it; only a matching one settles it.
   */
  unconfirmed: Map<string, { section: string; field: string; value: unknown }>;
  fieldGen: Map<string, number>;
  loading: Promise<void>;
  afterDrain: (() => void)[];
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
      unconfirmed: new Map(),
      fieldGen: new Map(),
      loading: Promise.resolve(),
      afterDrain: [],
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

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

/** Drop unconfirmed edits the server now shows; keep the rest on screen over the fetched value. */
function settle(e: Entry, fetched: Settings): Settings {
  let out = fetched;
  for (const [key, { section, field, value }] of e.unconfirmed) {
    if (same((fetched[section] as Settings | undefined)?.[field], value)) {
      e.unconfirmed.delete(key);
    } else {
      out = { ...out, [section]: { ...(out[section] as Settings | undefined), [field]: value } };
    }
  }
  e.needsReconcile = e.unconfirmed.size > 0;
  return out;
}

/**
 * Fetch the profile's settings. A response is dropped if a newer fetch started, and one that an
 * edit or settled save overtook is retried once the queue is idle, so the cache never stays on a
 * snapshot older than what it shows. Resolves once a fetch started after the call has landed.
 */
export function loadSettings(profile: string): Promise<void> {
  if (!profile) return Promise.resolve();
  const e = entry(profile);
  const { gen } = e;
  const seq = ++e.loadSeq;
  const request = fetchSettings(profile)
    .then((s): void | Promise<void> => {
      if (seq !== e.loadSeq) return e.loading;
      if (gen !== e.gen || e.pending > 0) {
        e.needsReconcile = true;
        if (e.pending === 0) return loadSettings(profile);
        return new Promise<void>((resolve) => e.afterDrain.push(resolve)).then(() => e.loading);
      }
      if (s) publish(e, { settings: settle(e, s) });
    })
    .catch(() => {
      if (seq === e.loadSeq && gen === e.gen && e.pending === 0) {
        publish(e, { error: "Could not refresh settings, please reload" });
      }
    });
  e.loading = request;
  return request;
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
  const key = `${section}\0${field}`;
  const fieldGen = (e.fieldGen.get(key) ?? 0) + 1;
  e.fieldGen.set(key, fieldGen);
  e.unconfirmed.delete(key);
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
  // A newer edit of the same field supersedes this one, so only the latest keeps its intent.
  if (outcome === "uncertain" && e.fieldGen.get(key) === fieldGen) {
    e.unconfirmed.set(key, { section, field, value });
  }
  const error =
    outcome === "rejected"
      ? "Failed to save, please try again"
      : outcome === "uncertain"
        ? "Could not confirm the save, please check the value"
        : e.view.error;
  publish(e, { error });
  if (e.pending === 0) {
    if (e.needsReconcile) void loadSettings(profile);
    for (const resolve of e.afterDrain.splice(0)) resolve();
  }
  return outcome !== "rejected";
}

export function resetSettingsStore(): void {
  entries.clear();
}

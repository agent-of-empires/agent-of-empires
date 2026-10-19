import { useSyncExternalStore } from "react";
import { fetchMachineSettings, fetchSettings, type SaveOutcome } from "./api";

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
interface Unconfirmed {
  section: string;
  field: string;
  value: unknown;
  send: () => Promise<SaveOutcome>;
  attempts: number;
}

/** Resends of an unconfirmed edit before it is left unresolved, still on screen, for the user to settle. */
const MAX_ATTEMPTS = 3;
const CONFIRM_ERROR = "Could not confirm";
const UNCERTAIN_ERROR = `${CONFIRM_ERROR} the save, please check the value`;
const PLUGIN_PREFIX = "plugin:";

/** Plugin settings live under `plugins.<id>.settings` rather than a section of their own. */
function readField(settings: Settings, section: string, field: string): unknown {
  if (!section.startsWith(PLUGIN_PREFIX)) return (settings[section] as Settings | undefined)?.[field];
  const plugins = settings.plugins as Record<string, { settings?: Settings }> | undefined;
  return plugins?.[section.slice(PLUGIN_PREFIX.length)]?.settings?.[field];
}

function writeField(settings: Settings, section: string, field: string, value: unknown): Settings {
  if (!section.startsWith(PLUGIN_PREFIX)) {
    return { ...settings, [section]: { ...(settings[section] as Settings | undefined), [field]: value } };
  }
  const id = section.slice(PLUGIN_PREFIX.length);
  const plugins = (settings.plugins ?? {}) as Record<string, { settings?: Settings }>;
  const plugin = plugins[id] ?? {};
  return {
    ...settings,
    plugins: { ...plugins, [id]: { ...plugin, settings: { ...plugin.settings, [field]: value } } },
  };
}

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
  unconfirmed: Map<string, Unconfirmed>;
  fieldGen: Map<string, number>;
  loading: Promise<boolean>;
  afterDrain: (() => void)[];
  loadSeq: number;
}

const entries = new Map<string | undefined, Entry>();
const listeners = new Set<() => void>();

function entry(profile: string | undefined): Entry {
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
      loading: Promise.resolve(false),
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

export function useProfileSettings(profile: string | undefined): SettingsView {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => entry(profile).view,
  );
}

const REFRESH_ERROR = "Could not refresh settings, please reload";

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

/**
 * Settle unconfirmed edits against a fetch: the ones it shows are done, the rest stay on screen
 * and are resent. One that kept failing stays on screen too, unresolved: no fetch proves a late
 * commit is not still coming, so the saved value never replaces it behind the user's back.
 */
function settle(e: Entry, fetched: Settings): { settings: Settings; resend: Unconfirmed[]; unresolved: string[] } {
  let settings = fetched;
  const resend: Unconfirmed[] = [];
  const unresolved: string[] = [];
  for (const [key, u] of e.unconfirmed) {
    if (same(readField(fetched, u.section, u.field), u.value)) {
      e.unconfirmed.delete(key);
      continue;
    }
    settings = writeField(settings, u.section, u.field, u.value);
    if (u.attempts >= MAX_ATTEMPTS) unresolved.push(`${u.section}.${u.field}`);
    else resend.push(u);
  }
  e.needsReconcile = e.unconfirmed.size > 0;
  return { settings, resend, unresolved };
}

/**
 * Fetch the profile's settings, or machine-wide settings for `undefined`. A response is dropped
 * if a newer fetch started, and one that an edit or settled save overtook is retried once the
 * queue is idle. Resolves once a fetch started after the call has landed, to whether a response
 * was applied.
 */
export function loadSettings(profile: string | undefined): Promise<boolean> {
  if (profile === "") return Promise.resolve(false);
  const e = entry(profile);
  const { gen } = e;
  const seq = ++e.loadSeq;
  const request = (profile === undefined ? fetchMachineSettings() : fetchSettings(profile))
    .then((s): boolean | Promise<boolean> => {
      if (seq !== e.loadSeq) return e.loading;
      if (gen !== e.gen || e.pending > 0) {
        e.needsReconcile = true;
        if (e.pending === 0) return loadSettings(profile);
        return new Promise<void>((resolve) => e.afterDrain.push(resolve)).then(() => e.loading);
      }
      if (!s) {
        publish(e, { error: REFRESH_ERROR });
        return false;
      }
      const { settings, resend, unresolved } = settle(e, s);
      const error =
        unresolved.length > 0
          ? `${CONFIRM_ERROR} ${unresolved.join(", ")}; showing your value. Reload to discard it`
          : e.unconfirmed.size === 0 && e.view.error?.startsWith(CONFIRM_ERROR)
            ? null
            : e.view.error;
      publish(e, { settings, error });
      for (const u of resend) void saveField(profile, u.section, u.field, u.value, u.send, u.attempts);
      return true;
    })
    .catch(() => {
      if (seq === e.loadSeq && gen === e.gen && e.pending === 0) publish(e, { error: REFRESH_ERROR });
      return false;
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
  profile: string | undefined,
  section: string,
  field: string,
  value: unknown,
  send: () => Promise<SaveOutcome>,
  attempts = 0,
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
    // Another edit does not answer a "could not confirm" still waiting on the server.
    error: e.unconfirmed.size > 0 ? e.view.error : null,
    settings: writeField(base ?? {}, section, field, value),
  });
  const request = e.tail.then(send);
  e.tail = request.catch(() => undefined);
  const outcome = await request.catch((): SaveOutcome => "uncertain");
  e.gen++;
  e.pending--;
  if (outcome !== "committed") e.needsReconcile = true;
  // A newer edit of the same field supersedes this one, so only the latest keeps its intent.
  if (outcome === "uncertain" && e.fieldGen.get(key) === fieldGen) {
    e.unconfirmed.set(key, { section, field, value, send, attempts: attempts + 1 });
  }
  const error =
    outcome === "rejected"
      ? "Failed to save, please try again"
      : outcome === "uncertain"
        ? UNCERTAIN_ERROR
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

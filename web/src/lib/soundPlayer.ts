import { fetchSettings, fetchSound } from "./api";
import { onSettingsChanged } from "./settingsEvents";

export type SoundEvent = "approval" | "waiting" | "idle" | "error";

interface SoundSettings {
  enabled?: boolean;
  browser_enabled?: boolean;
  browser_volume?: number;
  browser_scope?: "all" | "other_sessions" | "hidden_tab";
  browser_approval_enabled?: boolean;
  browser_waiting_enabled?: boolean;
  browser_idle_enabled?: boolean;
  browser_error_enabled?: boolean;
}

const SETTINGS_KEYS = [
  "enabled",
  "browser_enabled",
  "browser_volume",
  "browser_scope",
  "browser_approval_enabled",
  "browser_waiting_enabled",
  "browser_idle_enabled",
  "browser_error_enabled",
] as const;

let cachedSettings: SoundSettings | null = null;
let cachedSettingsAt = 0;
const SETTINGS_TTL_MS = 30_000;

const buffers = new Map<SoundEvent, Promise<AudioBuffer | null>>();

// Bumped on every clear, so a read started before a save cannot re-cache the old value.
let cacheGeneration = 0;

export function clearSoundCache(): void {
  cacheGeneration++;
  cachedSettings = null;
  cachedSettingsAt = 0;
  buffers.clear();
}

// A saved setting applies to the next sound, not after the cache expires.
onSettingsChanged(clearSoundCache);

async function loadSettings(): Promise<SoundSettings | null> {
  const now = Date.now();
  if (cachedSettings && now - cachedSettingsAt < SETTINGS_TTL_MS) {
    return cachedSettings;
  }
  const generation = cacheGeneration;
  const data = await fetchSettings();
  const raw = data?.sound as Record<string, unknown> | undefined;
  let sound: SoundSettings | null = null;
  if (raw) {
    const picked: Record<string, unknown> = {};
    for (const key of SETTINGS_KEYS) picked[key] = raw[key];
    sound = picked as SoundSettings;
  }
  if (generation === cacheGeneration) {
    cachedSettings = sound;
    cachedSettingsAt = now;
  }
  return sound;
}

// One context for the page: browsers start it suspended until a user gesture, and once resumed it stays unlocked,
// where each new <audio> element would need its own gesture on iOS. Resuming inside the first gesture of the
// page means a sound that comes later can play.
let context: AudioContext | null = null;
const UNLOCK_EVENTS = ["pointerdown", "keydown", "touchend"] as const;

function audioContext(): AudioContext | null {
  if (!context && globalThis.AudioContext) context = new globalThis.AudioContext();
  return context;
}

function unlock(): void {
  const ctx = audioContext();
  if (!ctx) return;
  void ctx.resume().then(
    () => {
      if (ctx.state !== "running") return;
      for (const type of UNLOCK_EVENTS) window.removeEventListener(type, unlock, true);
    },
    () => {},
  );
}

if (typeof window !== "undefined") {
  for (const type of UNLOCK_EVENTS) window.addEventListener(type, unlock, true);
}

// A resume outside a gesture settles at once when the page was unlocked earlier, and never when it was not.
const RESUME_WAIT_MS = 200;

async function running(ctx: AudioContext): Promise<boolean> {
  if (ctx.state === "running") return true;
  await Promise.race([ctx.resume().catch(() => {}), new Promise((r) => setTimeout(r, RESUME_WAIT_MS))]);
  // `resume` changes the state, which narrowing from the check above does not see.
  return (ctx.state as AudioContextState) === "running";
}

function loadBuffer(ctx: AudioContext, event: SoundEvent): Promise<AudioBuffer | null> {
  const cached = buffers.get(event);
  if (cached) return cached;
  const load = (async () => {
    const bytes = await fetchSound(event);
    if (!bytes) return null;
    try {
      return await ctx.decodeAudioData(bytes);
    } catch {
      return null;
    }
  })();
  buffers.set(event, load);
  // A failed load is retried by the next sound.
  void load.then((buffer) => {
    if (!buffer && buffers.get(event) === load) buffers.delete(event);
  });
  return load;
}

const blockedListeners = new Set<() => void>();
let blockedReported = false;

/** Calls `listener` the first time this page cannot play a sound until the user interacts with it. */
export function onSoundBlocked(listener: () => void): () => void {
  blockedListeners.add(listener);
  return () => void blockedListeners.delete(listener);
}

function reportBlocked(): void {
  if (blockedReported) return;
  blockedReported = true;
  for (const listener of blockedListeners) listener();
}

export interface SoundContext {
  /** Sessions the sound is for; it plays while any of them is in scope. */
  sessionIds?: readonly string[];
  /** Reads the session open in this tab now, since it can change while a sound loads. */
  activeSessionId?: () => string | null;
}

function inScope(sound: SoundSettings, { sessionIds, activeSessionId }: SoundContext): boolean {
  switch (sound.browser_scope) {
    case "other_sessions": {
      const active = activeSessionId?.();
      return !sessionIds?.length || sessionIds.some((id) => id !== active);
    }
    case "hidden_tab":
      return document.hidden;
    default:
      return true;
  }
}

/** Resolves true when the sound started playing. A settings save or cache clear mid-way drops it. */
export async function playSoundFor(event: SoundEvent, context: SoundContext = {}): Promise<boolean> {
  const generation = cacheGeneration;
  const sound = await loadSettings();
  if (generation !== cacheGeneration) return false;
  if (!sound || !sound.enabled || sound.browser_enabled === false) return false;
  if (sound[`browser_${event}_enabled` as const] === false || !inScope(sound, context)) return false;
  const ctx = audioContext();
  if (!ctx) return false;
  const buffer = await loadBuffer(ctx, event);
  // The tab may have come back to the foreground while the sound loaded.
  if (!buffer || generation !== cacheGeneration || !inScope(sound, context)) return false;
  if (!(await running(ctx))) {
    reportBlocked();
    return false;
  }
  const gain = ctx.createGain();
  gain.gain.value = typeof sound.browser_volume === "number" ? Math.max(0, sound.browser_volume) : 1.0;
  gain.connect(ctx.destination);
  const source = ctx.createBufferSource();
  source.buffer = buffer;
  source.connect(gain);
  source.start();
  return true;
}

// The status sound for a Waiting session would repeat the approval chime that just played.
const APPROVAL_DEDUPE_MS = 15_000;
const lastApprovalChimeAt = new Map<string, number>();

export function noteApprovalChime(sessionId: string, now: number = Date.now()): void {
  lastApprovalChimeAt.set(sessionId, now);
}

export function approvalChimedRecently(sessionId: string, now: number = Date.now()): boolean {
  const at = lastApprovalChimeAt.get(sessionId);
  return at !== undefined && now - at < APPROVAL_DEDUPE_MS;
}

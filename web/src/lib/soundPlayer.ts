import { fetchSettings, fetchSounds, fetchSoundBlob } from "./api";
import { onSettingsChanged } from "./settingsEvents";

export type SoundEvent = "approval" | "waiting" | "idle" | "error";

interface SoundSettings {
  enabled?: boolean;
  browser_enabled?: boolean;
  volume?: number;
  on_approval?: string | null;
  on_waiting?: string | null;
  on_idle?: string | null;
  on_error?: string | null;
}

const SETTINGS_KEYS = [
  "enabled",
  "browser_enabled",
  "volume",
  "on_approval",
  "on_waiting",
  "on_idle",
  "on_error",
] as const;

let cachedSettings: SoundSettings | null = null;
let cachedSettingsAt = 0;
const SETTINGS_TTL_MS = 30_000;

const soundUrls = new Map<string, string>();
const pendingSoundUrls = new Map<string, Promise<string | null>>();
let availableSounds: string[] | null = null;

// Bumped on every clear, so a read started before a save cannot re-cache the old value.
let cacheGeneration = 0;

export function clearSoundCache(): void {
  cacheGeneration++;
  cachedSettings = null;
  cachedSettingsAt = 0;
  availableSounds = null;
  for (const url of soundUrls.values()) URL.revokeObjectURL(url);
  soundUrls.clear();
  pendingSoundUrls.clear();
}

// A saved sound choice applies to the next sound, not after the cache expires.
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

async function resolveSoundName(sound: SoundSettings, event: SoundEvent): Promise<string | null> {
  const override = sound[`on_${event}` as const]?.trim();
  if (override) return override;
  // Approvals only chime when a sound is chosen. Status sounds pick at random, like the host does.
  if (event === "approval") return null;
  availableSounds ??= await fetchSounds();
  if (availableSounds.length === 0) return null;
  return availableSounds[Math.floor(Math.random() * availableSounds.length)] ?? null;
}

function ensureSoundUrl(name: string): Promise<string | null> {
  const cached = soundUrls.get(name);
  if (cached) return Promise.resolve(cached);
  const pending = pendingSoundUrls.get(name);
  if (pending) return pending;
  const generation = cacheGeneration;
  const load = (async () => {
    // `<audio src>` bypasses the fetch auth interceptor, so load the bytes via fetch.
    const blob = await fetchSoundBlob(name);
    if (!blob || generation !== cacheGeneration) return null;
    const url = URL.createObjectURL(blob);
    soundUrls.set(name, url);
    return url;
  })();
  pendingSoundUrls.set(name, load);
  void load.finally(() => {
    if (pendingSoundUrls.get(name) === load) pendingSoundUrls.delete(name);
  });
  return load;
}

/** Resolves true when the sound started playing. */
export async function playSoundFor(event: SoundEvent): Promise<boolean> {
  const sound = await loadSettings();
  if (!sound || !sound.enabled || sound.browser_enabled === false) return false;
  const name = await resolveSoundName(sound, event);
  if (!name) return false;
  const url = await ensureSoundUrl(name);
  if (!url) return false;
  const audio = new Audio(url);
  const volume = typeof sound.volume === "number" ? sound.volume : 1.0;
  // The host scale tops out at 1.5; HTMLAudioElement at 1.0.
  audio.volume = Math.max(0, Math.min(1, volume));
  try {
    await audio.play();
    return true;
  } catch {
    // Autoplay blocked; the push and toast still notify.
    return false;
  }
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

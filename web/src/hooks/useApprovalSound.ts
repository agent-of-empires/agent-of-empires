import { useEffect, useState } from "react";
import { fetchSettings, fetchSounds, fetchSoundBlob } from "../lib/api";

interface SoundSettings {
  enabled?: boolean;
  volume?: number;
  mode?: "random" | { specific: string };
  on_approval?: string | null;
}

interface CachedSound {
  name: string;
  url: string;
}

let cachedSettings: SoundSettings | null = null;
let cachedSettingsAt = 0;
const SETTINGS_TTL_MS = 30_000;

let cachedSound: CachedSound | null = null;

// The WS replays stored events on connect; don't chime for approvals that were already pending.
const REPLAY_QUIET_MS = 1500;

export function clearApprovalSoundCache(): void {
  cachedSettings = null;
  cachedSettingsAt = 0;
  if (cachedSound) {
    URL.revokeObjectURL(cachedSound.url);
    cachedSound = null;
  }
}

async function loadSettings(): Promise<SoundSettings | null> {
  const now = Date.now();
  if (cachedSettings && now - cachedSettingsAt < SETTINGS_TTL_MS) {
    return cachedSettings;
  }
  const data = await fetchSettings();
  const sound = data?.sound as SoundSettings | undefined;
  cachedSettings = sound ?? null;
  cachedSettingsAt = now;
  return cachedSettings;
}

async function resolveSoundName(sound: SoundSettings): Promise<string | null> {
  const override = sound.on_approval?.trim();
  if (override) return override;
  if (typeof sound.mode === "object" && sound.mode !== null) {
    const specific = sound.mode.specific?.trim();
    if (specific) return specific;
  }
  if (sound.mode === "random") {
    const list = await fetchSounds();
    if (list.length > 0) {
      return list[Math.floor(Math.random() * list.length)] ?? null;
    }
  }
  return null;
}

async function ensureSoundUrl(name: string): Promise<string | null> {
  if (cachedSound && cachedSound.name === name) {
    return cachedSound.url;
  }
  // `<audio src>` bypasses the fetch auth interceptor, so load the bytes via fetch.
  const blob = await fetchSoundBlob(name);
  if (!blob) return null;
  if (cachedSound) {
    URL.revokeObjectURL(cachedSound.url);
  }
  const url = URL.createObjectURL(blob);
  cachedSound = { name, url };
  return url;
}

async function playApprovalSound(): Promise<void> {
  const sound = await loadSettings();
  if (!sound || !sound.enabled) return;
  const name = await resolveSoundName(sound);
  if (!name) return;
  const url = await ensureSoundUrl(name);
  if (!url) return;
  const audio = new Audio(url);
  const volume = typeof sound.volume === "number" ? sound.volume : 1.0;
  // The host scale tops out at 1.5; HTMLAudioElement at 1.0.
  audio.volume = Math.max(0, Math.min(1, volume));
  try {
    await audio.play();
  } catch {
    // Autoplay blocked; the push and toast still notify.
  }
}

export function useApprovalSound(pendingCount: number): void {
  const [trackedPendingCount, setTrackedPendingCount] = useState(pendingCount);
  const [quietPeriodDone, setQuietPeriodDone] = useState(false);
  const [playbackToken, setPlaybackToken] = useState(0);

  useEffect(() => {
    const timer = setTimeout(() => {
      setQuietPeriodDone(true);
    }, REPLAY_QUIET_MS);
    return () => clearTimeout(timer);
  }, []);

  useEffect(() => {
    if (playbackToken === 0) return;
    const timer = setTimeout(() => void playApprovalSound(), 0);
    return () => clearTimeout(timer);
  }, [playbackToken]);

  if (pendingCount !== trackedPendingCount) {
    if (trackedPendingCount === 0 && pendingCount !== 0 && quietPeriodDone) {
      setPlaybackToken((t) => t + 1);
    }
    setTrackedPendingCount(pendingCount);
  }
}

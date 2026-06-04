// Browser-side approval sound for the structured view.
//
// When the structured view transitions from "no pending approvals" to "at least
// one pending approval", play the configured sound (from `[sound]` in
// the daemon's config) in the browser. The host-side sound module only
// fires on session-status transitions and runs on the server host, so
// it is the wrong side of the wire when the dashboard is on a separate
// machine. See #1038.
//
// Trade-offs:
//   - Autoplay policies: the first play() may reject if the user has
//     not interacted with the dashboard yet. We swallow the rejection;
//     the OS push notification and in-app toast still surface the
//     approval, so the missing sound is graceful, not fatal.
//   - Auth: the fetch interceptor injects `Authorization: Bearer` on
//     every fetch (web/src/lib/fetchInterceptor.ts), and `<audio src>`
//     does not run through that interceptor, so the bytes are fetched
//     and handed to Audio via `URL.createObjectURL`.

import { useEffect, useRef } from "react";
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

/** Grace window after mount during which 0->>=1 transitions are
 *  swallowed. The structured view WS replays every stored event for the
 *  session on connect, so a fresh page load on a session with pending
 *  approvals would otherwise ring the chime even though nothing new
 *  happened. The OS push only fires on the live broadcast edge in the
 *  supervisor (no replay path), so the two channels stay consistent.
 *
 *  Reconnects do not unmount `StructuredView`, so this gate only swallows
 *  the initial-load case; new approvals that arrive while the socket
 *  is offline still chime after the reducer applies them on reconnect. */
const REPLAY_QUIET_MS = 1500;

/** Drop the module-level caches. Called by the logout flow so the next
 *  authenticated user sees their own settings and doesn't replay the
 *  previous user's blob URL. */
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

async function resolveSoundName(
  sound: SoundSettings,
): Promise<string | null> {
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
  // The host-side scale is 0.1-1.5 with 1.0 = normal; HTMLAudioElement
  // tops out at 1.0, so clamp directly rather than rescaling 1.5 down
  // to a 0.667 audible "normal".
  audio.volume = Math.max(0, Math.min(1, volume));
  try {
    await audio.play();
  } catch {
    // Autoplay policy or backgrounded tab. The push and in-app toast
    // still cover the user-visible signal; the missing chime is not
    // worth a retry storm.
  }
}

/** Watch `pendingCount` for a 0 -> >=1 edge and play the configured
 *  approval sound. Pure passive: the hook does not mount any UI and
 *  has no return value. */
export function useApprovalSound(pendingCount: number): void {
  const lastCount = useRef(pendingCount);
  // `mountedAt` is set once in the mount effect rather than in the
  // useRef initialiser so `Date.now()` isn't re-evaluated on every
  // render. The sentinel 0 doubles as a "not yet armed" marker; the
  // transition effect treats 0 the same as a too-recent mount.
  const mountedAt = useRef<number>(0);
  useEffect(() => {
    mountedAt.current = Date.now();
  }, []);
  useEffect(() => {
    const prev = lastCount.current;
    lastCount.current = pendingCount;
    if (prev !== 0 || pendingCount === 0) return;
    if (
      mountedAt.current === 0 ||
      Date.now() - mountedAt.current < REPLAY_QUIET_MS
    ) {
      return;
    }
    void playApprovalSound();
  }, [pendingCount]);
}

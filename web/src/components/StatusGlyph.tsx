import { useEffect, useState } from "react";
import type { SessionStatus } from "../lib/types";
import { isFreshIdle } from "../lib/session";
import { useIdleDecayWindowMs } from "../lib/idleDecay";

/** Animated spinner frames from rattles (https://github.com/vyfor/rattles) */
const RATTLES: Record<string, { frames: string[]; interval: number }> = {
  dots: {
    frames: ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    interval: 220,
  },
  orbit: { frames: ["⠃", "⠉", "⠘", "⠰", "⢠", "⣀", "⡄", "⠆"], interval: 400 },
  breathe: {
    frames: ["⠀", "⠂", "⠌", "⡑", "⢕", "⢝", "⣫", "⣟", "⣿", "⣟", "⣫", "⢝", "⢕", "⡑", "⠌", "⠂", "⠀"],
    interval: 180,
  },
};

/** Which statuses get animated spinners vs static glyphs */
const STATUS_RATTLE: Partial<Record<SessionStatus, keyof typeof RATTLES>> = {
  Running: "dots",
  Waiting: "orbit",
  Starting: "breathe",
  Creating: "orbit",
};

/** Static glyphs for non-animated statuses (braille family) */
const STATIC_GLYPH: Record<SessionStatus, string> = {
  Running: "⠋",
  Waiting: "⠃",
  Idle: "⠒",
  Error: "✕",
  Starting: "⠀",
  Stopped: "⠒",
  Unknown: "⠤",
  Deleting: "✕",
  Creating: "⠀",
};

/** Glyph for a dormant (idle-reaped, resumable) structured worker. */
const DORMANT_GLYPH = "⠶";

/** Slowed-down `breathe` rattle for a freshly-stopped Idle session. */
const FRESH_IDLE_RATTLE = { frames: RATTLES.breathe!.frames, interval: 280 };

/** Animated status glyph that cycles through rattles frames. */
export function StatusGlyph({
  status,
  createdAt,
  idleEnteredAt,
  dormant = false,
}: {
  status: SessionStatus;
  createdAt: string | null;
  idleEnteredAt?: string | null;
  dormant?: boolean;
}) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const isFresh =
    !dormant && status === "Idle" && isFreshIdle({ status, idle_entered_at: idleEnteredAt ?? null }, idleDecayWindowMs);
  const rattleKey = STATUS_RATTLE[status];
  // A dormant worker is a resting state: no rattle, and its own static glyph.
  const rattle = dormant ? undefined : isFresh ? FRESH_IDLE_RATTLE : rattleKey ? RATTLES[rattleKey] : undefined;
  const parsed = createdAt ? Date.parse(createdAt) : 0;
  const epoch = Number.isNaN(parsed) ? 0 : parsed;
  const [frame, setFrame] = useState(() => {
    if (!rattle) return 0;
    return Math.floor((Date.now() - epoch) / rattle.interval) % rattle.frames.length;
  });

  useEffect(() => {
    if (!rattle) return;
    const r = rattle;
    const computeFrame = () => Math.floor((Date.now() - epoch) / r.interval) % r.frames.length;
    const initial = setTimeout(() => setFrame(computeFrame()), 0);
    const id = setInterval(() => setFrame(computeFrame()), r.interval);
    return () => {
      clearTimeout(initial);
      clearInterval(id);
    };
  }, [rattle, epoch]);

  if (!rattle) {
    return <>{dormant ? DORMANT_GLYPH : STATIC_GLYPH[status]}</>;
  }
  return <>{rattle.frames[frame]}</>;
}

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
    frames: [
      "⠀",
      "⠂",
      "⠌",
      "⡑",
      "⢕",
      "⢝",
      "⣫",
      "⣟",
      "⣿",
      "⣟",
      "⣫",
      "⢝",
      "⢕",
      "⡑",
      "⠌",
      "⠂",
      "⠀",
    ],
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

/** Slowed-down `breathe` rattle for a freshly-stopped Idle session.
 *  Reuses the same animation as Starting on purpose; differentiation is
 *  by color (Starting uses `--color-text-muted`, fresh-idle uses
 *  `--color-status-fresh-idle`). The longer interval (vs Starting) reads
 *  as "gentle reminder" rather than "actively transitioning". */
const FRESH_IDLE_RATTLE = { frames: RATTLES.breathe!.frames, interval: 280 };

/** Animated status glyph that cycles through rattles frames.
 *  Each instance offsets by `createdAt` so spinners look unique.
 *
 *  When `idleEnteredAt` is within the decay window, an Idle session
 *  renders an animated `breathe` rattle styled with
 *  `--color-status-fresh-idle`. The motion matches the visual language of
 *  the other attention-worthy states (Running, Waiting, Starting all
 *  animate); without it the row would be the only static-glyph state in
 *  the "needs attention" bucket, which reads inconsistent. The shape
 *  variation also serves as a redundant cue alongside color for
 *  colorblind users and monochrome terminals. */
export function StatusGlyph({
  status,
  createdAt,
  idleEnteredAt,
}: {
  status: SessionStatus;
  createdAt: string | null;
  idleEnteredAt?: string | null;
}) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const isFresh =
    status === "Idle" &&
    isFreshIdle(
      { status, idle_entered_at: idleEnteredAt ?? null },
      idleDecayWindowMs,
    );
  const rattleKey = STATUS_RATTLE[status];
  const rattle = isFresh
    ? FRESH_IDLE_RATTLE
    : rattleKey
      ? RATTLES[rattleKey]
      : undefined;
  const parsed = createdAt ? Date.parse(createdAt) : 0;
  const epoch = Number.isNaN(parsed) ? 0 : parsed;
  const [frame, setFrame] = useState(() => {
    if (!rattle) return 0;
    return (
      Math.floor((Date.now() - epoch) / rattle.interval) % rattle.frames.length
    );
  });

  useEffect(() => {
    if (!rattle) return;
    const r = rattle;
    const computeFrame = () =>
      Math.floor((Date.now() - epoch) / r.interval) % r.frames.length;
    const initial = setTimeout(() => setFrame(computeFrame()), 0);
    const id = setInterval(() => setFrame(computeFrame()), r.interval);
    return () => {
      clearTimeout(initial);
      clearInterval(id);
    };
  }, [rattle, epoch]);

  if (!rattle) {
    return <>{STATIC_GLYPH[status]}</>;
  }
  return <>{rattle.frames[frame]}</>;
}

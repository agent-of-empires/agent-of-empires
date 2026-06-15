import { useEffect } from "react";
import { createPortal } from "react-dom";
import type { CheatEffect, CheatEffectKind } from "../../lib/cheats";

// How long each effect lives before it self-cleans. Matches the CSS animation
// durations in index.css; the overlay unmounts when the longest piece ends.
const CHEAT_DURATION_MS: Record<CheatEffectKind, number> = {
  fly: 1600,
  confetti: 2200,
  flash: 600,
  pulse: 1000,
};

const CONFETTI_COUNT = 14;

interface Props {
  effect: CheatEffect;
  // Bump to replay the same cheat; used as the React key by the caller.
  onDone: () => void;
}

// Full-screen, pointer-events-none overlay so palette interaction is never
// blocked. Rendered into a body portal so the sprite is not clipped by the
// palette card. Self-cleans by calling onDone after the effect duration.
export function CheatOverlay({ effect, onDone }: Props) {
  useEffect(() => {
    const t = setTimeout(onDone, CHEAT_DURATION_MS[effect.kind]);
    return () => clearTimeout(t);
  }, [effect, onDone]);

  return createPortal(
    <div className="pointer-events-none fixed inset-0 z-[100] overflow-hidden" data-testid="cheat-overlay" aria-hidden>
      {renderEffect(effect)}
    </div>,
    document.body,
  );
}

function renderEffect(effect: CheatEffect) {
  switch (effect.kind) {
    case "fly":
      return (
        <span
          className={effect.dir === "rtl" ? "animate-cheat-fly-rtl" : "animate-cheat-fly-ltr"}
          style={{ position: "absolute", top: "50%", left: 0, fontSize: "6rem", lineHeight: 1 }}
        >
          {effect.emoji}
        </span>
      );
    case "confetti":
      return (
        <>
          {Array.from({ length: CONFETTI_COUNT }, (_, i) => (
            <span
              key={i}
              className="animate-cheat-confetti-fall"
              style={{
                position: "absolute",
                top: 0,
                left: `${(i / CONFETTI_COUNT) * 100}%`,
                fontSize: "2rem",
                animationDelay: `${(i % 5) * 0.12}s`,
              }}
            >
              {effect.emoji}
            </span>
          ))}
        </>
      );
    case "flash":
      return (
        <div className="animate-cheat-flash" style={{ position: "absolute", inset: 0, background: effect.color }} />
      );
    case "pulse":
      return (
        <span
          className="animate-cheat-pulse"
          style={{ position: "absolute", top: "50%", left: "50%", fontSize: "8rem", lineHeight: 1 }}
        >
          {effect.emoji}
        </span>
      );
  }
}

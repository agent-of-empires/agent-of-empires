// PROTOTYPE (throwaway branch prototype/web-visual-refresh): compares dashboard surface treatments.
// `?variant=A|B|C` picks one; the floating bar or the ← / → keys cycle them.
import { useEffect, useState } from "react";
import "./visualVariants.css";

const VARIANTS = [
  { key: "A", name: "Today: flat panels, hairlines" },
  { key: "B", name: "Cards on a dark canvas (Claude Code)" },
  { key: "C", name: "Flat neutral, pill tabs (Conductor)" },
] as const;

function current(): string {
  const v = new URLSearchParams(window.location.search).get("variant");
  return VARIANTS.some((x) => x.key === v) ? v! : "A";
}

export function PrototypeVariantBar() {
  const [variant, setVariant] = useState(current);
  useEffect(() => {
    document.documentElement.dataset.protoVariant = variant;
    const url = new URL(window.location.href);
    url.searchParams.set("variant", variant);
    window.history.replaceState(window.history.state, "", url);
  }, [variant]);
  const step = (d: number) =>
    setVariant((v) => VARIANTS[(VARIANTS.findIndex((x) => x.key === v) + d + VARIANTS.length) % VARIANTS.length]!.key);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (t?.closest("input, textarea, [contenteditable], [data-term]")) return;
      if (e.key === "ArrowLeft" && e.altKey) step(-1);
      if (e.key === "ArrowRight" && e.altKey) step(1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });
  const info = VARIANTS.find((x) => x.key === variant)!;
  return (
    <div className="proto-variant-bar" data-testid="proto-variant-bar">
      <button type="button" onClick={() => step(-1)} aria-label="Previous variant">
        ←
      </button>
      <span>
        {info.key} · {info.name}
      </span>
      <button type="button" onClick={() => step(1)} aria-label="Next variant">
        →
      </button>
    </div>
  );
}

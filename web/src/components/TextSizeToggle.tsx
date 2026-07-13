import { useEffect, useState } from "react";
import { Type } from "lucide-react";

type SizeKey = "sm" | "base" | "lg" | "xl";

const SIZE_STEPS: SizeKey[] = ["sm", "base", "lg", "xl"];
const SIZE_LABELS: Record<SizeKey, string> = {
  sm: "Small",
  base: "Default",
  lg: "Large",
  xl: "Extra Large",
};

const STORAGE_KEY = "aoe-text-size";

export function TextSizeToggle() {
  const [size, setSize] = useState<SizeKey>("base");
  const [mounted, setMounted] = useState(false);

  useEffect(() => {
    setMounted(true);
    const stored = localStorage.getItem(STORAGE_KEY) as SizeKey | null;
    if (stored && SIZE_STEPS.includes(stored)) {
      setSize(stored);
      applySize(stored);
    } else {
      applySize("base");
    }
  }, []);

  const applySize = (newSize: SizeKey) => {
    const root = document.documentElement;
    SIZE_STEPS.forEach((s) => root.classList.remove(`text-${s}`));
    root.classList.add(`text-${newSize}`);
  };

  const cycleSize = () => {
    const currentIndex = SIZE_STEPS.indexOf(size);
    const nextIndex = (currentIndex + 1) % SIZE_STEPS.length;
    const nextSize: SizeKey = SIZE_STEPS[nextIndex] ?? "base";
    setSize(nextSize);
    localStorage.setItem(STORAGE_KEY, nextSize);
    applySize(nextSize);
  };

  if (!mounted) {
    return (
      <button
        type="button"
        className="w-8 h-8 flex items-center justify-center rounded-md transition-colors text-text-dim hover:text-text-secondary hover:bg-surface-700/50"
        aria-label="Text size"
        disabled
      >
        <Type className="h-4 w-4" strokeWidth={2.5} />
      </button>
    );
  }

  return (
    <div className="relative inline-flex items-center">
      <button
        type="button"
        onClick={cycleSize}
        className="w-8 h-8 flex items-center justify-center rounded-md transition-colors text-text-secondary hover:text-text-primary hover:bg-surface-700/50"
        aria-label={`Text size: ${SIZE_LABELS[size]}. Click to cycle.`}
        aria-haspopup="true"
        aria-expanded="false"
      >
        <Type className="h-5 w-5" strokeWidth={2.5} />
      </button>
      <span className="sr-only">Text size: {SIZE_LABELS[size]}</span>
    </div>
  );
}
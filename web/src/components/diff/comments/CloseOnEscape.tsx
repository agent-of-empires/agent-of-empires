import { useEffect } from "react";

/** Stands in for a dialog whose chunk is still loading, so Escape is not lost. */
export function CloseOnEscape({ onClose }: { onClose: () => void }) {
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);
  return null;
}

import { useEffect, useSyncExternalStore, type RefObject } from "react";
import { writeClipboard } from "../../lib/clipboard";
import { inputTraceSnapshot, subscribeInputTrace, traceInputEvents } from "../../lib/inputTrace";

const SHOWN = 30;

/** `?livedebug=1` trace of both hidden inputs' events and what they sent, with a copy button for bug reports. */
export function InputTraceOverlay({
  inputRef,
  active,
}: {
  inputRef: RefObject<HTMLTextAreaElement | null>;
  /** Only the active terminal traces App's shared keyboard proxy. */
  active: boolean;
}) {
  const entries = useSyncExternalStore(subscribeInputTrace, inputTraceSnapshot);
  useEffect(() => {
    const live = inputRef.current;
    const proxy = active ? document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]") : null;
    const stops = [live && traceInputEvents(live, "live"), proxy && traceInputEvents(proxy, "proxy")];
    return () => {
      for (const stop of stops) stop?.();
    };
  }, [inputRef, active]);

  return (
    <div className="absolute bottom-10 inset-x-1 z-20 flex flex-col items-start gap-1 pointer-events-none">
      <button
        type="button"
        // Keeps the hidden input focused, so copying does not dismiss the keyboard mid-trace.
        onPointerDown={(e) => e.preventDefault()}
        onClick={() => void writeClipboard(entries.join("\n"))}
        className="pointer-events-auto font-mono text-[10px] text-amber-300 bg-black/80 rounded px-1.5 py-0.5"
      >
        copy trace ({entries.length})
      </button>
      <div
        aria-hidden="true"
        className="max-w-full overflow-hidden font-mono text-[9px] leading-tight text-amber-300 bg-black/80 rounded px-1.5 py-1 whitespace-pre"
        data-input-trace
      >
        {entries.slice(-SHOWN).join("\n")}
      </div>
    </div>
  );
}

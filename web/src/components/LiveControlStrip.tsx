import type { RefObject } from "react";
import type { LiveOwnerKind } from "../hooks/useLiveTerminal";

const OWNER_PLACE: Record<LiveOwnerKind, string> = {
  tui: "the TUI",
  web: "another Web dashboard window",
};

interface Props {
  /** The steady notice: view only while another window has control, or a read-only Daemon. */
  mode: "view-only" | "read-only" | null;
  /** A blocked key's text, shown for the hold. */
  hold: string | null;
  ownerKind: LiveOwnerKind | null;
  /** The TUI has the stream, so this tab gets the slower fallback. */
  lag: boolean;
  collapsed: boolean;
  onTakeControl: () => void;
  onCollapse: () => void;
  onExpand: () => void;
  takeControlRef: RefObject<HTMLButtonElement | null>;
}

/** Docked under a view-only terminal, above where you type. It never shows while this tab holds control. */
export function LiveControlStrip({
  mode,
  hold,
  ownerKind,
  lag,
  collapsed,
  onTakeControl,
  onCollapse,
  onExpand,
  takeControlRef,
}: Props) {
  if (!mode && !hold) return null;
  const viewOnly = mode === "view-only";
  if (viewOnly && collapsed && !hold) {
    return (
      <div data-live-control-strip="collapsed" className="flex shrink-0 justify-end px-3 py-1">
        <button
          type="button"
          onClick={onExpand}
          aria-label="View only. Show notice"
          className="h-8 px-3 rounded-md text-xs text-text-primary bg-surface-800 hover:bg-surface-700 cursor-pointer"
        >
          View only
        </button>
      </div>
    );
  }
  const message = hold
    ? hold
    : viewOnly
      ? `Open in ${ownerKind ? OWNER_PLACE[ownerKind] : "another window"}. Take control to type.`
      : "This Daemon is read-only.";
  const outlined = viewOnly && hold !== null;
  return (
    <div
      data-live-control-strip={hold ? "hold" : mode}
      className={`flex shrink-0 items-center gap-2 min-h-10 pl-3 pr-1 py-1 text-xs border-t ${
        viewOnly
          ? "bg-brand-600/14 border-brand-600/30 text-text-primary"
          : "bg-surface-850 border-surface-700/60 text-text-primary"
      } ${outlined ? "ring-1 ring-inset ring-brand-500" : ""}`}
    >
      <span role="status" className="flex-1 min-w-0 break-words">
        {message}
        {viewOnly && lag && !hold && (
          <>
            {" "}
            <span className="whitespace-nowrap">· Updates may lag</span>
          </>
        )}
      </span>
      {viewOnly && (
        <>
          <button
            ref={takeControlRef}
            type="button"
            onClick={onTakeControl}
            data-live-take-control
            className="h-8 shrink-0 px-3 rounded-md text-xs font-semibold bg-brand-600 hover:bg-brand-500 text-text-on-brand cursor-pointer"
          >
            Take control
          </button>
          <button
            type="button"
            onClick={onCollapse}
            aria-label="Collapse notice"
            className="h-8 w-8 shrink-0 rounded-md text-text-primary hover:bg-surface-700/50 cursor-pointer text-base leading-none"
          >
            &times;
          </button>
        </>
      )}
    </div>
  );
}

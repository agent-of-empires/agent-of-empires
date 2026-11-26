import type { ConnectionBannerState } from "../hooks/useConnectionBanner";
import { StrokeIcon } from "./icons";

/** Full-width banner shown when the Daemon is unreachable. */
export function DisconnectBanner({ state }: { state: ConnectionBannerState }) {
  if (state === "reconnected") {
    return (
      <div
        role="status"
        className="bg-status-running/10 border-b border-status-running/30 px-4 py-2 flex items-center justify-center gap-2 text-[13px] text-status-running animate-fade-in"
      >
        <StrokeIcon size={14} strokeWidth="2">
          <polyline points="20 6 9 17 4 12" />
        </StrokeIcon>
        Reconnected
      </div>
    );
  }

  return (
    <div
      role="alert"
      className="bg-status-error/10 border-b border-status-error/30 px-4 py-2 flex items-center justify-center gap-2 text-[13px] text-status-error animate-fade-in"
    >
      <span className="w-1.5 h-1.5 rounded-full bg-status-error animate-pulse shrink-0" />
      <span>{navigator.onLine === false ? "Offline. Reconnecting…" : "Daemon unreachable. Reconnecting…"}</span>
      {/* The list polls every few seconds on its own; this is for whoever would rather not wait. */}
      <button
        type="button"
        onClick={() => window.location.reload()}
        className="ml-1 min-h-8 px-2 -my-1 rounded-md font-medium underline underline-offset-2 cursor-pointer hover:text-text-primary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand-500"
      >
        Try again
      </button>
    </div>
  );
}

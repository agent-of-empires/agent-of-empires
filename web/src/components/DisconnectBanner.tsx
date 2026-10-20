import type { ConnectionBannerState } from "../hooks/useConnectionBanner";
import { StrokeIcon } from "./icons";

/** Full-width banner shown when the backend server is unreachable. */
export function DisconnectBanner({ state }: { state: ConnectionBannerState }) {
  if (state === "reconnected") {
    return (
      <div
        role="status"
        className="bg-status-running/10 border-b border-status-running/30 px-4 py-2 flex items-center justify-center gap-2 text-xs font-mono text-status-running animate-fade-in"
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
      className="bg-status-error/10 border-b border-status-error/30 px-4 py-2 flex items-center justify-center gap-2 text-xs font-mono text-status-error animate-fade-in"
    >
      <span className="w-1.5 h-1.5 rounded-full bg-status-error animate-pulse shrink-0" />
      Server unreachable. Reconnecting automatically...
    </div>
  );
}

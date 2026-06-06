import { useEffect, useState } from "react";
import { onServerDownChange, isServerDown } from "../lib/connectionState";

/**
 * Full-width banner shown when the backend server is unreachable. Replaces the
 * repeated "network error" toast spam with a single persistent notification
 * that auto-dismisses when the connection recovers.
 */
export function DisconnectBanner() {
  const [down, setDown] = useState(isServerDown);
  const [reconnected, setReconnected] = useState(false);

  useEffect(() => {
    return onServerDownChange((isDown) => {
      if (!isDown && down) {
        // Server came back. Flash a "reconnected" message briefly.
        setReconnected(true);
        setDown(false);
        const t = setTimeout(() => setReconnected(false), 3000);
        return () => clearTimeout(t);
      }
      setDown(isDown);
      if (isDown) setReconnected(false);
    });
  }, [down]);

  if (reconnected) {
    return (
      <div
        role="status"
        className="bg-status-running/10 border-b border-status-running/30 px-4 py-2 flex items-center justify-center gap-2 text-xs font-mono text-status-running animate-fade-in"
      >
        <svg
          width="14"
          height="14"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <polyline points="20 6 9 17 4 12" />
        </svg>
        Reconnected
      </div>
    );
  }

  if (!down) return null;

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

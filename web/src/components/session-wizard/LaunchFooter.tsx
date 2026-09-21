import { useEffect } from "react";
import { useServerDown, OFFLINE_TITLE } from "../../lib/connectionState";

interface LaunchData {
  path: string;
  tool: string;
  scratch: boolean;
  [key: string]: unknown;
}

interface Props {
  data: LaunchData;
  isSubmitting: boolean;
  error: string | null;
  onSubmit: () => void;
  /** CityHall name-only mode: the server derives the project. */
  nameOnly?: boolean;
  /** False until the wizard's profile defaults have been applied (or their
   *  fetch has failed); a launch before then would submit placeholder
   *  sandbox/worktree/yolo values. Omitted means ready. */
  defaultsReady?: boolean;
}

const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.userAgent);

/** Launch button, submit gate, Cmd/Ctrl+Enter shortcut, and error banners. */
export function LaunchFooter({ data, isSubmitting, error, onSubmit, nameOnly = false, defaultsReady = true }: Props) {
  const offline = useServerDown();
  // Scratch and name-only sessions get their directory from the server.
  const canSubmit =
    defaultsReady && !isSubmitting && !offline && (nameOnly || data.scratch || !!data.path) && !!data.tool;

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && canSubmit) {
        e.preventDefault();
        onSubmit();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [canSubmit, onSubmit]);

  return (
    <div>
      {error && <div className="text-sm text-status-error bg-status-error/10 rounded-lg p-3 mb-4">{error}</div>}
      {offline && (
        <div className="text-sm text-status-error bg-status-error/10 rounded-lg p-3 mb-4">{OFFLINE_TITLE}</div>
      )}
      <button
        onClick={onSubmit}
        disabled={!canSubmit}
        className={`w-full py-3 rounded-lg font-semibold text-sm transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
          !canSubmit
            ? "bg-brand-600/50 text-surface-900/50 cursor-not-allowed"
            : "bg-brand-600 hover:bg-brand-700 active:bg-brand-800 text-surface-900 cursor-pointer"
        }`}
      >
        {isSubmitting ? (
          <span className="flex items-center justify-center gap-2">
            <svg className="animate-spin h-4 w-4" viewBox="0 0 24 24">
              <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" />
              <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
            </svg>
            Creating session...
          </span>
        ) : (
          <span>
            Launch session <span className="opacity-60">({isMac ? "⌘" : "Ctrl"}+Enter)</span>
          </span>
        )}
      </button>
    </div>
  );
}

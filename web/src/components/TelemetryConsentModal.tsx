import { useEffect, useRef } from "react";

interface Props {
  /// Called with the user's choice. The parent persists it via the consent
  /// endpoint and hides the modal.
  onChoose: (enabled: boolean) => void;
}

/// One-time opt-in prompt for the web dashboard, mirroring the TUI walkthrough
/// pane and standalone popup. Shown on first load when the user has not yet
/// answered and DO_NOT_TRACK is not set. Declining ("Not now") still records a
/// response so it does not re-appear every visit.
export function TelemetryConsentModal({ onChoose }: Props) {
  const declineRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    // Default focus on decline so a stray Enter never silently opts in.
    declineRef.current?.focus();
  }, []);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="telemetry-modal-title"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[460px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-surface-700">
          <h2
            id="telemetry-modal-title"
            className="text-sm font-semibold text-text-bright"
          >
            Help improve aoe?
          </h2>
        </div>

        <div className="p-5 space-y-3 text-sm text-text-secondary">
          <p>
            Turning it on shows us how aoe is actually used, so we can
            prioritize the features that matter most. It is off by default and
            sends anonymous counts only: number of sessions, which agents and
            model families, your aoe version, and OS.
          </p>
          <p className="text-text-dim">
            It never sends prompts, file paths, names, branch names, or
            commands. You can change this any time under Settings &rarr;
            Telemetry.
          </p>
        </div>

        <div className="px-5 py-4 border-t border-surface-700 flex justify-end gap-2">
          <button
            ref={declineRef}
            onClick={() => onChoose(false)}
            className="h-8 px-3 rounded-md border border-surface-700/50 text-sm text-text-secondary hover:bg-surface-850 hover:text-text-primary transition-colors duration-150 cursor-pointer"
          >
            Not now
          </button>
          <button
            onClick={() => onChoose(true)}
            className="h-8 px-3 rounded-md bg-brand-600 text-sm text-white hover:bg-brand-500 transition-colors duration-150 cursor-pointer"
          >
            Enable telemetry
          </button>
        </div>
      </div>
    </div>
  );
}

import { useState } from "react";
import type { VolumeIgnoresGlobPreview } from "../../lib/api";
import { ConfirmCreateDialog } from "./ConfirmCreateDialog";

interface Props {
  globs: VolumeIgnoresGlobPreview[];
  /** Receives whether "Don't show this again" was ticked. */
  onConfirm: (dontShowAgain: boolean) => Promise<void> | void;
  onCancel: () => void;
}

/** Warns that glob `volume_ignores` expand to a point-in-time snapshot before a sandbox create. */
export function VolumeIgnoresGlobDialog({ globs, onConfirm, onCancel }: Props) {
  const [dontShowAgain, setDontShowAgain] = useState(false);
  const matchCount = globs.reduce((sum, g) => sum + g.matched_paths.length, 0);
  const label = "Don't show this again";

  return (
    <ConfirmCreateDialog
      id="volume-ignores-glob"
      title="Glob volume_ignores"
      confirmLabel="Proceed"
      onConfirm={() => onConfirm(dontShowAgain)}
      onCancel={onCancel}
    >
      <p className="text-[13px] text-text-secondary">
        This session's <span className="font-mono text-text-primary">volume_ignores</span> include glob patterns. They
        are expanded against the workspace now, matching <span className="text-text-primary">{matchCount}</span>{" "}
        director
        {matchCount === 1 ? "y" : "ies"}, and one ignore mount is created per match.
      </p>

      <ul className="space-y-1 max-h-40 overflow-y-auto" data-testid="volume-ignores-glob-list">
        {globs.map((g) => (
          <li key={g.pattern} className="text-[13px] text-text-secondary flex items-baseline justify-between gap-3">
            <span className="font-mono text-text-primary truncate">{g.pattern}</span>
            <span className="text-text-dim whitespace-nowrap">{g.matched_paths.length} matched</span>
          </li>
        ))}
      </ul>

      <p className="text-[12px] text-text-dim">
        This is a point-in-time snapshot. Directories a build creates later inside the container are not hidden;
        re-create the session to pick up new matches.
      </p>

      <label
        className="flex items-start gap-2.5 cursor-pointer group"
        data-testid="volume-ignores-glob-dont-show-again"
        data-checked={dontShowAgain ? "true" : "false"}
      >
        <input
          type="checkbox"
          checked={dontShowAgain}
          onChange={(e) => setDontShowAgain(e.target.checked)}
          aria-label={label}
          className="peer sr-only"
        />
        <span
          aria-hidden="true"
          className={`mt-0.5 w-4 h-4 rounded border flex items-center justify-center shrink-0 transition-colors peer-focus-visible:outline peer-focus-visible:outline-2 peer-focus-visible:outline-offset-2 peer-focus-visible:outline-green-500 ${
            dontShowAgain ? "bg-green-500 border-green-500" : "border-surface-600 group-hover:border-surface-500"
          }`}
        >
          {dontShowAgain && (
            <svg width="10" height="10" viewBox="0 0 10 10" fill="none">
              <path d="M2 5L4 7L8 3" stroke="white" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          )}
        </span>
        <span className="text-[13px] text-text-secondary group-hover:text-text-primary transition-colors">{label}</span>
      </label>
    </ConfirmCreateDialog>
  );
}

import { useEffect, useRef } from "react";
import type { CreateProgress } from "../../lib/types";

const STAGE_LABEL: Record<CreateProgress["stage"], string> = {
  preparing: "Setting up workspace",
  starting_container: "Starting container",
  running_hooks: "Running on_create hooks",
  starting: "Starting session",
};

/** Live stage and hook output of an in-flight create, like the TUI's Running Hooks dialog. */
export function CreateProgressView({ progress }: { progress: CreateProgress | null }) {
  const outputRef = useRef<HTMLPreElement | null>(null);
  // Set while the reader sits at the bottom; scrolling up to read stops the follow.
  const followRef = useRef(true);
  const lines = progress?.output ?? [];

  // Keyed on the snapshot, not the line count: a full window keeps its length while the tail changes.
  useEffect(() => {
    const el = outputRef.current;
    if (el && followRef.current) el.scrollTop = el.scrollHeight;
  }, [progress]);

  return (
    <div data-testid="create-progress" className="flex flex-col gap-3 min-h-0 h-full">
      <div className="flex items-center gap-2">
        <svg className="animate-spin h-4 w-4 text-brand-500 shrink-0" viewBox="0 0 24 24" aria-hidden="true">
          <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" />
          <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
        </svg>
        <span className="text-sm font-medium text-text-primary">
          {STAGE_LABEL[progress?.stage ?? "preparing"]}
          ...
        </span>
      </div>
      {progress?.hook && (
        <p className="text-xs font-mono text-text-secondary break-all" data-testid="create-progress-hook">
          $ {progress.hook}
        </p>
      )}
      {lines.length > 0 && (
        <pre
          ref={outputRef}
          onScroll={(e) => {
            const el = e.currentTarget;
            followRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
          }}
          data-testid="create-progress-output"
          className="flex-1 min-h-[8rem] max-h-[50vh] overflow-auto bg-surface-950 border border-surface-700/40 rounded-md p-2 text-[11px] leading-snug font-mono text-text-dim whitespace-pre-wrap break-all"
        >
          {lines.join("\n")}
        </pre>
      )}
    </div>
  );
}

// Chrome shared by the diff viewer and the plain file viewer: their header
// controls and the centered placeholder both panes fall back to.

import type { ReactNode } from "react";
import { useWebSettings } from "../../hooks/useWebSettings";

export const ACTIVE = "bg-surface-700 text-text-primary";
/** Split chosen but the pane is too narrow to show it. */
export const ACTIVE_DEFERRED = "bg-surface-700/50 text-text-secondary";
export const IDLE = "text-text-dim hover:text-text-secondary";
export const GROUP = "flex items-center rounded-md border border-surface-700/50 overflow-hidden";
/** One segment of a GROUP, or a lone toggle beside one. */
export const SEGMENT = "px-2 py-0.5 max-md:py-2 text-[11px] font-mono cursor-pointer transition-colors";

export function ToggleButton({
  pressed,
  onClick,
  title,
  skin,
  children,
}: {
  pressed: boolean;
  onClick: () => void;
  title: string;
  skin?: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={pressed}
      title={title}
      className={`${SEGMENT} ${skin ?? (pressed ? ACTIVE : IDLE)}`}
    >
      {children}
    </button>
  );
}

/** Leading chevron back to `label`. */
export function BackButton({ label, onClick }: { label: string; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      className="min-h-7 max-md:min-h-11 text-text-dim hover:text-text-secondary cursor-pointer transition-colors flex items-center gap-1 text-[12px]"
      title={`Back to ${label.toLowerCase()}`}
      aria-label={`Back to ${label.toLowerCase()}`}
    >
      <svg
        width="14"
        height="14"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.75"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <path d="M15 18l-6-6 6-6" />
      </svg>
      <span className="hidden sm:inline">{label}</span>
    </button>
  );
}

/** Rendered/Raw switch over the shared `markdownPreview` setting. */
export function MarkdownToggle({ className }: { className?: string }) {
  const { settings, update } = useWebSettings();
  return (
    <div className={className ? `${className} ${GROUP}` : GROUP}>
      <ToggleButton
        pressed={settings.markdownPreview === "rendered"}
        onClick={() => update({ markdownPreview: "rendered" })}
        title="Rendered Markdown"
      >
        Rendered
      </ToggleButton>
      <ToggleButton
        pressed={settings.markdownPreview === "raw"}
        onClick={() => update({ markdownPreview: "raw" })}
        title="Raw Markdown source"
      >
        Raw
      </ToggleButton>
    </div>
  );
}

/** A pane's loading, empty or failed state, centered in the space it has. */
export function Centered({ className = "text-text-dim", children }: { className?: string; children: ReactNode }) {
  return (
    <div className={`flex-1 flex items-center justify-center px-4 text-center text-[13px] leading-normal ${className}`}>
      {children}
    </div>
  );
}

/** The "too large to diff/display" placeholder; `hint` differs per pane. */
export function TooLarge({ what, hint }: { what: string; hint: string }) {
  return (
    <Centered>
      <div>
        <p className="mb-1 text-text-secondary">{what}</p>
        <p className="text-[12px]">{hint}</p>
      </div>
    </Centered>
  );
}

import { useId, useRef, useState, type ReactNode } from "react";
import { layerCount, type SandboxImageBannerView } from "../hooks/useSandboxImageBanner";
import { writeClipboard } from "../lib/clipboard";

const WHY = [
  "Sandboxed Sessions run in the current image. Newer AoE versions may need this update, or Structured view stops with an adapter error.",
  "Existing Sessions keep their image. To move one over after an update, use Recreate container in its menu.",
];

// Text stays at text contrast on every tint in every built-in theme (text-primary at least 5.99:1,
// text-subtle 4.84:1). A status hue on its own tint falls short (status-running is 2.68:1 and
// status-error-text 4.07:1 in Catppuccin Latte), so the state's colour is on the dot and border.
const TONES = {
  brand: { bar: "bg-brand-600/10 border-brand-600/30 text-text-primary", dot: "bg-brand-400" },
  done: { bar: "bg-status-running/10 border-status-running/30 text-text-primary", dot: "bg-status-running" },
  failed: { bar: "bg-status-error/10 border-status-error/30 text-text-primary", dot: "bg-status-error" },
};

const linkButton = "min-h-8 underline cursor-pointer hover:opacity-80";

/** Top-of-app banner for a newer sandbox image, with the pull and its outcome. */
export function SandboxImageBanner({ view }: { view: SandboxImageBannerView }) {
  switch (view.kind) {
    case "available":
      return (
        <Bar tone="brand" status="Sandbox image update available" onDismiss={view.onDismiss}>
          <Image name={view.image} />
          <button type="button" onClick={view.onUpdate} className={linkButton}>
            Update now
          </button>
          <Disclosure label="Why?">
            <span className="block">{WHY[0]}</span>
            <span className="mt-3 block">{WHY[1]}</span>
          </Disclosure>
        </Bar>
      );
    case "gated":
      return (
        <Bar
          tone="brand"
          status="Sandbox image update available. Run on the Daemon's machine:"
          onDismiss={view.onDismiss}
        >
          <CopyCommand command={view.command} />
        </Bar>
      );
    case "updating": {
      const { progress } = view;
      return (
        <Bar
          tone="brand"
          lead={<Spinner />}
          status={progress.phase === "unpacking" ? "Unpacking sandbox image…" : "Downloading sandbox image…"}
          onDismiss={view.onHide}
          dismissLabel="Hide sandbox image notice"
          fraction={progress.known ? (progress.done ?? 0) / progress.known : undefined}
        >
          {layerCount(progress) && <span aria-hidden="true">{layerCount(progress)}</span>}
          <span className="text-text-subtle">· Sessions keep running</span>
          {view.onCancel && (
            <button
              type="button"
              onClick={view.onCancel}
              disabled={view.cancelling}
              className={`${linkButton} disabled:opacity-50 disabled:cursor-default`}
            >
              Cancel
            </button>
          )}
        </Bar>
      );
    }
    case "done":
      return <Bar tone="done" status="Sandbox image updated. Applies to new Sessions." onDismiss={view.onDismiss} />;
    case "cancelled":
      return (
        <Bar tone="brand" status="Update cancelled. Sessions keep the current image." onDismiss={view.onDismiss}>
          <button type="button" onClick={view.onUpdate} className={linkButton}>
            Update now
          </button>
        </Bar>
      );
    case "config_unreadable":
      return (
        <Bar
          tone="failed"
          status="AoE can't read its configuration (config.toml or state.toml), so sandbox image updates aren't checked. Fix the file, then reload."
          onDismiss={view.onDismiss}
        />
      );
    case "failed":
      return (
        <Bar tone="failed" status={view.message} onDismiss={view.onDismiss}>
          <button type="button" onClick={view.onRetry} className={linkButton}>
            Retry
          </button>
          <Disclosure label="Details">
            <pre className="whitespace-pre-wrap break-all font-mono">{view.error}</pre>
          </Disclosure>
        </Bar>
      );
  }
}

/** `status` is the only live part, so a changing count beside it isn't announced; `fraction` draws the
 *  progress bar on the bottom edge, which the text duplicates. */
function Bar({
  tone,
  lead,
  status,
  onDismiss,
  dismissLabel = "Dismiss sandbox image notice",
  fraction,
  children,
}: {
  tone: keyof typeof TONES;
  lead?: ReactNode;
  status: string;
  onDismiss?: () => void;
  dismissLabel?: string;
  fraction?: number;
  children?: ReactNode;
}) {
  const { bar, dot } = TONES[tone];
  return (
    <div
      data-testid="sandbox-image-banner"
      className={`relative border-b px-4 py-2 flex flex-wrap items-center justify-center gap-x-3 gap-y-1 text-xs font-mono animate-fade-in ${onDismiss ? "pr-10 sm:pr-4" : ""} ${bar}`}
    >
      {/* On a phone the line wraps, and a lone dot or close button would take a row of its own. */}
      <span className={`hidden sm:block w-1.5 h-1.5 rounded-full shrink-0 ${dot}`} />
      {lead}
      <span role="status">{status}</span>
      {children}
      {fraction !== undefined && (
        <span
          aria-hidden="true"
          data-testid="sandbox-image-progress"
          className="absolute left-0 bottom-0 h-0.5 bg-brand-500"
          style={{ width: `${Math.min(1, Math.max(0, fraction)) * 100}%` }}
        />
      )}
      {onDismiss && (
        <button
          type="button"
          onClick={onDismiss}
          aria-label={dismissLabel}
          className="absolute right-2 top-1 sm:static sm:ml-2 min-h-8 text-text-subtle hover:text-text-primary cursor-pointer text-base leading-none px-1"
        >
          &times;
        </button>
      )}
    </div>
  );
}

/** Still while the user prefers reduced motion; the text carries the progress. */
export function Spinner({ size = "h-3.5 w-3.5" }: { size?: string }) {
  return (
    <svg className={`animate-spin motion-reduce:animate-none ${size} shrink-0`} viewBox="0 0 24 24" aria-hidden="true">
      <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" />
      <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z" />
    </svg>
  );
}

function Image({ name }: { name: string }) {
  return <span className="text-text-subtle break-all">· {name}</span>;
}

/** A link-styled toggle with its panel below the banner; Escape closes it. */
function Disclosure({ label, children }: { label: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  const panelId = useId();
  return (
    <span onKeyDown={(e) => e.key === "Escape" && setOpen(false)}>
      <button
        type="button"
        aria-expanded={open}
        aria-controls={panelId}
        onClick={() => setOpen((o) => !o)}
        className={linkButton}
      >
        {label}
      </button>
      <span
        id={panelId}
        hidden={!open}
        className="absolute left-1/2 top-full z-30 mt-1 w-[min(32rem,calc(100vw-2rem))] -translate-x-1/2 rounded-md border border-surface-700 bg-surface-850 p-3 text-left font-sans text-xs leading-relaxed text-text-primary shadow-lg"
      >
        {children}
      </span>
    </span>
  );
}

export function CopyCommand({ command }: { command: string }) {
  const [feedback, setFeedback] = useState<"copied" | "failed" | null>(null);
  const codeRef = useRef<HTMLElement>(null);
  const copy = async () => {
    if (await writeClipboard(command)) {
      setFeedback("copied");
      setTimeout(() => setFeedback(null), 2000);
      return;
    }
    // Leave it selected so the user can copy it by hand.
    const range = document.createRange();
    if (codeRef.current) range.selectNodeContents(codeRef.current);
    window.getSelection()?.removeAllRanges();
    window.getSelection()?.addRange(range);
    setFeedback("failed");
  };
  return (
    <span className="inline-flex flex-wrap items-center gap-x-2">
      <code ref={codeRef} className="whitespace-pre-wrap break-all text-text-primary">
        {command}
      </code>
      <button type="button" onClick={() => void copy()} className={linkButton} aria-label={`Copy ${command}`}>
        Copy
      </button>
      {/* Callers put it inside a live region, so this is announced without a nested one. */}
      <span>
        {feedback === "copied" ? "Copied" : feedback === "failed" ? "Couldn't copy. Select and copy it." : ""}
      </span>
    </span>
  );
}

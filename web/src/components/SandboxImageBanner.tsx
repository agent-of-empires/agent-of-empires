import { useId, useRef, useState, type ReactNode } from "react";
import type { SandboxImageBannerView } from "../hooks/useSandboxImageBanner";
import { writeClipboard } from "../lib/clipboard";

const WHY =
  "Sandboxed sessions run in a container image that AoE doesn't update automatically. Newer AoE versions can need a newer image; without it, Structured view sessions stop with an adapter error.";

const TONES = {
  brand: { bar: "bg-brand-600/10 border-brand-600/30 text-brand-300", dot: "bg-brand-400" },
  done: { bar: "bg-status-running/10 border-status-running/30 text-status-running", dot: "bg-status-running" },
  failed: { bar: "bg-status-error/10 border-status-error/30 text-status-error", dot: "bg-status-error" },
};

const linkButton = "min-h-8 underline cursor-pointer hover:opacity-80";

/** Top-of-app banner for a newer sandbox image, with the pull and its outcome. */
export function SandboxImageBanner({ view }: { view: SandboxImageBannerView }) {
  switch (view.kind) {
    case "available":
      return (
        <Bar tone="brand" label="Sandbox image update available" onDismiss={view.onDismiss}>
          <span>Sandbox image update available</span>
          <Image name={view.image} />
          <button type="button" onClick={view.onUpdate} className={linkButton}>
            Update now
          </button>
          <Disclosure label="Why?">{WHY}</Disclosure>
        </Bar>
      );
    case "gated":
      return (
        <Bar tone="brand" label="Sandbox image update available" onDismiss={view.onDismiss}>
          <span>Sandbox image update available. Run on the Daemon&apos;s machine:</span>
          <CopyCommand command={view.command} />
        </Bar>
      );
    case "updating":
      return (
        <Bar tone="brand" label="Downloading sandbox image">
          <svg className="animate-spin h-3.5 w-3.5 shrink-0" viewBox="0 0 24 24" aria-hidden="true">
            <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" />
            <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z" />
          </svg>
          <span>Downloading sandbox image…</span>
          <span className="text-text-subtle">· sessions keep running</span>
        </Bar>
      );
    case "done":
      return (
        <Bar tone="done" label="Sandbox image updated" onDismiss={view.onDismiss}>
          <span>Sandbox image updated. New sessions use it; running sessions keep the image they started with.</span>
        </Bar>
      );
    case "config_unreadable":
      return (
        <Bar tone="failed" label="Couldn't check the sandbox image" onDismiss={view.onDismiss}>
          <span>
            Couldn&apos;t check the sandbox image: the Daemon can&apos;t read its configuration (config.toml or
            state.toml). Fix the file, then reload.
          </span>
        </Bar>
      );
    case "failed":
      return (
        <Bar tone="failed" label="Couldn't download the sandbox image" onDismiss={view.onDismiss}>
          <span>Couldn&apos;t download the sandbox image</span>
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

function Bar({
  tone,
  label,
  onDismiss,
  children,
}: {
  tone: keyof typeof TONES;
  label: string;
  onDismiss?: () => void;
  children: ReactNode;
}) {
  const { bar, dot } = TONES[tone];
  return (
    <div
      role="status"
      aria-label={label}
      className={`relative border-b px-4 py-2 flex flex-wrap items-center justify-center gap-x-3 gap-y-1 text-xs font-mono animate-fade-in ${onDismiss ? "pr-10 sm:pr-4" : ""} ${bar}`}
    >
      {/* On a phone the line wraps, and a lone dot or close button would take a row of its own. */}
      <span className={`hidden sm:block w-1.5 h-1.5 rounded-full shrink-0 ${dot}`} />
      {children}
      {onDismiss && (
        <button
          type="button"
          onClick={onDismiss}
          aria-label="Dismiss sandbox image notice"
          className="absolute right-2 top-1 sm:static sm:ml-2 min-h-8 text-text-muted hover:text-text-secondary cursor-pointer text-base leading-none px-1"
        >
          &times;
        </button>
      )}
    </div>
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

function CopyCommand({ command }: { command: string }) {
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
      <span role="status">
        {feedback === "copied" ? "Copied" : feedback === "failed" ? "Couldn't copy. Select and copy it." : ""}
      </span>
    </span>
  );
}

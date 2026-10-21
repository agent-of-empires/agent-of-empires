import { useEffect, useRef, useState, type ReactNode } from "react";
import { Check, Copy, X } from "lucide-react";
import type { UpdateStatus } from "../lib/api";
import { writeClipboard } from "../lib/clipboard";

const COPIED_MS = 2000;
const RESTART_COMMAND = "aoe serve --restart";

/** A command with a copy button; a failed copy selects the text so it can be copied by hand. */
function CopyCommand({ command }: { command: string }) {
  const [result, setResult] = useState<"copied" | "failed" | null>(null);
  const codeRef = useRef<HTMLElement>(null);

  useEffect(() => {
    if (result !== "copied") return;
    const timer = setTimeout(() => setResult(null), COPIED_MS);
    return () => clearTimeout(timer);
  }, [result]);

  const copy = async () => {
    if (await writeClipboard(command)) {
      setResult("copied");
      return;
    }
    const code = codeRef.current;
    const selection = window.getSelection();
    if (code && selection) {
      const range = document.createRange();
      range.selectNodeContents(code);
      selection.removeAllRanges();
      selection.addRange(range);
    }
    setResult("failed");
  };

  return (
    <span className="inline-flex max-w-full items-stretch rounded-md border border-surface-600 bg-surface-900">
      <code ref={codeRef} className="self-center px-2 py-1 font-mono text-text-primary whitespace-pre-wrap break-all">
        {command}
      </code>
      <button
        type="button"
        onClick={() => void copy()}
        aria-label={`Copy ${command}`}
        className="flex min-h-8 min-w-8 shrink-0 items-center justify-center border-l border-surface-600 px-2 text-brand-400 hover:text-brand-200 cursor-pointer"
      >
        {result === "copied" ? (
          <Check className="h-3.5 w-3.5 text-status-running" aria-hidden="true" />
        ) : result === "failed" ? (
          <X className="h-3.5 w-3.5 text-status-error" aria-hidden="true" />
        ) : (
          <Copy className="h-3.5 w-3.5" aria-hidden="true" />
        )}
      </button>
      {/* Always mounted so screen readers announce the change. */}
      <span
        role="status"
        className={`self-center ${result ? "pr-2" : ""} ${result === "failed" ? "text-status-error" : "text-status-running"}`}
      >
        {result === "copied" ? "Copied" : result === "failed" ? "Couldn't copy. Select and copy it." : ""}
      </span>
    </span>
  );
}

function restartLine(): ReactNode {
  return (
    <>
      <span>then run</span>
      <CopyCommand command={RESTART_COMMAND} />
      <span className="text-text-subtle">or restart it through your service manager</span>
    </>
  );
}

/** Top-of-app banner offering the update `useUpdateBanner` found, and how to install it. */
export function UpdateBanner({ status, onDismiss }: { status: UpdateStatus; onDismiss: () => void }) {
  const method = status.install_method ?? "unknown";
  const command = status.update_command ?? "aoe update";
  const after =
    method === "nix"
      ? "or rebuild your flake input"
      : method === "unknown"
        ? "to see how to update this install."
        : null;
  const second =
    method === "release" ? (
      <span>It offers to restart the Daemon for you when it's done.</span>
    ) : method === "unknown" ? null : (
      restartLine()
    );

  return (
    <div
      role="status"
      aria-label={`Update available: v${status.latest_version}`}
      className="bg-brand-600/10 border-b border-brand-600/30 px-4 py-2 flex items-start gap-3 text-xs text-brand-300 animate-fade-in"
    >
      <span className="mt-3.5 w-1.5 h-1.5 rounded-full bg-brand-400 shrink-0" />
      <div className="min-w-0 flex-1 flex flex-col gap-1.5">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5">
          <span>
            Update available:{" "}
            <span className="font-mono text-brand-200">
              v{status.current_version} → v{status.latest_version}
            </span>
          </span>
          <span>On the machine running AoE, run</span>
          <CopyCommand command={command} />
          {after && <span>{after}</span>}
          {status.release_url && (
            <a
              href={status.release_url}
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex min-h-8 items-center underline hover:text-brand-200"
            >
              Release notes
            </a>
          )}
        </div>
        {second && <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5">{second}</div>}
      </div>
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss update notice"
        className="flex h-8 w-8 shrink-0 items-center justify-center text-text-muted hover:text-text-secondary cursor-pointer text-base leading-none"
      >
        &times;
      </button>
    </div>
  );
}

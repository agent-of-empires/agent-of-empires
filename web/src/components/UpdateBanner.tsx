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
      <span role="status" className={`self-center text-text-primary ${result ? "pr-2" : ""}`}>
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
  // An unknown install has no command of its own: `aoe update` prints the steps for it.
  const instruction =
    method === "unknown" ? (
      <>
        <span>Run</span>
        <CopyCommand command={command} />
        <span>on the machine running AoE.</span>
      </>
    ) : (
      <>
        <span>On the machine running AoE, run</span>
        <CopyCommand command={command} />
        {method === "nix" && <span>or rebuild your flake input</span>}
      </>
    );
  // A release install's `aoe update` asks about the restart itself, and an unknown one prints its own steps.
  const second = method === "release" || method === "unknown" ? null : restartLine();

  return (
    <div
      role="status"
      aria-label={`Update available: v${status.latest_version}`}
      className="relative bg-brand-600/10 border-b border-brand-600/30 py-2 pl-4 pr-12 sm:px-12 flex justify-center text-xs text-brand-300 animate-fade-in"
    >
      {/* Centered as one block (the padding is even from sm up, × sits in it), so a second line starts under the
          first line's text; full width and left-aligned on phones. */}
      <div className="flex min-w-0 gap-3 max-sm:w-full">
        {/* As tall as a line, so the dot sits on the first line's center. */}
        <span className="flex h-8 shrink-0 items-center" aria-hidden="true">
          <span className="w-1.5 h-1.5 rounded-full bg-brand-400" />
        </span>
        <div className="min-w-0 flex flex-col gap-1.5">
          <div data-banner-line className="flex flex-wrap items-center gap-x-2 gap-y-1.5">
            <span>
              Update available:{" "}
              <span className="font-mono text-brand-200">
                v{status.current_version} → v{status.latest_version}
              </span>
            </span>
            {instruction}
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
          {second && (
            <div data-banner-line className="flex flex-wrap items-center gap-x-2 gap-y-1.5">
              {second}
            </div>
          )}
        </div>
      </div>
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss update notice"
        className="absolute right-2 top-2 flex h-8 w-8 items-center justify-center text-text-subtle hover:text-text-primary cursor-pointer text-base leading-none"
      >
        &times;
      </button>
    </div>
  );
}

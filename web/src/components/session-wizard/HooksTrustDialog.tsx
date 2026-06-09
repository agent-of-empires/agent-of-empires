import { useCallback, useEffect, useRef, useState } from "react";

interface Props {
  /** The `on_create` commands that will run once approved. */
  onCreate: string[];
  /** The `on_launch` commands the same approval trusts (run on every later
   *  session start, including TUI/CLI ones). */
  onLaunch: string[];
  /** The `on_destroy` commands the same approval trusts (run on delete). */
  onDestroy: string[];
  /** Whether the repo's `.mcp.json` also needs approval. */
  needsMcpTrust: boolean;
  onConfirm: () => Promise<void> | void;
  onCancel: () => void;
}

/**
 * Confirmation shown when the server refuses a create because the repo's
 * `on_create` hooks (and optionally its `.mcp.json`) need approval (#2066).
 * Lists the commands that will run, then resubmits with `trust_hooks: true`.
 * Mirrors the native TUI trust dialog and the CLI `--trust-hooks` prompt, and
 * shares the VolumeIgnoresGlobDialog layout.
 */
export function HooksTrustDialog({ onCreate, onLaunch, onDestroy, needsMcpTrust, onConfirm, onCancel }: Props) {
  // Approval trusts the repo's whole hooks hash, so every hook type the trust
  // covers is listed, mirroring the CLI/TUI prompts (hook_display_groups).
  const groups = [
    { name: "on_create", commands: onCreate },
    { name: "on_launch", commands: onLaunch },
    { name: "on_destroy", commands: onDestroy },
  ].filter((g) => g.commands.length > 0);
  const [confirming, setConfirming] = useState(false);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  const handleConfirm = useCallback(async () => {
    setConfirming(true);
    try {
      await onConfirm();
    } catch {
      setConfirming(false);
    }
  }, [onConfirm]);

  // Restore focus to the trigger on unmount, matching the other wizard dialogs.
  useEffect(() => {
    previousFocusRef.current = document.activeElement as HTMLElement | null;
    confirmButtonRef.current?.focus();
    return () => {
      previousFocusRef.current?.focus?.();
    };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onCancel();
        return;
      }
      if (e.key === "Enter") {
        const target = e.target as HTMLElement | null;
        if (target) {
          const tag = target.tagName;
          if (tag === "INPUT" || tag === "TEXTAREA" || tag === "BUTTON") return;
        }
        if (confirming) return;
        e.preventDefault();
        void handleConfirm();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, handleConfirm, confirming]);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="hooks-trust-dialog-title"
      data-testid="hooks-trust-dialog"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onCancel}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[460px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="px-5 py-4 border-b border-surface-700">
          <h2 id="hooks-trust-dialog-title" className="text-sm font-semibold text-status-warning">
            Trust repository hooks
          </h2>
        </div>

        {/* Body */}
        <div className="px-5 py-4 space-y-3">
          <p className="text-[13px] text-text-secondary">
            This repository defines lifecycle hooks. They run on your machine; approving trusts every hook type listed
            below, not only the ones that run now. Review the commands before approving.
          </p>

          <div className="space-y-2 max-h-40 overflow-y-auto" data-testid="hooks-trust-list">
            {groups.map((group) => (
              <div key={group.name}>
                <div className="text-[11px] font-mono text-text-dim">{group.name}:</div>
                <ul className="space-y-1">
                  {group.commands.map((cmd, i) => (
                    <li key={i} className="text-[13px] font-mono text-text-primary break-all pl-3">
                      {cmd}
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>

          {needsMcpTrust && (
            <p className="text-[12px] text-text-dim">
              The repository's <span className="font-mono text-text-secondary">.mcp.json</span> will also be trusted.
            </p>
          )}

          <p className="text-[12px] text-text-dim">
            Approving trusts this repository's hooks so future sessions (including worktrees) run them without
            prompting.
          </p>
        </div>

        {/* Footer */}
        <div className="flex justify-end gap-3 px-5 py-3 border-t border-surface-700">
          <button
            onClick={onCancel}
            disabled={confirming}
            className="px-3 py-1.5 text-sm text-text-secondary hover:text-text-primary rounded-md hover:bg-surface-700/50 cursor-pointer transition-colors disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            ref={confirmButtonRef}
            onClick={handleConfirm}
            disabled={confirming}
            data-testid="hooks-trust-proceed"
            className="px-3 py-1.5 text-sm text-surface-900 bg-green-500 hover:bg-green-600 active:bg-green-700 rounded-md cursor-pointer transition-colors disabled:opacity-50 flex items-center gap-2"
          >
            {confirming && (
              <svg className="animate-spin h-3.5 w-3.5" viewBox="0 0 24 24">
                <circle
                  className="opacity-25"
                  cx="12"
                  cy="12"
                  r="10"
                  stroke="currentColor"
                  strokeWidth="4"
                  fill="none"
                />
                <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
              </svg>
            )}
            {confirming ? "Creating..." : "Trust and create"}
          </button>
        </div>
      </div>
    </div>
  );
}

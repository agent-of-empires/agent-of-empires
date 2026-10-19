import { useEffect, useRef, useState } from "react";
import { setSessionAutoApprove } from "../lib/api";
import { reportError, reportInfo } from "../lib/toastBus";
import { useServerDown } from "../lib/connectionState";

interface Props {
  sessionId: string;
  sessionTitle: string;
  onClose: () => void;
}

export function AutoApproveDialog({ sessionId, sessionTitle, onClose }: Props) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const offline = useServerDown();
  const cancelRef = useRef<HTMLButtonElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const previousFocus = document.activeElement as HTMLElement | null;
    cancelRef.current?.focus();
    return () => previousFocus?.focus?.();
  }, []);

  const apply = async (enabled: boolean) => {
    if (pending || offline) return;
    dialogRef.current?.focus();
    setPending(true);
    setError(null);
    try {
      const result = await setSessionAutoApprove(sessionId, enabled);
      reportInfo(
        `Auto-approve ${result.enabled ? "on" : "off"}${
          result.restarted ? "; restarted and resumed the conversation" : ""
        }`,
      );
      onClose();
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      setError(message);
      cancelRef.current?.focus();
      reportError(message);
    } finally {
      setPending(false);
    }
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="auto-approve-title"
      aria-describedby="auto-approve-description"
      data-testid="auto-approve-dialog"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={() => {
        if (!pending) onClose();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation();
          if (!pending) onClose();
        } else if (e.key === "Tab") {
          const buttons = dialogRef.current?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)");
          if (!buttons?.length) {
            e.preventDefault();
            return;
          }
          const first = buttons[0];
          const last = buttons[buttons.length - 1];
          if (!first || !last) return;
          if (e.shiftKey && document.activeElement === dialogRef.current) {
            e.preventDefault();
            last.focus();
          } else if (e.shiftKey && document.activeElement === first) {
            e.preventDefault();
            last.focus();
          } else if (!e.shiftKey && document.activeElement === dialogRef.current) {
            e.preventDefault();
            first.focus();
          } else if (!e.shiftKey && document.activeElement === last) {
            e.preventDefault();
            first.focus();
          }
        }
      }}
    >
      <div
        ref={dialogRef}
        tabIndex={-1}
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[440px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-surface-700">
          <h2 id="auto-approve-title" className="text-sm font-semibold text-text-primary">
            Auto-approve
          </h2>
        </div>
        <div id="auto-approve-description" className="px-5 py-4 space-y-3 text-[13px] text-text-secondary">
          <p>
            <span className="font-mono text-text-primary">{sessionTitle}</span>: choose whether the agent may run tools
            without asking for approval.
          </p>
          <p>
            When a live change isn't supported, AoE may restart and resume this conversation, interrupting the current
            turn. Unsupported agents are rejected without changing the setting.
          </p>
          <p>Codex applies changes on its next turn. Plan mode does not imply auto-approve.</p>
          {error && (
            <p role="alert" className="text-status-error">
              {error}
            </p>
          )}
        </div>
        <div className="flex flex-wrap justify-end gap-3 px-5 py-3 border-t border-surface-700">
          <button
            ref={cancelRef}
            onClick={onClose}
            disabled={pending}
            className="px-3 py-1.5 text-sm text-text-secondary rounded-md hover:bg-surface-700/50 cursor-pointer disabled:opacity-50"
          >
            Cancel
          </button>
          {[false, true].map((enabled) => (
            <button
              key={String(enabled)}
              onClick={() => void apply(enabled)}
              disabled={pending || offline}
              className="px-3 py-1.5 text-sm text-text-primary border border-surface-700 rounded-md hover:bg-surface-700/50 cursor-pointer disabled:opacity-50"
            >
              Turn {enabled ? "on" : "off"} auto-approve
            </button>
          ))}
        </div>
        {pending && (
          <p role="status" className="px-5 pb-3 text-xs text-text-dim">
            Changing approval policy…
          </p>
        )}
      </div>
    </div>
  );
}

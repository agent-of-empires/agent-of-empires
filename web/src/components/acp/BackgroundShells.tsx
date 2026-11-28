// A backgrounded shell command's row, and its command and output.

import { useEffect, useRef, useState } from "react";
import { Maximize2, Terminal } from "lucide-react";

import { useFocusTrap } from "../../hooks/useFocusTrap";
import { fetchTaskOutput, type TaskOutput } from "../../lib/api";
import type { BackgroundShell } from "../../lib/acpTypes";
import { BACKDROP, DIALOG } from "../../lib/surfaces";
import { BUTTON_GHOST_SM, CAPTION_INLINE, CODE_BLOCK } from "../../lib/surfaces";
import { CopyMarkdownButton } from "./CopyMarkdownButton";
import { CloseButton } from "../Dialog";

const OUTPUT_POLL_MS = 2000;
// Failed reads retried after the shell ended, before the modal gives up.
const MAX_RETRIES_AFTER_FINISH = 5;

export function ShellRow({ shell, onOpen }: { shell: BackgroundShell; onOpen: (taskId: string) => void }) {
  return (
    <button
      type="button"
      onClick={() => onOpen(shell.taskId)}
      title="Open command and output"
      className="flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-surface-800"
    >
      <span
        className={`h-2 w-2 shrink-0 rounded-full ${shell.finished ? "bg-text-dim/60" : "bg-status-waiting animate-pulse"}`}
      />
      <Terminal className="h-3.5 w-3.5 shrink-0 text-text-dim" />
      <span className="min-w-0 flex-1 truncate font-mono text-xs text-text-secondary">
        {shell.command || shell.taskId}
      </span>
      <span className="shrink-0 text-[11px] text-text-dim">{shell.finished ? "done" : "running"}</span>
      <Maximize2 className="h-3.5 w-3.5 shrink-0 text-text-dim" />
    </button>
  );
}

export function ShellDetailModal({
  sessionId,
  shell,
  onClose,
}: {
  sessionId: string;
  shell: BackgroundShell;
  onClose: () => void;
}) {
  const [out, setOut] = useState<TaskOutput | undefined>(undefined);
  const [failed, setFailed] = useState(false);
  const [wrap, setWrap] = useState(true);
  const preRef = useRef<HTMLPreElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);

  // Focus moves into the dialog, Tab stays inside it, and closing returns focus.
  useFocusTrap(() => panelRef.current, { restoreFocus: true });
  useEffect(() => {
    panelRef.current?.focus();
  }, []);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Poll while the shell runs, and until one read lands after it ends.
  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let retries = 0;
    const read = async () => {
      const next = await fetchTaskOutput(sessionId, shell.taskId);
      if (cancelled) return;
      if (next) setOut(next);
      setFailed(!next);
      if (!next && shell.finished && ++retries > MAX_RETRIES_AFTER_FINISH) return;
      if (!next || !shell.finished) timer = setTimeout(read, OUTPUT_POLL_MS);
    };
    void read();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [sessionId, shell.taskId, shell.finished]);

  // Follow the end of the output while the reader stays at the bottom.
  const pinned = useRef(true);
  useEffect(() => {
    const el = preRef.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  }, [out?.output]);

  return (
    <div
      className={`fixed inset-0 z-50 flex items-center justify-center ${BACKDROP} animate-fade-in`}
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label="Background shell"
    >
      <div
        ref={panelRef}
        tabIndex={-1}
        className={`flex max-h-[85vh] w-[680px] max-w-[92vw] flex-col ${DIALOG}`}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-2 border-b border-surface-700/50 px-4 py-3">
          <Terminal className="h-4 w-4 shrink-0 text-text-dim" />
          <span className="min-w-0 flex-1 truncate text-sm font-semibold text-text-bright">Background shell</span>
          <span className="shrink-0 text-[11px] text-text-dim">{shell.finished ? "done" : "running"}</span>
          <CloseButton onClick={onClose} />
        </div>
        <div className="flex min-h-0 flex-col gap-3 overflow-y-auto px-4 py-3 text-[12px]">
          <div className="flex flex-col gap-0.5">
            <div className="flex items-center justify-between">
              <span className={CAPTION_INLINE}>command</span>
              {shell.command && <CopyMarkdownButton compact source={shell.command} label="Copy command" />}
            </div>
            <pre className="whitespace-pre-wrap break-words font-mono text-text-secondary">
              {shell.command || "(command unavailable)"}
            </pre>
          </div>
          {shell.startedAt && (
            <div className="text-[11px] text-text-dim">started {new Date(shell.startedAt).toLocaleString()}</div>
          )}
          <div className="flex flex-col gap-0.5">
            <div className="flex items-center justify-between">
              <span className={CAPTION_INLINE}>output</span>
              <button type="button" onClick={() => setWrap((v) => !v)} aria-pressed={wrap} className={BUTTON_GHOST_SM}>
                Wrap
              </button>
            </div>
            {out === undefined ? (
              <span className="text-text-dim">{failed ? "Could not load output. Retrying…" : "Loading output…"}</span>
            ) : !out.exists ? (
              <span className="text-text-dim">Output not available.</span>
            ) : (
              <>
                {out.truncated && <span className="text-text-dim">Showing the end of a long output.</span>}
                <pre
                  ref={preRef}
                  tabIndex={0}
                  role="region"
                  aria-label="Shell output"
                  onScroll={(e) => {
                    const el = e.currentTarget;
                    pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 8;
                  }}
                  className={`${CODE_BLOCK} max-h-[50vh] ${wrap ? "whitespace-pre-wrap break-words" : "whitespace-pre"}`}
                >
                  {out.output || "(no output yet)"}
                </pre>
              </>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

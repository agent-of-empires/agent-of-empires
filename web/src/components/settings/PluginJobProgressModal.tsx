import { useEffect, useRef, useState } from "react";

import { fetchPluginJob, type PluginJob } from "../../lib/api";
import { BUTTON_PRIMARY } from "../../lib/surfaces";
import { PluginModal } from "./PluginModal";

interface PluginJobProgressModalProps {
  jobId: string;
  title: string;
  /** Closing mid-run only stops polling; the job keeps running. */
  onClose: () => void;
}

/// Polls a plugin lifecycle job and its log tail until it finishes.
export function PluginJobProgressModal({ jobId, title, onClose }: PluginJobProgressModalProps) {
  const [job, setJob] = useState<PluginJob | null>(null);
  const [error, setError] = useState<string | null>(null);
  // A 404 (e.g. daemon restart) is terminal, so Close is not stuck disabled.
  const [gone, setGone] = useState(false);
  const logRef = useRef<HTMLPreElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      const res = await fetchPluginJob(jobId);
      if (cancelled) return;
      if (res.kind === "ok") {
        setJob(res.job);
        setError(null);
        if (res.job.job.status.state === "running") {
          timer = setTimeout(() => void poll(), 1000);
        }
      } else if (res.status === 404) {
        setGone(true);
        setError(res.message);
      } else {
        setError(res.message);
        timer = setTimeout(() => void poll(), 2000);
      }
    };
    void poll();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [jobId]);

  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [job?.log.tail]);

  const state = job?.job.status.state ?? "running";
  const done = state === "succeeded" || state === "failed" || gone;
  const failedError = job?.job.status.state === "failed" ? job.job.status.error : null;

  useEffect(() => {
    if (!done) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const dialog = e
        .composedPath()
        .find((node) => node instanceof HTMLElement && node.getAttribute("aria-modal") === "true");
      if (dialog && dialog !== dialogRef.current) return;
      onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [done, onClose]);

  return (
    <PluginModal
      testId="plugin-job-modal"
      ariaLabel={title}
      rootRef={dialogRef}
      closeTestId="plugin-job-close"
      closeLabel={done ? "Close" : "Running…"}
      busy={!done}
      closeOnEscape={false}
      onClose={onClose}
      panelClassName="md:w-[42rem] md:max-h-[80vh]"
      header={
        <>
          <h2 className="text-[16px] md:text-[15px] font-semibold text-text-primary">{title}</h2>
          <p className="text-xs text-text-dim" data-testid="plugin-job-status">
            {gone ? "Job no longer available." : state === "running" && "Running…"}
            {!gone && state === "succeeded" && "Done."}
            {!gone && state === "failed" && "Failed."}
          </p>
        </>
      }
      actions={
        done && (
          <button type="button" className={BUTTON_PRIMARY} onClick={onClose} data-testid="plugin-job-done">
            Close
          </button>
        )
      }
    >
      {failedError && (
        <p className="mb-2 text-xs text-status-error" data-testid="plugin-job-error">
          {failedError}
        </p>
      )}
      {error && !done && <p className="mb-2 text-xs text-text-dim">Reconnecting… ({error})</p>}

      <pre
        ref={logRef}
        className="max-h-[50vh] overflow-auto whitespace-pre-wrap break-words rounded-lg border border-surface-700/60 bg-surface-950 p-2 font-mono text-[11px] text-text-dim"
        data-testid="plugin-job-log"
      >
        {job?.log.tail || (state === "running" ? "Starting…" : "")}
      </pre>
      {job?.log.path && (
        <p className="mt-2 break-all font-mono text-xs text-text-dim" data-testid="plugin-job-host-path">
          Full log on Daemon host: {job.log.path}
        </p>
      )}
    </PluginModal>
  );
}

import { useEffect, useRef, useState } from "react";

import { fetchPluginJob, type PluginJob } from "../../lib/api";

interface PluginJobProgressModalProps {
  /** The lifecycle job to follow. */
  jobId: string;
  /** Header line, e.g. "Installing acme.widget". */
  title: string;
  /** Close the modal. The caller refreshes the plugin list. Closing mid-run
   *  only stops polling; the host-side job keeps running. */
  onClose: () => void;
}

/// Live progress for a host-side plugin lifecycle job (install / update /
/// uninstall). Polls the job status + log tail once a second until the job
/// reaches a terminal state, rendering the verbatim host output so a
/// dashboard-only user can watch fetch / build / remove work and see the final
/// success or failure without a terminal.
export function PluginJobProgressModal({ jobId, title, onClose }: PluginJobProgressModalProps) {
  const [job, setJob] = useState<PluginJob | null>(null);
  const [error, setError] = useState<string | null>(null);
  // The job is gone (404), e.g. the daemon restarted mid-run. Terminal: stop
  // polling and let the user close, rather than spinning forever.
  const [gone, setGone] = useState(false);
  const logRef = useRef<HTMLPreElement>(null);

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
        // The job no longer exists; treat it as terminal instead of retrying.
        setGone(true);
        setError(res.message);
      } else {
        // A transient read failure should not kill the follow; retry slower.
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

  // Keep the newest output in view as the tail grows.
  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [job?.log.tail]);

  const state = job?.job.status.state ?? "running";
  const done = state === "succeeded" || state === "failed" || gone;
  const failedError = job?.job.status.state === "failed" ? job.job.status.error : null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      data-testid="plugin-job-modal"
    >
      <div className="max-h-[80vh] w-full max-w-2xl overflow-auto rounded border border-surface-700 bg-surface-900 p-4 text-sm">
        <div className="mb-3 flex items-start justify-between gap-3">
          <div>
            <h2 className="font-semibold">{title}</h2>
            <p className="text-xs text-text-dim" data-testid="plugin-job-status">
              {gone ? "Job no longer available." : state === "running" && "Running…"}
              {!gone && state === "succeeded" && "Done."}
              {!gone && state === "failed" && "Failed."}
            </p>
          </div>
          <button
            type="button"
            className="rounded border border-surface-700 px-2 py-0.5 text-xs hover:bg-surface-800 disabled:opacity-50"
            disabled={!done}
            onClick={onClose}
            data-testid="plugin-job-close"
          >
            {done ? "Close" : "Running…"}
          </button>
        </div>

        {failedError && (
          <p className="mb-2 text-xs text-status-error" data-testid="plugin-job-error">
            {failedError}
          </p>
        )}
        {error && !done && <p className="mb-2 text-xs text-text-dim">Reconnecting… ({error})</p>}

        <pre
          ref={logRef}
          className="max-h-[50vh] overflow-auto whitespace-pre-wrap break-words rounded border border-surface-700 bg-surface-950 p-2 font-mono text-[11px] text-text-dim"
          data-testid="plugin-job-log"
        >
          {job?.log.tail || (state === "running" ? "Starting…" : "")}
        </pre>

        {done && (
          <div className="mt-3 flex justify-end">
            <button
              type="button"
              className="rounded bg-brand-600 px-3 py-1 text-xs font-medium text-white hover:bg-brand-500"
              onClick={onClose}
              data-testid="plugin-job-done"
            >
              Close
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

import { useCallback, useEffect, useRef, useState } from "react";
import { getSessionDiffFiles, reportTelemetrySeen } from "../lib/api";
import type { RepoBase, RichDiffFile } from "../lib/types";

const POLL_INTERVAL = 10_000;

interface UseDiffFilesResult {
  files: RichDiffFile[];
  /** One entry per repo whose diff was computed. Single-repo sessions
   *  get a one-element array; workspace sessions get one entry per
   *  workspace member with each repo's default branch. See #1047. */
  perRepoBases: RepoBase[];
  warning: string | null;
  loading: boolean;
  /** Monotonically increasing revision counter; bumps when the file list changes. */
  revision: number;
  refresh: () => void;
}

export function useDiffFiles(
  sessionId: string | null,
  enabled: boolean,
): UseDiffFilesResult {
  const [files, setFiles] = useState<RichDiffFile[]>([]);
  const [perRepoBases, setPerRepoBases] = useState<RepoBase[]>([
    { base_branch: "main" },
  ]);
  const [warning, setWarning] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [revision, setRevision] = useState(0);
  const lastFingerprintRef = useRef("");
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const requestIdRef = useRef(0);
  // Session the `diff_panel` usage signal last fired for, so opening the panel
  // reports once per session rather than on every 10s poll tick or re-render.
  const diffPanelSeenForRef = useRef<string | null>(null);
  // Mirrors `enabled` so fetchFiles can read the current panel state without
  // taking it as a dep (which would tear down and re-run the fetch effects).
  const enabledRef = useRef(enabled);

  const fetchFiles = useCallback(async () => {
    if (!sessionId) return;
    const reqId = ++requestIdRef.current;
    const capturedSessionId = sessionId;
    const resp = await getSessionDiffFiles(capturedSessionId);
    // Drop stale responses: another fetch started, or session changed mid-flight
    if (reqId !== requestIdRef.current || capturedSessionId !== sessionId)
      return;
    if (resp) {
      const fingerprint = JSON.stringify(resp.files);
      if (fingerprint !== lastFingerprintRef.current) {
        lastFingerprintRef.current = fingerprint;
        setFiles(resp.files);
        setPerRepoBases(resp.per_repo_bases);
        setWarning(resp.warning ?? null);
        setRevision((r) => r + 1);
      }
      // The diff list loaded successfully: report diff_panel once per session.
      // Gated on enabledRef so a background fetch fired on session change while
      // the panel is closed does not count, and on the per-session ref so the
      // 10s poll does not re-fire it.
      if (
        enabledRef.current &&
        diffPanelSeenForRef.current !== capturedSessionId
      ) {
        diffPanelSeenForRef.current = capturedSessionId;
        reportTelemetrySeen("diff_panel");
      }
    }
    setLoading(false);
  }, [sessionId]);

  // Keep enabledRef in sync with the latest panel state.
  useEffect(() => {
    enabledRef.current = enabled;
  }, [enabled]);

  // Reset state when sessionId changes (render-time, avoids effect-based setState)
  const [trackedSessionId, setTrackedSessionId] = useState(sessionId);
  if (sessionId !== trackedSessionId) {
    setTrackedSessionId(sessionId);
    if (sessionId === null) {
      setFiles([]);
      setLoading(false);
      setRevision(0);
    } else {
      setLoading(true);
    }
  }

  // Fetch on session change; invalidate any in-flight requests.
  useEffect(() => {
    requestIdRef.current += 1;
    lastFingerprintRef.current = "";
    if (!sessionId) return;
    const timer = setTimeout(() => {
      void fetchFiles();
    }, 0);
    return () => clearTimeout(timer);
  }, [sessionId, fetchFiles]);

  // Poll when enabled
  useEffect(() => {
    if (intervalRef.current) {
      clearInterval(intervalRef.current);
      intervalRef.current = null;
    }
    if (enabled && sessionId) {
      intervalRef.current = setInterval(() => void fetchFiles(), POLL_INTERVAL);
    }
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [enabled, sessionId, fetchFiles]);

  return {
    files,
    perRepoBases,
    warning,
    loading,
    revision,
    refresh: fetchFiles,
  };
}

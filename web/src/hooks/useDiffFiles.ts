import { useCallback, useEffect, useRef, useState } from "react";
import { getSessionDiffFiles, reportTelemetrySeen } from "../lib/api";
import type { DiffComparison, RepoBase, RichDiffFile } from "../lib/types";
import { useLatestRef } from "./useLatestRef";

const POLL_INTERVAL = 10_000;

interface UseDiffFilesResult {
  files: RichDiffFile[];
  perRepoBases: RepoBase[];
  warning: string | null;
  loading: boolean;
  revision: number;
  refresh: () => void;
}

export function useDiffFiles(
  sessionId: string | null,
  enabled: boolean,
  comparison: DiffComparison = "base",
): UseDiffFilesResult {
  const [files, setFiles] = useState<RichDiffFile[]>([]);
  const [perRepoBases, setPerRepoBases] = useState<RepoBase[]>([{ base_branch: "main", repo_path: "" }]);
  const [warning, setWarning] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [revision, setRevision] = useState(0);
  const lastFingerprintRef = useRef("");
  const requestIdRef = useRef(0);
  const diffPanelSeenForRef = useRef<string | null>(null);
  const enabledRef = useLatestRef(enabled);

  const fetchFiles = useCallback(async () => {
    if (!sessionId) return;
    const reqId = ++requestIdRef.current;
    const capturedSessionId = sessionId;
    const capturedComparison = comparison;
    const resp = await getSessionDiffFiles(capturedSessionId, capturedComparison);
    // Include bases and warning: a base change can leave the file list identical (#3329).
    // Drop stale responses: another fetch started, or session/comparison changed mid-flight.
    if (reqId !== requestIdRef.current || capturedSessionId !== sessionId || capturedComparison !== comparison) return;
    if (resp) {
      const fingerprint = JSON.stringify({
        files: resp.files,
        per_repo_bases: resp.per_repo_bases,
        warning: resp.warning ?? null,
      });
      if (fingerprint !== lastFingerprintRef.current) {
        lastFingerprintRef.current = fingerprint;
        setFiles(resp.files);
        setPerRepoBases(resp.per_repo_bases);
        setWarning(resp.warning ?? null);
        setRevision((r) => r + 1);
      }
      if (enabledRef.current && diffPanelSeenForRef.current !== capturedSessionId) {
        diffPanelSeenForRef.current = capturedSessionId;
        reportTelemetrySeen("diff_panel");
      }
    }
    setLoading(false);
  }, [sessionId, comparison, enabledRef]);

  const [trackedSessionId, setTrackedSessionId] = useState(sessionId);
  const [trackedComparison, setTrackedComparison] = useState(comparison);
  if (sessionId !== trackedSessionId || comparison !== trackedComparison) {
    setTrackedSessionId(sessionId);
    setTrackedComparison(comparison);
    if (sessionId === null || sessionId !== trackedSessionId) {
      setFiles([]);
      setPerRepoBases([{ base_branch: "main", repo_path: "" }]);
      setWarning(null);
      setRevision(0);
      setLoading(sessionId !== null);
    } else {
      setFiles([]);
      setPerRepoBases([{ base_branch: "main", repo_path: "" }]);
      setWarning(null);
      setLoading(true);
    }
  }

  // Fetch on session/comparison change; invalidate any in-flight requests.
  useEffect(() => {
    requestIdRef.current += 1;
    lastFingerprintRef.current = "";
    if (!sessionId) return;
    const timer = setTimeout(() => {
      void fetchFiles();
    }, 0);
    return () => clearTimeout(timer);
  }, [sessionId, comparison, fetchFiles]);

  useEffect(() => {
    if (!enabled || !sessionId) return;
    const id = setInterval(() => void fetchFiles(), POLL_INTERVAL);
    return () => clearInterval(id);
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

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { getSessionDiffFiles, reportTelemetrySeen } from "../lib/api";
import type { DiffScope } from "../lib/diffScope";
import type { RepoBase, RichDiffFile } from "../lib/types";
import { useLatestRef } from "./useLatestRef";

const POLL_INTERVAL = 10_000;
const DEFAULT_BASES: RepoBase[] = [{ base_branch: "main", repo_path: "" }];

interface UseDiffFilesResult {
  files: RichDiffFile[];
  perRepoBases: RepoBase[];
  warning: string | null;
  loading: boolean;
  revision: number;
  refresh: () => void;
  /** The scope the files were computed for: Branch while a requested Last turn has no snapshot. */
  scope: DiffScope;
  lastTurnAt: string | null;
}

export function useDiffFiles(
  sessionId: string | null,
  enabled: boolean,
  requestedScope: DiffScope = "branch",
): UseDiffFilesResult {
  const [files, setFiles] = useState<RichDiffFile[]>([]);
  const [scope, setScope] = useState<DiffScope>(requestedScope);
  const [lastTurnAt, setLastTurnAt] = useState<string | null>(null);
  const [perRepoBases, setPerRepoBases] = useState<RepoBase[]>(DEFAULT_BASES);
  const [warning, setWarning] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [revision, setRevision] = useState(0);
  const lastFingerprintRef = useRef("");
  const requestIdRef = useRef(0);
  const currentSessionIdRef = useRef(sessionId);
  const diffPanelSeenForRef = useRef<string | null>(null);
  const enabledRef = useLatestRef(enabled);

  const fetchFiles = useCallback(async () => {
    if (!sessionId) return;
    const capturedSessionId = sessionId;
    if (capturedSessionId !== currentSessionIdRef.current) return;
    const reqId = ++requestIdRef.current;
    let shown = requestedScope;
    let resp = await getSessionDiffFiles(capturedSessionId, shown);
    // Without a turn snapshot the server refuses Last turn; show Branch until a turn starts.
    if (!resp && shown === "last_turn") {
      shown = "branch";
      resp = await getSessionDiffFiles(capturedSessionId, shown);
    }
    if (reqId !== requestIdRef.current || capturedSessionId !== currentSessionIdRef.current) return;
    if (resp) {
      // Include bases and warning: a base change can leave the file list identical (#3329).
      const fingerprint = JSON.stringify({
        files: resp.files,
        per_repo_bases: resp.per_repo_bases,
        warning: resp.warning ?? null,
        scope: shown,
        last_turn_at: resp.last_turn_at ?? null,
      });
      if (fingerprint !== lastFingerprintRef.current) {
        lastFingerprintRef.current = fingerprint;
        setScope(shown);
        setLastTurnAt(resp.last_turn_at ?? null);
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
  }, [sessionId, enabledRef, requestedScope]);

  const [trackedSessionId, setTrackedSessionId] = useState(sessionId);
  if (sessionId !== trackedSessionId) {
    setTrackedSessionId(sessionId);
    setFiles([]);
    setPerRepoBases(DEFAULT_BASES);
    setWarning(null);
    setLastTurnAt(null);
    if (sessionId === null) {
      setLoading(false);
      setRevision(0);
    } else {
      setLoading(true);
    }
  }

  // Layout phase: drop in-flight responses before they can land on the new session.
  useLayoutEffect(() => {
    currentSessionIdRef.current = sessionId;
    requestIdRef.current += 1;
    lastFingerprintRef.current = "";
  }, [sessionId]);

  useEffect(() => {
    if (!sessionId) return;
    const timer = setTimeout(() => {
      void fetchFiles();
    }, 0);
    return () => clearTimeout(timer);
  }, [sessionId, fetchFiles]);

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
    scope,
    lastTurnAt,
  };
}

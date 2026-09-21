import { useCallback, useEffect, useMemo, useState } from "react";

/** Prefix matches, then label substrings, then description substrings; shorter labels first. */
export function fuzzyFilter<T extends { label: string; description?: string }>(
  items: T[],
  query: string,
  cap = 30,
): T[] {
  const q = query.toLowerCase();
  if (!q) return items.slice(0, cap);
  return items
    .map((it) => {
      const label = it.label.toLowerCase();
      const hint = it.description?.toLowerCase() ?? "";
      if (label.startsWith(q)) return { it, score: 0 };
      if (label.includes(q)) return { it, score: 1 };
      if (hint.includes(q)) return { it, score: 2 };
      return { it, score: 99 };
    })
    .filter((x) => x.score < 99)
    .sort((a, b) => a.score - b.score || a.it.label.length - b.it.label.length)
    .slice(0, cap)
    .map((x) => x.it);
}

/** The session's workspace file list, fetched once per session. */
export function useFilesIndex(sessionId: string): {
  files: string[];
  loading: boolean;
  /** The last fetch failed, as opposed to an empty list. */
  error: boolean;
  reload: () => void;
} {
  const [files, setFiles] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const reload = useCallback(() => setAttempt((n) => n + 1), []);
  const [trackedSessionId, setTrackedSessionId] = useState(sessionId);
  if (sessionId !== trackedSessionId) {
    setTrackedSessionId(sessionId);
    setLoading(true);
  }
  useEffect(() => {
    let cancelled = false;
    fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/files`)
      .then((r) => {
        if (!r.ok) throw new Error(`files list failed: ${r.status}`);
        return r.json();
      })
      .then((data: { files?: string[] }) => {
        if (cancelled) return;
        setFiles(data.files ?? []);
        setError(false);
      })
      .catch(() => {
        if (cancelled) return;
        setFiles([]);
        setError(true);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, attempt]);
  return useMemo(() => ({ files, loading, error, reload }), [files, loading, error, reload]);
}

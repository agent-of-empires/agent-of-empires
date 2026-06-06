// Helpers for the @-mention file picker. The hook fetches the
// session's workspace file list once on mount and memoizes it; the
// fuzzyFilter helper is used by both the file adapter and any other
// place that wants prefix-then-substring ordering.

import { useEffect, useMemo, useState } from "react";

/** Lightweight fuzzy filter: prefer prefix matches, then substring. */
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

/**
 * Subscribe to the workspace file index once per sessionId. Backed by
 * `GET /api/sessions/:id/acp/files` which walks the session's
 * project_path tree (capped at 5k entries).
 */
export function useFilesIndex(sessionId: string): {
  files: string[];
  loading: boolean;
} {
  const [files, setFiles] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  // Render-time: reset loading when sessionId changes
  const [trackedSessionId, setTrackedSessionId] = useState(sessionId);
  if (sessionId !== trackedSessionId) {
    setTrackedSessionId(sessionId);
    setLoading(true);
  }
  useEffect(() => {
    let cancelled = false;
    // loading is set to true in render-time above when sessionId changes
    fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/files`)
      .then((r) => (r.ok ? r.json() : { files: [] }))
      .then((data: { files?: string[] }) => {
        if (cancelled) return;
        setFiles(data.files ?? []);
      })
      .catch(() => {
        if (!cancelled) setFiles([]);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId]);
  return useMemo(() => ({ files, loading }), [files, loading]);
}

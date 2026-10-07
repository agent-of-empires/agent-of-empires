import { useEffect, useState } from "react";
import { fetchBranches, type BranchInfo } from "../../../lib/api";

/**
 * Loads a repo's branches the first time the field opens and filters them by
 * `query`. Closing drops the list, so reopening picks up branches created since.
 */
export function useBranchSuggestions(repoPath: string, open: boolean, query: string, limit: number) {
  const [branches, setBranches] = useState<BranchInfo[] | null>(null);
  const loadKey = open ? repoPath : null;
  const [trackedKey, setTrackedKey] = useState(loadKey);
  if (loadKey !== trackedKey) {
    setTrackedKey(loadKey);
    setBranches(null);
  }

  useEffect(() => {
    if (!open || !repoPath) return;
    let cancelled = false;
    fetchBranches(repoPath, true).then((rows) => {
      if (!cancelled) setBranches(rows ?? []);
    });
    return () => {
      cancelled = true;
    };
  }, [open, repoPath]);

  const q = query.trim().toLowerCase();
  return {
    loading: open && branches === null,
    suggestions: (branches ?? []).filter((b) => !q || b.name.toLowerCase().includes(q)).slice(0, limit),
  };
}

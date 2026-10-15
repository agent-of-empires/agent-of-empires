import type { RichDiffFile } from "./types";

/** A plain diff-list pick whose path and repo left the diff files. Selections opened from transcript links are exempt. */
export function diffSelectionStale(
  selectedFile: { path: string; repoName?: string; cited?: boolean } | null,
  diffFilesLoading: boolean,
  diffFiles: RichDiffFile[],
): boolean {
  if (!selectedFile || selectedFile.cited || diffFilesLoading) return false;
  return !diffFiles.some(
    (f) => f.path === selectedFile.path && (f.repo_name ?? undefined) === (selectedFile.repoName ?? undefined),
  );
}

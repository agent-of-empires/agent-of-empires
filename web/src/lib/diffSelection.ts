import type { RichDiffFile } from "./types";

/**
 * Whether a diff-list file selection has gone stale and should be cleared.
 *
 * A selection is stale when it is a plain diff-list pick (not opened from a
 * transcript link) whose path is no longer present in the current diff files.
 * Cited files (opened from a `path:line` transcript link) are exempt: they may
 * have no diff against the base and stay viewable via the full-file fallback
 * (#1810). Path and repo are matched together so a same-path file in another
 * workspace repo can't keep a selection alive.
 */
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

import { extractSnippetFromContents } from "./extractSnippetFromContents";
import type { AnchoredComment, DiffComment } from "./types";

/** A comment is active while its range still fits its side of the file, stale otherwise. */
export function anchorCommentsToContents(
  comments: DiffComment[],
  filePath: string,
  repoName: string | undefined,
  oldContent: string,
  newContent: string,
): AnchoredComment[] {
  return comments
    .filter((c) => c.filePath === filePath && (c.repoName ?? undefined) === (repoName ?? undefined))
    .map((c) => ({
      comment: c,
      status:
        extractSnippetFromContents(oldContent, newContent, c.side, c.startLine, c.endLine) == null ? "stale" : "active",
    }));
}

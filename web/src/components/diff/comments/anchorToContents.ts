import { extractSnippetFromContents } from "./extractSnippetFromContents";
import type { AnchoredComment, DiffComment } from "./types";

/**
 * Anchor comments against the raw old/new contents fed to the
 * contents-based (`@pierre/diffs`) renderer.
 *
 * Unlike the hunk-based {@link anchorComments}, any line that exists in the
 * current file can host an annotation, so a comment is `active` whenever its
 * range is in-bounds for its side and `stale` only when the range no longer
 * fits the file. `hunkIndex`/`endRowIndex` are not used on this path; the
 * renderer places the annotation from `comment.side`/`comment.endLine`.
 */
export function anchorCommentsToContents(
  comments: DiffComment[],
  filePath: string,
  repoName: string | undefined,
  oldContent: string,
  newContent: string,
): AnchoredComment[] {
  return comments
    .filter((c) => c.filePath === filePath && (c.repoName ?? undefined) === (repoName ?? undefined))
    .map((c) => {
      const snippet = extractSnippetFromContents(oldContent, newContent, c.side, c.startLine, c.endLine);
      if (snippet == null) {
        return { comment: c, status: "stale", contentChanged: false };
      }
      return {
        comment: c,
        status: "active",
        contentChanged: snippet !== c.capturedSnippet,
      };
    });
}

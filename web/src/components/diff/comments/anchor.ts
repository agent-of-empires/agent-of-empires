import type { RichDiffHunk } from "../../../lib/types";
import { extractSnippetFromHunks } from "./extractSnippet";
import type { AnchoredComment, DiffComment } from "./types";

/** Map every comment for a given (repoName, filePath) against the
 *  currently loaded hunks. A comment whose line range no longer maps
 *  to any hunk row of the matching side becomes `stale`; the prompt
 *  still includes it (via `comment.capturedSnippet`) but the inline
 *  card moves to the file-level stale block.
 *
 *  `contentChanged` is computed here so a future UI can render a
 *  separate `[changed]` chip without touching the anchor logic. v1 UI
 *  only branches on `status`. */
export function anchorComments(
  comments: DiffComment[],
  filePath: string,
  repoName: string | undefined,
  hunks: RichDiffHunk[],
): AnchoredComment[] {
  return comments
    .filter((c) => c.filePath === filePath && (c.repoName ?? undefined) === (repoName ?? undefined))
    .map((c) => anchorOne(c, hunks));
}

function anchorOne(comment: DiffComment, hunks: RichDiffHunk[]): AnchoredComment {
  const found = extractSnippetFromHunks(hunks, comment.side, comment.startLine, comment.endLine);
  if (!found) {
    return { comment, status: "stale", contentChanged: false };
  }
  return {
    comment,
    status: "active",
    contentChanged: found.snippet !== comment.capturedSnippet,
    hunkIndex: found.hunkIndex,
    endRowIndex: found.endRowIndex,
  };
}

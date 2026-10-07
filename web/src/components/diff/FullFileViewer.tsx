import { useMemo } from "react";
import { File, Virtualizer } from "@pierre/diffs/react";
import type { FileContents, FileOptions } from "@pierre/diffs";
import { useShikiTheme } from "../../hooks/useShikiTheme";
import { DiffWorkerPoolProvider } from "./pierre/DiffWorkerPoolProvider";

interface Props {
  /** Full file text to render. */
  content: string;
  /** File path, used to pick the syntax-highlighting grammar. */
  filePath: string;
}

/** djb2 over the file text, as a 32-bit unsigned hex string. Not a checksum:
 *  it only has to change when the content does, so an equal-length edit
 *  remounts the renderer instead of keeping stale row measurements. */
function hashContent(content: string): string {
  let h = 5381;
  for (let i = 0; i < content.length; i++) {
    h = (h * 33) ^ content.charCodeAt(i);
  }
  return (h >>> 0).toString(16);
}

/**
 * Full-file viewer for a file with no diff against the base (#1810, #4003).
 * Renders through the same `@pierre/diffs` file renderer the diff pane drives,
 * so a file pane and a diff pane of the same file share one gutter, one
 * highlighter and one theme path. Line numbers come from the renderer; the
 * library handles an unresolved grammar as plain text itself, so no local
 * fallback or stale-markup guard is needed here.
 */
export function FullFileViewer({ content, filePath }: Props) {
  const { theme } = useShikiTheme();

  const file = useMemo<FileContents>(() => ({ name: filePath, contents: content }), [filePath, content]);

  const options = useMemo<FileOptions<undefined, undefined>>(() => ({ theme, disableFileHeader: true }), [theme]);

  // Keyed on the path plus a content hash, standing in for DiffFileViewer's
  // revision (the /file read carries no revision or etag to key on): the same
  // path re-rendering with new content remeasures rows from scratch instead of
  // reusing the previous file's layout. A length alone would miss an
  // equal-length edit. See #4008 review.
  const viewKey = useMemo(() => `${filePath}:${content.length}:${hashContent(content)}`, [filePath, content]);

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <DiffWorkerPoolProvider>
        <Virtualizer key={viewKey} className="flex-1 overflow-auto">
          <File file={file} options={options} />
        </Virtualizer>
      </DiffWorkerPoolProvider>
    </div>
  );
}

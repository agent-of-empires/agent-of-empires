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

/**
 * Full-file viewer for a file with no diff against the base (#1810, #4003).
 * Renders through the same `@pierre/diffs` file renderer the diff pane drives,
 * so a file pane and a diff pane of the same file share one gutter, one
 * highlighter and one theme path. Line numbers come from the renderer; the
 * library handles an unresolved grammar as plain text itself, so no local
 * fallback or stale-markup guard is needed here.
 *
 * Continues the consolidation in #3913: #3958 moved highlighting onto this
 * library, this moves whole-file rendering.
 */
export function FullFileViewer({ content, filePath }: Props) {
  const { theme } = useShikiTheme();

  const file = useMemo<FileContents>(() => ({ name: filePath, contents: content }), [filePath, content]);

  const options = useMemo<FileOptions<undefined>>(() => ({ theme, disableFileHeader: true }), [theme]);

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <DiffWorkerPoolProvider>
        <Virtualizer key={filePath} className="flex-1 overflow-auto">
          <File file={file} options={options} />
        </Virtualizer>
      </DiffWorkerPoolProvider>
    </div>
  );
}

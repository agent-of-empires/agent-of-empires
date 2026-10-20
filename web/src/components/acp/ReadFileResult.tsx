import { useEffect, useState, type ReactNode } from "react";
import { FileDown } from "lucide-react";

import { useArtifactObjectUrl } from "../../hooks/useArtifactObjectUrl";
import { getSessionFile, sessionRawFileUrl } from "../../lib/api";
import { isImagePath } from "../../lib/imagePath";
import { openInNewTab } from "../../lib/openInNewTab";
import { ExpandableImage } from "./artifactMedia";
import { HighlightedBlock, PlaceholderLine } from "./ToolCardChrome";

type Loaded = { kind: "text"; text: string; truncated: boolean } | { kind: "binary" } | null;

const PDF_PATH = /\.pdf$/i;

/** Lines `offset` (1-based) through `offset + limit - 1`, as the Read call asked. */
function sliceLines(text: string, offset?: number, limit?: number): string {
  if (offset === undefined && limit === undefined) return text;
  const start = Math.max((offset ?? 1) - 1, 0);
  return text
    .split("\n")
    .slice(start, limit === undefined ? undefined : start + limit)
    .join("\n");
}

/** Lines in a prefix, the last one possibly cut mid-line. */
function lineCount(prefix: string): number {
  return prefix === "" ? 0 : prefix.replace(/\n$/, "").split("\n").length;
}

/** Whether the requested range starts past a server-truncated prefix. */
function startsBeyond(prefix: string, offset?: number): boolean {
  return Math.max((offset ?? 1) - 1, 0) >= lineCount(prefix);
}

/** Whether the requested lines reach past the complete lines of a server-truncated prefix. */
function missesLines(prefix: string, offset?: number, limit?: number): boolean {
  if (limit === undefined) return true;
  const complete = prefix.endsWith("\n") ? lineCount(prefix) : lineCount(prefix) - 1;
  return complete < Math.max((offset ?? 1) - 1, 0) + limit;
}

/** The file a Read call returned, fetched from the session when the adapter sent no text.
 *  It is the file as it is now, not as the agent read it, and says so. Text shows inline,
 *  an image in the lightbox, any other file as a chip to open. A refused or missing file
 *  renders nothing. */
export function ReadFileResult({
  sessionId,
  path,
  offset,
  limit,
  language,
}: {
  sessionId: string;
  path: string;
  offset?: number;
  limit?: number;
  language?: string;
}) {
  // Keyed by the request, so a card reused for another file shows nothing until its own load lands.
  const key = `${sessionId} ${path}`;
  const [result, setResult] = useState<{ key: string; loaded: Loaded } | null>(null);
  useEffect(() => {
    let stale = false;
    void getSessionFile(sessionId, path).then((file) => {
      if (stale || !file) return;
      // The server calls a file binary only when it holds a NUL byte, which a PDF may lack.
      const binary = file.is_binary || PDF_PATH.test(path);
      setResult({
        key,
        loaded: binary ? { kind: "binary" } : { kind: "text", text: file.content, truncated: file.truncated },
      });
    });
    return () => {
      stale = true;
    };
  }, [sessionId, path, key]);

  const loaded = result?.key === key ? result.loaded : null;
  if (!loaded) return null;
  let body: ReactNode;
  if (loaded.kind === "text") {
    const text = sliceLines(loaded.text, offset, limit);
    const cutShort = loaded.truncated && missesLines(loaded.text, offset, limit);
    body =
      loaded.truncated && startsBeyond(loaded.text, offset) ? (
        <PlaceholderLine>The requested lines are beyond the part of the file the server returned.</PlaceholderLine>
      ) : (
        <>
          <HighlightedBlock text={text} language={language} maxLines={16} unwrapFence={false} />
          {cutShort && <Note>Truncated by the server: the lines past its limit are not shown.</Note>}
        </>
      );
  } else {
    const url = sessionRawFileUrl(sessionId, path);
    body = isImagePath(path) ? (
      <ReadImage url={url} name={basename(path)} />
    ) : (
      <ReadFileChip url={url} name={basename(path)} />
    );
  }
  return (
    <>
      {body}
      <Note>Current file contents; the output of the original Read is unavailable.</Note>
    </>
  );
}

function Note({ children }: { children: ReactNode }) {
  return <div className="mt-1 text-[11px] italic text-text-dim">{children}</div>;
}

function basename(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

function ReadImage({ url, name }: { url: string; name: string }) {
  // The raw route needs the token a bare <img src> lacks.
  const src = useArtifactObjectUrl(url);
  if (!src) return <ReadFileChip url={url} name={name} />;
  return (
    <ExpandableImage
      src={src}
      alt={name}
      className="max-h-80 max-w-full rounded border border-surface-700 object-contain"
    />
  );
}

function ReadFileChip({ url, name }: { url: string; name: string }) {
  return (
    <button
      type="button"
      onClick={() => void openInNewTab(url, name)}
      className="inline-flex cursor-pointer items-center gap-2 text-xs text-accent-500 hover:underline"
    >
      <FileDown className="h-3.5 w-3.5" />
      {name}
    </button>
  );
}

import { useEffect, useRef, useState } from "react";
import { getSessionFile } from "../../lib/api";
import { useWebSettings } from "../../hooks/useWebSettings";
import { extensionToLanguage } from "./comments/language";
import { FullFileViewer } from "./FullFileViewer";
import { MarkdownFileView } from "./MarkdownFileView";
import { BackButton, Centered, MarkdownToggle, TooLarge } from "./viewerChrome";

interface Props {
  sessionId: string;
  /** The server confines which paths may be read. */
  filePath: string;
  onBack?: () => void;
}

interface Loaded {
  content: string;
  is_binary: boolean;
  truncated: boolean;
}

/** A session file: Markdown with a Rendered/Raw toggle, otherwise highlighted source. */
export function FileContentViewer({ sessionId, filePath, onBack }: Props) {
  const { settings } = useWebSettings();
  const containerRef = useRef<HTMLDivElement>(null);
  // Keyed by target so a stale response from a fast switch is ignored at render.
  const key = `${sessionId} ${filePath}`;
  const [loaded, setLoaded] = useState<{ key: string; data: Loaded | null; error: string | null }>({
    key,
    data: null,
    error: null,
  });

  useEffect(() => {
    let cancelled = false;
    getSessionFile(sessionId, filePath)
      .then((resp) => {
        if (cancelled) return;
        setLoaded({ key, data: resp ?? null, error: resp ? null : "Failed to load file" });
      })
      .catch(() => {
        if (!cancelled) setLoaded({ key, data: null, error: "Failed to load file" });
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, filePath, key]);

  const current = loaded.key === key ? loaded : { key, data: null, error: null };
  const data = current.data;
  const error = current.error;
  const loading = data === null && error === null;

  const isMarkdown = extensionToLanguage(filePath) === "markdown";
  const showRendered = isMarkdown && !data?.is_binary && settings.markdownPreview === "rendered";

  // Move focus into the viewer; FilesPane restores it to the row on close.
  useEffect(() => {
    containerRef.current?.focus();
  }, [filePath]);

  return (
    <div
      ref={containerRef}
      tabIndex={-1}
      className="flex-1 flex flex-col bg-surface-900 overflow-hidden focus:outline-none"
    >
      <div className="px-3 py-2 border-b border-surface-700/20 flex items-center gap-2 shrink-0">
        {onBack && <BackButton label="Files" onClick={onBack} />}
        <span className="font-mono text-[12px] text-text-primary truncate">{filePath}</span>
        {isMarkdown && !data?.is_binary && <MarkdownToggle className="ml-auto" />}
      </div>

      {loading ? (
        <Centered>
          <span className="text-sm">Loading file...</span>
        </Centered>
      ) : error ? (
        <Centered className="text-status-error">
          <span className="text-sm">{error}</span>
        </Centered>
      ) : data?.is_binary ? (
        <Centered>
          <span className="text-sm">Binary file</span>
        </Centered>
      ) : data?.truncated ? (
        <TooLarge what="File too large to display inline" hint="Open it in your editor instead." />
      ) : data && showRendered ? (
        <MarkdownFileView content={data.content} />
      ) : data ? (
        <FullFileViewer content={data.content} filePath={filePath} />
      ) : null}
    </div>
  );
}

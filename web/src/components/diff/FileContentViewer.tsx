import { useEffect, useRef, useState } from "react";
import { getSessionFile } from "../../lib/api";
import { useWebSettings } from "../../hooks/useWebSettings";
import { extensionToLanguage } from "./comments/language";
import { FullFileViewer } from "./FullFileViewer";
import { MarkdownFileView } from "./MarkdownFileView";

interface Props {
  sessionId: string;
  /** Project-relative or absolute path; the server enforces provenance. */
  filePath: string;
  /** Called when the user wants to go back to the file list. */
  onBack?: () => void;
}

interface Loaded {
  content: string;
  is_binary: boolean;
  truncated: boolean;
}

/**
 * View a session file fetched from the provenance-confined `/file` endpoint
 * (#3088). Markdown renders via {@link MarkdownFileView} with a Rendered/Raw
 * toggle (sharing the `markdownPreview` setting with the diff viewer); other
 * files fall back to the shared shiki {@link FullFileViewer}.
 */
export function FileContentViewer({ sessionId, filePath, onBack }: Props) {
  const { settings, update } = useWebSettings();
  const containerRef = useRef<HTMLDivElement>(null);
  // Result keyed by target: a stale response (from a rapid file switch) is
  // dropped at render time by comparing keys, so no state is reset in render or
  // synchronously in the effect. Mirrors FullFileViewer's async-only writes.
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

  // Move focus into the viewer when a file opens, so keyboard and screen-reader
  // users land on the content instead of staying on the (now unmounted) file
  // row. FilesPane restores focus to that row on close. See #3088 review.
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
        {onBack && (
          <button
            onClick={onBack}
            className="text-text-dim hover:text-text-secondary cursor-pointer transition-colors flex items-center gap-1 text-[11px]"
            title="Back to files"
            aria-label="Back to files"
          >
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.75"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M15 18l-6-6 6-6" />
            </svg>
            <span className="hidden sm:inline">Files</span>
          </button>
        )}
        <span className="font-mono text-[12px] text-text-primary truncate">{filePath}</span>
        {isMarkdown && !data?.is_binary && (
          <div className="ml-auto flex items-center rounded border border-surface-700/40 overflow-hidden">
            <button
              type="button"
              onClick={() => update({ markdownPreview: "rendered" })}
              aria-pressed={settings.markdownPreview === "rendered"}
              title="Rendered Markdown"
              className={`px-2 py-0.5 text-[11px] font-mono cursor-pointer transition-colors ${
                settings.markdownPreview === "rendered"
                  ? "bg-brand-600 text-white"
                  : "text-text-dim hover:text-text-secondary"
              }`}
            >
              Rendered
            </button>
            <button
              type="button"
              onClick={() => update({ markdownPreview: "raw" })}
              aria-pressed={settings.markdownPreview === "raw"}
              title="Raw Markdown source"
              className={`px-2 py-0.5 text-[11px] font-mono cursor-pointer transition-colors ${
                settings.markdownPreview === "raw"
                  ? "bg-brand-600 text-white"
                  : "text-text-dim hover:text-text-secondary"
              }`}
            >
              Raw
            </button>
          </div>
        )}
      </div>

      {loading ? (
        <div className="flex-1 flex items-center justify-center text-text-dim">
          <span className="text-sm">Loading file...</span>
        </div>
      ) : error ? (
        <div className="flex-1 flex items-center justify-center text-status-error">
          <span className="text-sm">{error}</span>
        </div>
      ) : data?.is_binary ? (
        <div className="flex-1 flex items-center justify-center text-text-dim">
          <span className="text-sm">Binary file</span>
        </div>
      ) : data?.truncated ? (
        <div className="flex-1 flex items-center justify-center text-text-dim">
          <div className="text-center px-4">
            <p className="text-sm mb-1">File too large to display inline</p>
            <p className="text-xs">Open it in your editor instead.</p>
          </div>
        </div>
      ) : data && showRendered ? (
        <MarkdownFileView content={data.content} />
      ) : data ? (
        <FullFileViewer content={data.content} filePath={filePath} />
      ) : null}
    </div>
  );
}

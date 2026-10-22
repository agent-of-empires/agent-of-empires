import { useEffect, useRef, useState } from "react";
import { Pencil } from "lucide-react";
import { TextFileEditor } from "./TextFileEditor";
import { requestComposerPrefill } from "../../lib/composerPrefill";
import { getSessionFile } from "../../lib/api";
import { useWebSettings } from "../../hooks/useWebSettings";
import { extensionToLanguage } from "./comments/language";
import { BinaryFileView } from "./BinaryFileView";
import { FullFileViewer } from "./FullFileViewer";
import { MarkdownFileView } from "./MarkdownFileView";
import { BackButton, Centered, MarkdownToggle, TooLarge } from "./viewerChrome";

interface Props {
  sessionId: string;
  /** The server confines which paths may be read. */
  filePath: string;
  onBack?: () => void;
  structured?: boolean;
}

interface Loaded {
  content: string;
  is_binary: boolean;
  truncated: boolean;
  version?: string | null;
  edit_disabled_reason?: string | null;
}

/** A session file: Markdown with a Rendered/Raw toggle, otherwise highlighted source. */
export function FileContentViewer(props: Props) {
  return <SessionFileViewer key={`${props.sessionId} ${props.filePath}`} {...props} />;
}

function SessionFileViewer({ sessionId, filePath, onBack, structured }: Props) {
  const { settings } = useWebSettings();
  // The opened snapshot, frozen so a refetch cannot move the version under a draft.
  const [editing, setEditing] = useState<Loaded | null>(null);
  const [revision, setRevision] = useState(0);
  const [saved, setSaved] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  // Keyed by target so a stale response from a fast switch is ignored at render.
  const key = `${sessionId} ${filePath} ${revision}`;
  const [loaded, setLoaded] = useState<{ key: string; data: Loaded | null; error: string | null }>({
    key,
    data: null,
    error: null,
  });

  useEffect(() => {
    let cancelled = false;
    getSessionFile(sessionId, filePath, { editing: settings.fileEditing })
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
  }, [sessionId, filePath, key, settings.fileEditing]);

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
      {!editing && (
        <div className="px-3 py-2 border-b border-surface-700/20 flex items-center gap-2 shrink-0">
          {onBack && <BackButton label="Files" onClick={onBack} />}
          <span className="font-mono text-[12px] text-text-primary truncate flex-1">{filePath}</span>
          {isMarkdown && !data?.is_binary && <MarkdownToggle />}
          {settings.fileEditing && data && (
            <button
              type="button"
              disabled={!data.version || !!data.edit_disabled_reason}
              title={data.edit_disabled_reason ?? undefined}
              className="flex items-center gap-1 min-h-8 px-2 py-1 text-xs rounded border border-surface-700/50 text-text-secondary disabled:opacity-40 cursor-pointer"
              onClick={() => {
                setEditing(data);
                setSaved(false);
              }}
            >
              <Pencil size={13} aria-hidden="true" />
              Edit
            </button>
          )}
        </div>
      )}

      {!editing && settings.fileEditing && data?.edit_disabled_reason && (
        <div className="px-3 py-2 text-xs text-text-dim">{data.edit_disabled_reason}</div>
      )}
      {saved && (
        <div role="status" className="px-3 py-2 text-xs text-text-secondary flex gap-2 items-center flex-wrap">
          <span>File saved.</span>
          {structured ? (
            <button
              type="button"
              className="underline cursor-pointer"
              onClick={() => {
                requestComposerPrefill(
                  sessionId,
                  `I edited ${filePath} in the file viewer. Please review the changes before continuing.`,
                );
                setSaved(false);
              }}
            >
              Draft a note to the agent
            </button>
          ) : (
            <span>Let the agent know you changed {filePath} in its terminal.</span>
          )}
        </div>
      )}
      {editing ? (
        <TextFileEditor
          sessionId={sessionId}
          filePath={filePath}
          original={editing}
          onBack={onBack}
          onCancel={() => {
            setEditing(null);
            setRevision((value) => value + 1);
          }}
          onSaved={() => {
            setEditing(null);
            setSaved(true);
            setRevision((value) => value + 1);
          }}
        />
      ) : loading ? (
        <Centered>
          <span className="text-sm">Loading file...</span>
        </Centered>
      ) : error ? (
        <Centered className="text-status-error">
          <span className="text-sm">{error}</span>
        </Centered>
      ) : data?.is_binary ? (
        <BinaryFileView sessionId={sessionId} filePath={filePath} />
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

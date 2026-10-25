import { useEffect, useRef, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { BackButton } from "./viewerChrome";
import { getSessionFile, saveSessionFile, type SessionFileResponse } from "../../lib/api";
import { writeClipboard } from "../../lib/clipboard";
import {
  confirmFileEditNavigation,
  fileDraftKey,
  getFileDraft,
  preserveLineEndings,
  registerFileEditor,
  setFileDraft,
} from "../../lib/fileEditGuard";
import { useLatestRef } from "../../hooks/useLatestRef";

interface Props {
  sessionId: string;
  filePath: string;
  original: SessionFileResponse;
  onBack?(): void;
  onCancel(): void;
  onSaved(): void;
}

const button =
  "min-h-8 px-2 py-1 text-xs rounded-md border border-surface-700/50 text-text-secondary hover:bg-surface-800 disabled:opacity-40 cursor-pointer";

export function TextFileEditor({ sessionId, filePath, original, onBack, onCancel, onSaved }: Props) {
  const normalized = original.content.replace(/\r\n|\r/g, "\n");
  const key = fileDraftKey(sessionId, filePath);
  const [draft, setDraft] = useState(() => getFileDraft(key)?.text ?? normalized);
  const [pendingSave, setPendingSave] = useState(() => getFileDraft(key)?.pendingSave ?? null);
  const saving = pendingSave !== null;
  const [changed, setChanged] = useState(false);
  const [checkFailed, setCheckFailed] = useState(false);
  const [editDisabledReason, setEditDisabledReason] = useState(original.edit_disabled_reason ?? null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const textarea = useRef<HTMLTextAreaElement>(null);
  const dirty = draft !== normalized;
  const latest = useLatestRef({ onCancel, onSaved });

  useEffect(() => {
    textarea.current?.focus();
    return registerFileEditor(key, () => latest.current.onCancel());
  }, [key, latest]);

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    let checking = false;
    const check = async () => {
      if (checking || cancelled) return;
      checking = true;
      clearTimeout(timer);
      try {
        const current = await getSessionFile(sessionId, filePath, { versionOnly: true });
        if (cancelled || getFileDraft(key)?.pendingSave) return;
        setCheckFailed(current === null);
        setEditDisabledReason(current?.edit_disabled_reason ?? null);
        if (current && current.version !== original.version) setChanged(true);
      } finally {
        checking = false;
        if (!cancelled) timer = setTimeout(() => void check(), 2000);
      }
    };
    timer = setTimeout(() => void check(), 2000);
    const onFocus = () => {
      clearTimeout(timer);
      void check();
    };
    window.addEventListener("focus", onFocus);
    return () => {
      cancelled = true;
      clearTimeout(timer);
      window.removeEventListener("focus", onFocus);
    };
  }, [sessionId, filePath, original.version, key]);

  useEffect(() => {
    if (!pendingSave) return;
    let cancelled = false;
    void pendingSave.then((result) => {
      if (cancelled) return;
      if (result.ok) {
        setFileDraft(key, null);
        latest.current.onSaved();
      } else {
        setFileDraft(key, { original, text: draft, pendingSave: null });
        if (result.status === 409 || result.status === 404) setChanged(true);
        setError(result.error ?? "Could not save file. Your edits are kept.");
      }
      setPendingSave(null);
    });
    return () => {
      cancelled = true;
    };
  }, [pendingSave, key, original, draft, latest]);

  const discard = () => {
    setFileDraft(key, null);
    onCancel();
  };
  const cancel = () => {
    if (getFileDraft(key)?.pendingSave) return;
    if (!dirty || window.confirm("Discard unsaved file changes?")) discard();
  };
  const save = () => {
    if (getFileDraft(key)?.pendingSave || !dirty || changed || editDisabledReason || !original.version) return;
    const pending = saveSessionFile(
      sessionId,
      filePath,
      original.version,
      preserveLineEndings(draft, original.content),
    ).catch(() => ({
      ok: false,
      status: 0,
      error: "Save could not be confirmed. Your edits are kept; check the file before retrying.",
    }));
    setFileDraft(key, { original, text: draft, pendingSave: pending });
    setPendingSave(pending);
    setError(null);
  };

  return (
    <div className="flex-1 min-h-0 flex flex-col" data-file-editor>
      <div className="flex gap-2 items-center px-3 py-2 border-b border-surface-700/20 shrink-0">
        {onBack && (
          <BackButton
            label="Files"
            onClick={() => {
              if (confirmFileEditNavigation()) onBack();
            }}
          />
        )}
        {dirty && <span aria-label="Unsaved changes" className="w-2 h-2 rounded-full bg-status-warning shrink-0" />}
        <span className="font-mono text-[12px] text-text-primary truncate flex-1" title={filePath}>
          {filePath}
        </span>
        <button className={button} disabled={saving} onClick={cancel}>
          Cancel
        </button>
        <button
          className={`${button} bg-brand-600 text-text-on-brand hover:bg-brand-500 disabled:hover:bg-brand-600`}
          disabled={!dirty || saving || changed || !!editDisabledReason}
          onClick={() => void save()}
          title="Save (Ctrl/Cmd+S)"
        >
          {saving ? "Saving..." : "Save"}
        </button>
      </div>
      {changed && (
        <div
          role="alert"
          className="px-3 py-2 text-xs text-status-warning bg-status-warning/10 border-b border-status-warning/30"
        >
          <span className="flex items-start gap-2">
            <AlertTriangle size={15} className="shrink-0" aria-hidden="true" />
            <span>File changed on disk. Copy your edits or reload before saving.</span>
          </span>
          <div className="flex gap-2 mt-2">
            <button
              className={button}
              onClick={() => {
                void writeClipboard(draft).then((ok) => {
                  setCopied(ok);
                  if (!ok) setError("Could not copy. Select your text and copy it manually.");
                });
              }}
            >
              {copied ? "Copied" : "Copy my edits"}
            </button>
            <button
              className={button}
              disabled={saving}
              onClick={() => {
                if (!dirty || window.confirm("Discard your edits and reload the file from disk?")) discard();
              }}
            >
              Reload from disk
            </button>
          </div>
        </div>
      )}
      {editDisabledReason && (
        <div role="status" className="px-3 py-2 text-xs text-text-dim">
          {editDisabledReason}
        </div>
      )}
      {checkFailed && (
        <div role="status" className="px-3 py-2 text-xs text-text-dim">
          Cannot check disk changes. Your edits are kept.
        </div>
      )}
      {error && (
        <div role="alert" className="px-3 py-2 text-xs text-status-error">
          {error}
        </div>
      )}
      <textarea
        ref={textarea}
        aria-label={`Edit ${filePath}`}
        value={draft}
        disabled={saving}
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        className="flex-1 min-h-0 w-full resize-none p-3 font-mono text-base md:text-[12px] text-text-primary bg-surface-950 focus:outline-none focus:ring-1 focus:ring-inset focus:ring-brand-500/50 whitespace-pre overflow-auto"
        onChange={(event) => {
          const text = event.target.value;
          setDraft(text);
          setFileDraft(key, text === normalized ? null : { original, text, pendingSave: null });
          setCopied(false);
        }}
        onKeyDown={(event) => {
          if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
            event.preventDefault();
            event.stopPropagation();
            void save();
          } else if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            cancel();
          }
        }}
      />
    </div>
  );
}

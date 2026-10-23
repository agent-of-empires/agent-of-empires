import type { SessionFileResponse } from "./api";
import { toastBus } from "./toastBus";

interface FileDraft {
  /** The snapshot the edit started from; its version guards the save. */
  original: SessionFileResponse;
  text: string;
  saving: boolean;
}

// Unsaved drafts outlive their editor, so a layout switch or reopening the file restores them.
// Leaving an open editor asks first; a page unload loses every draft.
const drafts = new Map<string, FileDraft>();
// Mounted editors by draft key; discarding a draft closes its editor.
const editors = new Map<string, () => void>();

export function fileDraftKey(sessionId: string, filePath: string): string {
  return `${sessionId} ${filePath}`;
}

export function getFileDraft(key: string): FileDraft | undefined {
  return drafts.get(key);
}

/** Store an unsaved draft, or drop it with `null` once saved, discarded or clean. */
export function setFileDraft(key: string, draft: FileDraft | null): void {
  if (draft) drafts.set(key, draft);
  else drafts.delete(key);
}

export function registerFileEditor(key: string, discard: () => void): () => void {
  editors.set(key, discard);
  return () => {
    if (editors.get(key) === discard) editors.delete(key);
  };
}

/** An open editor owns Escape: it cancels the edit rather than closing the file. */
export function isFileEditorOpen(): boolean {
  return editors.size > 0;
}

function openDrafts(): [string, FileDraft][] {
  return [...drafts].filter(([key]) => editors.has(key));
}

export function hasUnsavedFileEdits(): boolean {
  return openDrafts().length > 0;
}

export function confirmFileEditNavigation(): boolean {
  const open = openDrafts();
  if (open.some(([, draft]) => draft.saving)) {
    toastBus.handler?.error("Wait for the file save to finish.");
    return false;
  }
  if (open.length === 0) return true;
  if (!window.confirm("Discard unsaved file changes?")) return false;
  open.forEach(([key]) => {
    drafts.delete(key);
    editors.get(key)?.();
  });
  return true;
}

let previousLocation: { index: number | undefined; state: unknown; url: string } | null = null;
let restoringHistory = false;
let installed = false;

export function rememberFileEditLocation(): void {
  if (restoringHistory) return;
  previousLocation = {
    index: window.history.state?.idx as number | undefined,
    state: window.history.state,
    url: window.location.href,
  };
}

/** Install before BrowserRouter so its popstate listener cannot unmount a dirty editor first. */
export function installFileEditGuards(): void {
  if (installed) return;
  installed = true;
  window.addEventListener("beforeunload", (event) => {
    if (drafts.size === 0) return;
    event.preventDefault();
    event.returnValue = "";
  });
  window.addEventListener(
    "popstate",
    (event: PopStateEvent) => {
      if (restoringHistory) {
        restoringHistory = false;
        event.stopImmediatePropagation();
        return;
      }
      if (!previousLocation || !hasUnsavedFileEdits() || confirmFileEditNavigation()) return;
      event.stopImmediatePropagation();
      const index = window.history.state?.idx as number | undefined;
      if (index !== undefined && previousLocation.index !== undefined && index !== previousLocation.index) {
        restoringHistory = true;
        window.history.go(previousLocation.index - index);
      } else {
        window.history.pushState(previousLocation.state, "", previousLocation.url);
      }
    },
    true,
  );
}

/** A textarea normalizes CRLF and CR. Keep each original line ending where it exists. */
export function preserveLineEndings(edited: string, original: string): string {
  const endings = original.match(/\r\n|\r|\n/g) ?? [];
  const fallback = endings[0] ?? "\n";
  let index = 0;
  return edited.replace(/\r\n|\r|\n/g, () => endings[index++] ?? fallback);
}

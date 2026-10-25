import type { SessionFileResponse, saveSessionFile } from "./api";
import { toastBus } from "./toastBus";

interface FileSave {
  promise: ReturnType<typeof saveSessionFile>;
  result?: Awaited<ReturnType<typeof saveSessionFile>>;
}

interface FileDraft {
  /** The snapshot the edit started from; its version guards the save. */
  original: SessionFileResponse;
  text: string;
  pendingSave: FileSave | null;
}

// Unsaved drafts outlive their editor, so a layout switch or reopening the file restores them.
// Leaving an open editor asks first; a page unload loses every draft.
const drafts = new Map<string, FileDraft>();
// One editor owns each draft, even when multiple panes show the same file.
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

/** The request settles independently of whichever layout currently owns the editor. */
export function startFileSave(
  key: string,
  draft: Omit<FileDraft, "pendingSave">,
  promise: ReturnType<typeof saveSessionFile>,
): FileSave {
  const save: FileSave = { promise };
  setFileDraft(key, { ...draft, pendingSave: save });
  void promise.then((result) => {
    save.result = result;
  });
  return save;
}

export function isFileSaving(key: string): boolean {
  const save = drafts.get(key)?.pendingSave;
  return save != null && save.result === undefined;
}

function isUnsaved(draft: FileDraft): boolean {
  return !draft.pendingSave?.result?.ok;
}

export function registerFileEditor(key: string, discard: () => void): () => void {
  editors.get(key)?.();
  editors.set(key, discard);
  return () => {
    if (editors.get(key) === discard) editors.delete(key);
  };
}

/** An open editor owns Escape: it cancels the edit rather than closing the file. */
export function isFileEditorOpen(): boolean {
  return editors.size > 0;
}

function blockingDrafts(): [string, FileDraft][] {
  return [...drafts].filter(([key, draft]) => isUnsaved(draft) && (editors.has(key) || isFileSaving(key)));
}

export function hasUnsavedFileEdits(): boolean {
  return blockingDrafts().length > 0;
}

export function confirmFileEditNavigation(): boolean {
  return confirmDrafts(blockingDrafts());
}

/** Removing a Session also discards drafts parked outside the current layout. */
export function confirmFileEditRemoval(sessionIds: readonly string[]): boolean {
  return confirmDrafts(
    [...drafts].filter(
      ([key, draft]) => isUnsaved(draft) && sessionIds.some((id) => key.startsWith(fileDraftKey(id, ""))),
    ),
  );
}

function confirmDrafts(affected: [string, FileDraft][]): boolean {
  if ([...drafts.keys()].some(isFileSaving)) {
    toastBus.handler?.error("Wait for the file save to finish.");
    return false;
  }
  if (affected.length === 0) return true;
  if (!window.confirm("Discard unsaved file changes?")) return false;
  affected.forEach(([key]) => {
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
    if (![...drafts.values()].some(isUnsaved)) return;
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

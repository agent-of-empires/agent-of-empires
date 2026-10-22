import { toastBus } from "./toastBus";

interface EditGuard {
  dirty(): boolean;
  saving(): boolean;
  discard(): void;
}

const guards = new Set<EditGuard>();

export function registerFileEditGuard(guard: EditGuard): () => void {
  guards.add(guard);
  return () => {
    guards.delete(guard);
  };
}

/** An open editor owns Escape: it cancels the edit rather than closing the file. */
export function isFileEditorOpen(): boolean {
  return guards.size > 0;
}

export function hasUnsavedFileEdits(): boolean {
  return [...guards].some((guard) => guard.dirty() || guard.saving());
}

export function confirmFileEditNavigation(): boolean {
  if ([...guards].some((guard) => guard.saving())) {
    toastBus.handler?.error("Wait for the file save to finish.");
    return false;
  }
  const dirty = [...guards].filter((guard) => guard.dirty());
  if (dirty.length === 0) return true;
  if (!window.confirm("Discard unsaved file changes?")) return false;
  dirty.forEach((guard) => {
    guards.delete(guard);
    guard.discard();
  });
  return true;
}

let previousLocation: { index: number | undefined; state: unknown; url: string } | null = null;
let restoringHistory = false;
let historyInstalled = false;

export function rememberFileEditLocation(): void {
  if (restoringHistory) return;
  previousLocation = {
    index: window.history.state?.idx as number | undefined,
    state: window.history.state,
    url: window.location.href,
  };
}

/** Install before BrowserRouter so its popstate listener cannot unmount a dirty editor first. */
export function installFileEditHistoryGuard(): void {
  if (historyInstalled) return;
  historyInstalled = true;
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

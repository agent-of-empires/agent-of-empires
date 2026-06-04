import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  DiffComment,
  DiffCommentDraft,
  DiffCommentsStorageV1,
} from "../components/diff/comments/types";
import {
  EMPTY_STORAGE,
  loadComments,
  saveComments,
} from "../components/diff/comments/storage";

export interface UseDiffCommentsResult {
  comments: DiffComment[];
  count: number;
  clearAfterSend: boolean;
  setClearAfterSend(v: boolean): void;
  introDraft: string;
  outroDraft: string;
  setIntroDraft(v: string): void;
  setOutroDraft(v: string): void;
  addComment(draft: DiffCommentDraft): DiffComment;
  updateComment(id: string, body: string): void;
  deleteComment(id: string): void;
  clearComments(): void;
}

/** Session-scoped comments store backed by localStorage. Comments
 *  persist across page reloads inside the same session and are wiped
 *  when the user explicitly clears them or after a successful send
 *  (when `clearAfterSend` is true). State only switches when the
 *  session id changes; if the active session changes we reload from
 *  storage so each session sees its own list. See #928. */
export function useDiffComments(
  sessionId: string | null,
): UseDiffCommentsResult {
  const [state, setState] = useState<DiffCommentsStorageV1>(() =>
    sessionId ? loadComments(sessionId) : { ...EMPTY_STORAGE },
  );

  // Track the latest state in a ref so save operations always have
  // the current data without depending on state in effect deps.
  const stateRef = useRef(state);
  useEffect(() => {
    stateRef.current = state;
  }, [state]);
  const [trackedSessionId, setTrackedSessionId] = useState(sessionId);

  // Render-time sync: reload from storage when sessionId changes.
  if (sessionId !== trackedSessionId) {
    setTrackedSessionId(sessionId);
    setState(sessionId ? loadComments(sessionId) : { ...EMPTY_STORAGE });
  }

  // Debounced save to localStorage. A counter drives the effect so
  // it re-runs when state changes, but we read the latest state via
  // stateRef to avoid a direct state dependency.
  const [saveCounter, setSaveCounter] = useState(0);
  const bumpSave = useCallback(() => setSaveCounter((c) => c + 1), []);
  const debounceTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (!sessionId) return;
    debounceTimerRef.current = setTimeout(() => {
      saveComments(sessionId, stateRef.current);
    }, 200);
    return () => {
      if (debounceTimerRef.current) clearTimeout(debounceTimerRef.current);
    };
  }, [sessionId, saveCounter]);

  // Flush any pending debounced write before the tab closes / hides
  // so the user doesn't lose the last keystrokes on a refresh.
  useEffect(() => {
    if (!sessionId) return;
    const flush = () => saveComments(sessionId, stateRef.current);
    window.addEventListener("beforeunload", flush);
    window.addEventListener("pagehide", flush);
    return () => {
      window.removeEventListener("beforeunload", flush);
      window.removeEventListener("pagehide", flush);
    };
  }, [sessionId]);

  const addComment = useCallback(
    (draft: DiffCommentDraft): DiffComment => {
      const created: DiffComment = {
        id: cryptoRandomId(),
        createdAt: new Date().toISOString(),
        ...draft,
      };
      setState((s) => ({ ...s, comments: [...s.comments, created] }));
      bumpSave();
      return created;
    },
    [bumpSave],
  );

  const updateComment = useCallback((id: string, body: string) => {
    const ts = new Date().toISOString();
    setState((s) => ({
      ...s,
      comments: s.comments.map((c) =>
        c.id === id ? { ...c, body, updatedAt: ts } : c,
      ),
    }));
    bumpSave();
  }, [bumpSave]);

  const deleteComment = useCallback((id: string) => {
    setState((s) => ({
      ...s,
      comments: s.comments.filter((c) => c.id !== id),
    }));
    bumpSave();
  }, [bumpSave]);

  const clearComments = useCallback(() => {
    setState((s) => ({ ...s, comments: [] }));
    bumpSave();
  }, [bumpSave]);

  const setClearAfterSend = useCallback((v: boolean) => {
    setState((s) => ({ ...s, clearAfterSend: v }));
    bumpSave();
  }, [bumpSave]);

  const setIntroDraft = useCallback((v: string) => {
    setState((s) => ({ ...s, introDraft: v }));
    bumpSave();
  }, [bumpSave]);

  const setOutroDraft = useCallback((v: string) => {
    setState((s) => ({ ...s, outroDraft: v }));
    bumpSave();
  }, [bumpSave]);

  return useMemo(
    () => ({
      comments: state.comments,
      count: state.comments.length,
      clearAfterSend: state.clearAfterSend,
      setClearAfterSend,
      introDraft: state.introDraft,
      outroDraft: state.outroDraft,
      setIntroDraft,
      setOutroDraft,
      addComment,
      updateComment,
      deleteComment,
      clearComments,
    }),
    [
      state,
      addComment,
      updateComment,
      deleteComment,
      clearComments,
      setClearAfterSend,
      setIntroDraft,
      setOutroDraft,
    ],
  );
}

function cryptoRandomId(): string {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  // Fallback for environments without crypto.randomUUID (older Safari, jsdom).
  return `dc_${Date.now().toString(36)}_${Math.random()
    .toString(36)
    .slice(2, 10)}`;
}

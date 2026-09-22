import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { DiffComment, DiffCommentDraft, DiffCommentsStorageV1 } from "../components/diff/comments/types";
import { EMPTY_STORAGE, loadComments, saveComments } from "../components/diff/comments/storage";
import { listen } from "./domEvents";
import { useLatestRef } from "./useLatestRef";

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

export function useDiffComments(sessionId: string | null): UseDiffCommentsResult {
  const [state, setState] = useState<DiffCommentsStorageV1>(() =>
    sessionId ? loadComments(sessionId) : { ...EMPTY_STORAGE },
  );

  const stateRef = useLatestRef(state);
  const [trackedSessionId, setTrackedSessionId] = useState(sessionId);

  if (sessionId !== trackedSessionId) {
    setTrackedSessionId(sessionId);
    setState(sessionId ? loadComments(sessionId) : { ...EMPTY_STORAGE });
  }

  const [saveCounter, setSaveCounter] = useState(0);
  const patch = useCallback((fn: (s: DiffCommentsStorageV1) => DiffCommentsStorageV1) => {
    setState(fn);
    setSaveCounter((c) => c + 1);
  }, []);
  const debounceTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (!sessionId) return;
    debounceTimerRef.current = setTimeout(() => {
      saveComments(sessionId, stateRef.current);
    }, 200);
    return () => {
      if (debounceTimerRef.current) clearTimeout(debounceTimerRef.current);
    };
  }, [sessionId, saveCounter, stateRef]);

  useEffect(() => {
    if (!sessionId) return;
    const flush = () => saveComments(sessionId, stateRef.current);
    return listen(flush, [window, "beforeunload"], [window, "pagehide"]);
  }, [sessionId, stateRef]);

  const addComment = useCallback(
    (draft: DiffCommentDraft): DiffComment => {
      const created: DiffComment = {
        id: globalThis.crypto.randomUUID(),
        createdAt: new Date().toISOString(),
        ...draft,
      };
      patch((s) => ({ ...s, comments: [...s.comments, created] }));
      return created;
    },
    [patch],
  );

  const updateComment = useCallback(
    (id: string, body: string) => {
      const updatedAt = new Date().toISOString();
      patch((s) => ({ ...s, comments: s.comments.map((c) => (c.id === id ? { ...c, body, updatedAt } : c)) }));
    },
    [patch],
  );

  const deleteComment = useCallback(
    (id: string) => patch((s) => ({ ...s, comments: s.comments.filter((c) => c.id !== id) })),
    [patch],
  );
  const clearComments = useCallback(() => patch((s) => ({ ...s, comments: [] })), [patch]);
  const setClearAfterSend = useCallback((v: boolean) => patch((s) => ({ ...s, clearAfterSend: v })), [patch]);
  const setIntroDraft = useCallback((v: string) => patch((s) => ({ ...s, introDraft: v })), [patch]);
  const setOutroDraft = useCallback((v: string) => patch((s) => ({ ...s, outroDraft: v })), [patch]);

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
    [state, addComment, updateComment, deleteComment, clearComments, setClearAfterSend, setIntroDraft, setOutroDraft],
  );
}

// State hooks behind the Composer: the stable composer client, queue recall,
// draft persistence, touch-input detection, trigger adapters, and attachments.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useAui, useAuiState } from "@assistant-ui/react";
import type { Unstable_TriggerAdapter, Unstable_TriggerItem } from "@assistant-ui/core";

import { getDraft, setDraft } from "../../lib/acpDrafts";
import type { AcpState, PromptAttachmentInput, PromptCapabilities, QueuedPrompt } from "../../lib/acpTypes";
import { useClearAliases } from "../../lib/agentProfileContext";
import { fitTextarea, fileToBase64, kindSupported, MAX_ATTACHMENTS, mimeToKind } from "./composerInput";
import { nextRecallTarget, recallBannerInfo, type RecallCursor, type RecallNav } from "./recallNav";
import { fuzzyFilter, useFilesIndex } from "./useFilesIndex";

type TextareaRef = React.RefObject<HTMLTextAreaElement | null>;

/** The slice of `aui.composer` the Composer drives. */
export type ComposerClient = Pick<ReturnType<typeof useAui>["composer"], "getState" | "setText">;

/** A stable client over `aui.composer`, whose identity changes on every store
 *  update. `draftTextRef` is stamped synchronously on `setText`, because the
 *  store applies it a tick later and a persist in that window would restore a
 *  just-sent prompt. */
export function useComposerClient() {
  const aui = useAui();
  const composerRef = useRef(aui.composer);
  useEffect(() => {
    composerRef.current = aui.composer;
  }, [aui]);
  const composerText = useAuiState((s) => s.composer.text);
  const draftTextRef = useRef(composerText);
  useEffect(() => {
    draftTextRef.current = composerText;
  }, [composerText]);
  const client = useMemo<ComposerClient>(
    () => ({
      getState: () => composerRef.current.getState(),
      setText: (text: string) => {
        draftTextRef.current = text;
        composerRef.current.setText(text);
      },
    }),
    [],
  );
  return { client, composerText, draftTextRef };
}

/** Set the text, then focus with the caret at the end and resize (setText fires no input event). */
export function useLoadText(client: ComposerClient, taRef: TextareaRef) {
  return useCallback(
    (text: string) => {
      client.setText(text);
      requestAnimationFrame(() => {
        const el = taRef.current;
        if (!el) return;
        el.focus();
        const len = el.value.length;
        try {
          el.setSelectionRange(len, len);
        } catch {
          // Detached nodes can throw.
        }
        fitTextarea(el);
      });
    },
    [client, taRef],
  );
}

/** Per-session draft in localStorage: seeded on mount, debounced while typing,
 *  and flushed on unload, hide, and unmount. */
export function useDraftPersistence(
  sessionId: string,
  client: ComposerClient,
  composerText: string,
  draftTextRef: React.RefObject<string>,
  taRef: TextareaRef,
) {
  useEffect(() => {
    const saved = getDraft(sessionId);
    if (saved && client.getState().text === "") {
      client.setText(saved);
      requestAnimationFrame(() => {
        if (taRef.current) fitTextarea(taRef.current);
      });
    }
    const flush = () => setDraft(sessionId, draftTextRef.current);
    // iOS Safari fires pagehide only on real unload, not on app switch.
    const onHidden = () => {
      if (document.visibilityState === "hidden") flush();
    };
    window.addEventListener("beforeunload", flush);
    window.addEventListener("pagehide", flush);
    document.addEventListener("visibilitychange", onHidden);
    return () => {
      window.removeEventListener("beforeunload", flush);
      window.removeEventListener("pagehide", flush);
      document.removeEventListener("visibilitychange", onHidden);
      flush();
    };
  }, [client, sessionId, draftTextRef, taRef]);

  // Reads the ref at fire time so a write scheduled before a send persists the cleared text.
  useEffect(() => {
    const writeTimer = window.setTimeout(() => setDraft(sessionId, draftTextRef.current), 250);
    return () => window.clearTimeout(writeTimer);
  }, [composerText, sessionId, draftTextRef]);
}

/** Touch-primary with no precise pointer. An iPad with a trackpad counts as desktop
 *  so hardware-keyboard Enter still sends. */
function detectMobileInput(): boolean {
  if (typeof window === "undefined" || !window.matchMedia) return false;
  return window.matchMedia("(pointer: coarse)").matches && !window.matchMedia("(any-pointer: fine)").matches;
}

export function useIsMobileInput() {
  const [isMobile, setIsMobile] = useState<boolean>(() => detectMobileInput());
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const queries = [window.matchMedia("(pointer: coarse)"), window.matchMedia("(any-pointer: fine)")];
    const onChange = () => setIsMobile(detectMobileInput());
    for (const q of queries) q.addEventListener("change", onChange);
    return () => {
      for (const q of queries) q.removeEventListener("change", onChange);
    };
  }, []);
  return isMobile;
}

const flatAdapter = (search: Unstable_TriggerAdapter["search"]): Unstable_TriggerAdapter => ({
  categories: () => [],
  categoryItems: () => [],
  search,
});

/** `@` files and `/` commands, both as flat search lists. Server clear aliases the
 *  agent does not advertise itself (codex/opencode `/new`) are appended. */
export function useTriggerAdapters(sessionId: string, availableCommands: AcpState["availableCommands"]) {
  const { files } = useFilesIndex(sessionId);
  const fileAdapter = useMemo(
    () =>
      flatAdapter((query) =>
        fuzzyFilter(
          files.map((path) => ({
            id: path,
            type: "file",
            label: path,
            description: path.match(/\.([a-z0-9]+)$/i)?.[1]?.toLowerCase(),
          })),
          query,
          30,
        ),
      ),
    [files],
  );
  const clearAliases = useClearAliases();
  const slashAdapter = useMemo(() => {
    const advertised = new Set(availableCommands.map((c) => c.name));
    const items: Unstable_TriggerItem[] = availableCommands.map((c) => ({
      id: c.name,
      type: "command",
      label: `/${c.name}`,
      description: c.description,
      acceptsInput: c.accepts_input,
    }));
    for (const alias of clearAliases) {
      const name = alias.startsWith("/") ? alias.slice(1) : alias;
      if (!name || advertised.has(name)) continue;
      items.push({
        id: name,
        type: "command",
        label: `/${name}`,
        description: "clear conversation",
        acceptsInput: false,
      } as Unstable_TriggerItem);
      advertised.add(name);
    }
    return flatAdapter((query) => fuzzyFilter(items, query, 30));
  }, [availableCommands, clearAliases]);
  return { fileAdapter, slashAdapter };
}

/** ArrowUp/ArrowDown recall of queued prompts. The cursor lives in a ref for the
 *  synchronous keydown handler; `recallInfo` mirrors it for the banner. */
export function useQueueRecall(
  queuedPrompts: QueuedPrompt[],
  client: ComposerClient,
  loadText: (text: string) => void,
) {
  const recallRef = useRef<RecallCursor | null>(null);
  const [recallInfo, setRecallInfo] = useState<{ pos: number; total: number } | null>(null);
  const applyRecall = useCallback(
    (next: RecallCursor | null) => {
      recallRef.current = next;
      setRecallInfo(recallBannerInfo(queuedPrompts, next));
    },
    [queuedPrompts],
  );
  const applyNav = useCallback(
    (nav: RecallNav) => {
      if (nav.kind === "load") {
        applyRecall(nav.cursor);
        loadText(nav.text);
      } else if (nav.kind === "restore") {
        loadText(nav.text);
        applyRecall(null);
      } else if (nav.kind === "exit") {
        applyRecall(null);
      }
    },
    [applyRecall, loadText],
  );
  const recall = useCallback(
    (direction: "older" | "newer") =>
      applyNav(
        nextRecallTarget(
          queuedPrompts,
          recallRef.current,
          direction,
          direction === "older" ? client.getState().text : "",
        ),
      ),
    [queuedPrompts, client, applyNav],
  );
  const cancelToDraft = useCallback(() => {
    const cur = recallRef.current;
    if (!cur) return;
    loadText(cur.stashedDraft);
    applyRecall(null);
  }, [loadText, applyRecall]);
  return { recallRef, recallInfo, applyRecall, recall, cancelToDraft };
}

/** Attachments the agent accepts, capped per prompt; the server re-validates. */
export function useAttachments(
  promptCapabilities: PromptCapabilities | null,
  pendingAttachments: PromptAttachmentInput[],
  setPendingAttachments: React.Dispatch<React.SetStateAction<PromptAttachmentInput[]>>,
) {
  const addFiles = useCallback(
    async (files: FileList | File[]) => {
      // Encode only what fits, so a large drop does not stall on discarded files.
      const remaining = Math.max(0, MAX_ATTACHMENTS - pendingAttachments.length);
      if (remaining === 0) return;
      const accepted: PromptAttachmentInput[] = [];
      for (const file of Array.from(files).slice(0, remaining)) {
        const kind = mimeToKind(file.type || "application/octet-stream");
        if (!kindSupported(kind, promptCapabilities)) continue;
        const dataB64 = await fileToBase64(file);
        if (!dataB64) continue;
        accepted.push({
          kind,
          mimeType: file.type || "application/octet-stream",
          name: file.name || undefined,
          dataB64,
        });
      }
      if (accepted.length === 0) return;
      setPendingAttachments((prev) => prev.concat(accepted).slice(0, MAX_ATTACHMENTS));
    },
    [pendingAttachments.length, promptCapabilities, setPendingAttachments],
  );
  const supported = useMemo(
    () =>
      promptCapabilities
        ? pendingAttachments.filter((att) => kindSupported(att.kind, promptCapabilities))
        : pendingAttachments,
    [pendingAttachments, promptCapabilities],
  );
  const remove = useCallback(
    (index: number) => {
      const target = supported[index];
      if (!target) return;
      setPendingAttachments((prev) => prev.filter((att) => att !== target));
    },
    [setPendingAttachments, supported],
  );
  const enabled =
    !!promptCapabilities &&
    (promptCapabilities.image || promptCapabilities.audio || promptCapabilities.embeddedContext);
  return { addFiles, supported, remove, enabled };
}

// Bridges the ACP session store (useAcpSession) to assistant-ui's external-store
// runtime. assistant-ui renders the thread; we own the data and the actions.

import {
  AssistantRuntimeProvider,
  useExternalStoreRuntime,
  type ExternalStoreAdapter,
  type ThreadMessageLike,
} from "@assistant-ui/react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode, type SetStateAction } from "react";

import { useAcpSession } from "../../hooks/useAcpSession";
import { useHistoryWindow } from "../../hooks/useHistoryWindow";
import {
  clearDraft,
  exclusiveRestore,
  getDraftAttachments,
  notifyAttachmentRestoreFailure,
  setDraftAttachments,
} from "../../lib/acpDrafts";
import { MAX_ATTACHMENTS } from "./composerInput";
import { isVisiblyBusy } from "../../lib/acpTypes";
import type { AcpState, ApprovalDecision, ElicitationResolution, PromptAttachmentInput } from "../../lib/acpTypes";
import { useAgentProfile } from "../../lib/agentProfileContext";
import { canOfferEarlier, earlierAction } from "../../lib/historyScroll";
import { activityToThreadMessages, clearFoldGeneration } from "./activityMessages";
import { useCancelEscalation } from "./useCancelEscalation";

type Session = ReturnType<typeof useAcpSession>;

interface Props {
  sessionId: string;
  /** Parks the queue drain while the reconciler resumes the worker. */
  acpWorkerState?: "absent" | "resuming" | "running" | "stopping";
  /** Snoozed sessions auto-wake on send; archived ones refuse it. */
  archivedAt?: string | null;
  snoozedUntil?: string | null;
  /** Render rows before the latest `/clear` instead of folding them. */
  showClearedTurns?: boolean;
  children: (ctx: AcpContext) => ReactNode;
}

export interface AcpContext {
  state: AcpState;
  status: Session["status"];
  hasEverOpened: boolean;
  /** A replay delivered events while the socket was down. */
  replayedWhileDown: boolean;
  /** The auto-reconnect backoff is armed between a close and the next dial. */
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  maxRetries: number;
  manualReconnect: () => void;
  resolveApproval: (nonce: string, decision: ApprovalDecision, optionId?: string) => Promise<void>;
  resolveElicitation: (nonce: string, resolution: ElicitationResolution) => Promise<void>;
  sendPrompt: (text: string, attachments?: PromptAttachmentInput[]) => Promise<void>;
  /** Staged for the next send; owned here so assistant-ui's `onNew` can send them. */
  pendingAttachments: PromptAttachmentInput[];
  setPendingAttachments: React.Dispatch<React.SetStateAction<PromptAttachmentInput[]>>;
  forceEndTurn: () => Promise<void>;
  lastActivityRef: Session["lastActivityRef"];
  dismissError: () => void;
  dismissPrimer: () => void;
  dismissCompactionReminder: () => void;
  removeQueuedPrompt: (id: string) => void;
  editQueuedPrompt: (id: string, text: string) => void;
  clearQueue: () => void;
  sendQueuedNow: Session["sendQueuedNow"];
  canSendQueuedNow: boolean;
  sendNowInterruptsTurn: boolean;
  dismissRejectedPrompt: (id: string) => void;
  dismissModeSwitchFailed: () => void;
  dismissSessionNotice: (id: string) => void;
  setConfigOption: (configId: string, value: string) => Promise<void>;
  dismissConfigOptionSwitchFailed: () => void;
  /** Older rows exist above the window, loaded or still on the server. */
  canLoadEarlierHistory: boolean;
  /** Reveal loaded older rows first, then fetch the next page. */
  loadEarlierHistory: () => void;
  loadingEarlierHistory: boolean;
}

/** Owns the external store so a new `key` builds a fresh runtime. A `/clear`
 *  shortens a kept message's parts, and assistant-ui's store would otherwise read
 *  a stale part index and crash the view (assistant-ui#5708). */
function RuntimeHost({ adapter, children }: { adapter: ExternalStoreAdapter<ThreadMessageLike>; children: ReactNode }) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>(adapter);
  return <AssistantRuntimeProvider runtime={runtime}>{children}</AssistantRuntimeProvider>;
}

/** Staged attachments persisted per session. The ref is the source of truth for `onNew`
 *  and for functional updates, so persistence happens in the setter instead of an effect. */
function usePendingAttachments(sessionId: string) {
  const [pendingAttachments, setState] = useState<PromptAttachmentInput[]>([]);
  const pendingAttachmentsRef = useRef<PromptAttachmentInput[]>(pendingAttachments);
  // The view remounts per session. Writes wait for the async restore so the empty initial
  // state cannot delete the stored draft.
  const hydratedRef = useRef(false);
  const setPendingAttachments = useCallback(
    (action: SetStateAction<PromptAttachmentInput[]>) => {
      const next = typeof action === "function" ? action(pendingAttachmentsRef.current) : action;
      pendingAttachmentsRef.current = next;
      setState(next);
      if (hydratedRef.current) void setDraftAttachments(sessionId, next);
    },
    [sessionId],
  );
  useEffect(() => {
    let cancelled = false;
    void exclusiveRestore(sessionId, async () => {
      const stored = await getDraftAttachments(sessionId);
      // A failed read leaves the runtime unhydrated, so a staged attachment cannot overwrite
      // the draft that may still be intact in storage.
      if (stored === null) {
        if (!cancelled) notifyAttachmentRestoreFailure(sessionId);
        return;
      }
      // Anything staged while the read was in flight goes after the restored set, and near the
      // cap the older stored files yield.
      const staged = pendingAttachmentsRef.current;
      const merged =
        staged.length === 0 ? stored : stored.slice(0, Math.max(0, MAX_ATTACHMENTS - staged.length)).concat(staged);
      if (cancelled) {
        // Navigated away mid-read: still persist what was staged, but leave the shared state alone.
        if (staged.length > 0) void setDraftAttachments(sessionId, merged);
        return;
      }
      hydratedRef.current = true;
      pendingAttachmentsRef.current = merged;
      setState(merged);
      // Without staged files the merge is exactly what storage already holds.
      if (staged.length > 0) void setDraftAttachments(sessionId, merged);
    });
    return () => {
      cancelled = true;
    };
  }, [sessionId]);
  return { pendingAttachments, setPendingAttachments, pendingAttachmentsRef };
}

export function AcpRuntime({
  sessionId,
  acpWorkerState = "running",
  archivedAt = null,
  snoozedUntil = null,
  showClearedTurns = false,
  children,
}: Props) {
  const acp = useAcpSession(sessionId, acpWorkerState, archivedAt, snoozedUntil);
  const agentProfile = useAgentProfile();
  const { pendingAttachments, setPendingAttachments, pendingAttachmentsRef } = usePendingAttachments(sessionId);
  const onCancel = useCancelEscalation(
    sessionId,
    acp.state.promptSeq,
    acp.state.cancelling,
    acp.cancelPrompt,
    acp.forceEndTurn,
  );
  // Only the recent slice renders, so a long session does not block first paint.
  const { windowedActivity, canLoadEarlier, loadEarlier } = useHistoryWindow(
    sessionId,
    acp.state.activity,
    showClearedTurns,
  );
  const { loadOlder, hasMoreOlder, loadingOlder } = acp;
  const loadEarlierHistory = useCallback(() => {
    const action = earlierAction(canLoadEarlier, hasMoreOlder);
    if (action === "reveal") loadEarlier();
    else if (action === "fetch") void loadOlder();
  }, [canLoadEarlier, hasMoreOlder, loadEarlier, loadOlder]);

  // Optimistic rows append until their server row (by id, in the full activity) lands.
  const displayActivity = useMemo(() => {
    const optimistic = acp.state.optimisticRows;
    if (optimistic.length === 0) return windowedActivity;
    const serverIds = new Set(acp.state.activity.map((r) => r.id));
    const pending = optimistic.filter((o) => !serverIds.has(o.id));
    return pending.length > 0 ? windowedActivity.concat(pending) : windowedActivity;
  }, [windowedActivity, acp.state.optimisticRows, acp.state.activity]);

  const visiblyBusy = isVisiblyBusy(acp.state);
  const messages = useMemo(
    () =>
      activityToThreadMessages(
        displayActivity,
        visiblyBusy,
        showClearedTurns,
        agentProfile.capabilities.todos,
        agentProfile,
      ),
    [displayActivity, visiblyBusy, showClearedTurns, agentProfile],
  );
  const foldGeneration = useMemo(
    () => clearFoldGeneration(displayActivity, showClearedTurns),
    [displayActivity, showClearedTurns],
  );

  const adapter: ExternalStoreAdapter<ThreadMessageLike> = {
    messages,
    // NOT visiblyBusy: assistant-ui's own ComposerInput swallows Enter
    // outright when isRunning is true and the adapter has no queue
    // capability (`if (threadState.isRunning && !hasQueue) return;`), so this
    // has to track only the main turn, exactly like Composer.tsx's turnActive
    // gate, or a background sub-agent with an idle main turn silently eats
    // every keystroke.
    isRunning: acp.state.turnActive,
    convertMessage: (m) => m,
    // The idle Enter path: text comes from the message, attachments from our staging.
    onNew: async (msg) => {
      const text = msg.content
        .map((c) => (c.type === "text" ? c.text : ""))
        .join("")
        .trim();
      const attachments = [...pendingAttachmentsRef.current];
      if (!text && attachments.length === 0) return;
      // Clear drafts before awaiting: an unmount mid-send must not rehydrate what was sent.
      clearDraft(sessionId);
      setPendingAttachments([]);
      await acp.sendPrompt(text, attachments);
    },
    onCancel,
  };

  return (
    <RuntimeHost key={foldGeneration} adapter={adapter}>
      {children({
        state: acp.state,
        status: acp.status,
        hasEverOpened: acp.hasEverOpened,
        replayedWhileDown: acp.replayedWhileDown,
        reconnecting: acp.reconnecting,
        retryCount: acp.retryCount,
        retryCountdown: acp.retryCountdown,
        maxRetries: acp.maxRetries,
        manualReconnect: acp.manualReconnect,
        resolveApproval: acp.resolveApproval,
        resolveElicitation: acp.resolveElicitation,
        sendPrompt: acp.sendPrompt,
        pendingAttachments,
        setPendingAttachments,
        forceEndTurn: acp.forceEndTurn,
        lastActivityRef: acp.lastActivityRef,
        dismissError: acp.dismissError,
        dismissPrimer: acp.dismissPrimer,
        dismissCompactionReminder: acp.dismissCompactionReminder,
        removeQueuedPrompt: acp.removeQueuedPrompt,
        editQueuedPrompt: acp.editQueuedPrompt,
        clearQueue: acp.clearQueue,
        sendQueuedNow: acp.sendQueuedNow,
        canSendQueuedNow: acp.canSendQueuedNow,
        sendNowInterruptsTurn: acp.sendNowInterruptsTurn,
        dismissRejectedPrompt: acp.dismissRejectedPrompt,
        dismissModeSwitchFailed: acp.dismissModeSwitchFailed,
        dismissSessionNotice: acp.dismissSessionNotice,
        setConfigOption: acp.setConfigOption,
        dismissConfigOptionSwitchFailed: acp.dismissConfigOptionSwitchFailed,
        canLoadEarlierHistory: canOfferEarlier(canLoadEarlier, hasMoreOlder),
        loadEarlierHistory,
        loadingEarlierHistory: loadingOlder,
      })}
    </RuntimeHost>
  );
}

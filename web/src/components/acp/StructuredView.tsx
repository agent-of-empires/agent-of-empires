// Structured view conversation surface. assistant-ui renders the thread shell;
// state lives in AcpRuntime and is only fed to assistant-ui, never owned by it.

import { useState } from "react";
import { ThreadPrimitive } from "@assistant-ui/react";
import { ChevronDown } from "lucide-react";

import { useIsWideViewport } from "../../hooks/useIsWideViewport";
import { useMobileKeyboard } from "../../hooks/useMobileKeyboard";
import { useRespawnSession } from "../../hooks/useRespawnSession";
import { useWebSettings } from "../../hooks/useWebSettings";
import { lastClearIndex } from "../../lib/acpHistoryWindow";
import { AgentProfileProvider } from "../../lib/agentProfileContext";
import { conversationFontSizeRem } from "../../lib/conversationFontSize";
import type { FileRef, FileRefSession } from "../../lib/fileRef";
import { ChromeCollapseHandle, CollapsibleRegion } from "../CollapsibleChrome";
import { AcpFileRefContext } from "./AcpFileRefContext";
import { AcpRuntime, type AcpContext } from "./AcpRuntime";
import { ApprovalCard } from "./ApprovalCard";
import { AskUserQuestionCard } from "./AskUserQuestionCard";
import { AttentionChime } from "./AttentionChime";
import { BackgroundAgentsContext } from "./backgroundAgentsContext";
import { CompactionReminderBanner } from "./CompactionReminderBanner";
import { Composer } from "./Composer";
import { ContextPrimerBanner } from "./ContextPrimerBanner";
import { PlanStrip } from "./PlanStrip";
import { ModeSwitchFailedNotice, QueuedPromptsStrip, RejectedPromptsStrip } from "./PromptStrips";
import { SessionBanners } from "./SessionBanners";
import { ConfigOptionSwitchFailedNotice } from "./SessionConfigControls";
import { StartupErrorScreen } from "./StartupErrorScreen";
import { RateLimitRecoverySection, SystemNotices } from "./SystemNotices";
import { AssistantMessage, UserMessage } from "./ThreadMessages";
import { ToolDensityToggle, ToolDisplayModeProvider, useToolDensityPref } from "./ToolDisplayMode";
import { useTranscriptScroll } from "./useTranscriptScroll";
import { WorkingSpinner } from "./WorkingSpinner";

type WorkerState = "absent" | "resuming" | "running" | "stopping";

interface Props {
  sessionId: string;
  /** `SessionResponse.acp_worker_state`, polled. */
  acpWorkerState: WorkerState;
  /** Whether rate-limit auto-resume is on for this session's profile. */
  rateLimitAutoResume?: boolean;
  /** Session `tool` registry key; selects the AgentProfile. */
  tool: string | null | undefined;
  /** Resolved ACP agent key; the switch-agent modal's fallback before any `AgentSwitched`. */
  acpAgent: string | null;
  /** Server-owned conversation-reset slash aliases (`/clear`, `/new`). */
  clearAliases?: readonly string[];
  archivedAt: string | null;
  snoozedUntil: string | null;
  /** Trashed sessions are read-only: no composer or queue strips. */
  trashedAt: string | null;
  /** Restore the trashed workspace; resolves false on failure. */
  onRestore?: () => Promise<boolean> | void;
  onOpenFileRef?: (ref: FileRef) => void;
  fileRefSession?: FileRefSession | null;
  isSandboxed?: boolean;
  onOpenAgentsPane?: () => void;
}

const STARTER_PROMPTS = [
  "Explain this codebase",
  "Find recent changes worth reviewing",
  "What does the build pipeline do?",
];

export function StructuredView(props: Props) {
  const { sessionId, acpWorkerState, tool, clearAliases, archivedAt, snoozedUntil, onOpenFileRef, fileRefSession } =
    props;
  const [showClearedTurns, setShowClearedTurns] = useState(false);
  const [toolDensity, toggleToolDensity] = useToolDensityPref();
  return (
    <AcpFileRefContext.Provider value={{ onOpenFileRef, fileRefSession }}>
      <AgentProfileProvider toolKey={tool} clearAliases={clearAliases}>
        <ToolDisplayModeProvider density={toolDensity}>
          <AcpRuntime
            sessionId={sessionId}
            acpWorkerState={acpWorkerState}
            archivedAt={archivedAt}
            snoozedUntil={snoozedUntil}
            showClearedTurns={showClearedTurns}
          >
            {(ctx) => (
              <BackgroundAgentsContext.Provider
                value={{ agents: ctx.state.backgroundAgents, openPane: props.onOpenAgentsPane }}
              >
                <AcpChrome
                  view={props}
                  ctx={ctx}
                  showClearedTurns={showClearedTurns}
                  onToggleClearedTurns={() => setShowClearedTurns((v) => !v)}
                  toolDensity={toolDensity}
                  onToggleToolDensity={toggleToolDensity}
                />
              </BackgroundAgentsContext.Provider>
            )}
          </AcpRuntime>
        </ToolDisplayModeProvider>
      </AgentProfileProvider>
    </AcpFileRefContext.Provider>
  );
}

/** Bottom padding reserving the soft keyboard where the layout viewport does
 *  not shrink for it (iOS regular Safari); 0 elsewhere. */
function structuredViewRootStyle(keyboardHeight: number): React.CSSProperties | undefined {
  return keyboardHeight > 0 ? { paddingBottom: keyboardHeight } : undefined;
}

/** Flex root publishing the keyboard reservation and both conversation font
 *  sizes (as rem); `index.css` picks the active size. */
export function StructuredViewRoot({ children }: { children: React.ReactNode }) {
  const { keyboardHeight } = useMobileKeyboard();
  const { settings } = useWebSettings();
  return (
    <div
      data-testid="structured-view-root"
      className="acp-conversation-scope flex h-full flex-col bg-surface-900 text-text-primary"
      style={
        {
          ...structuredViewRootStyle(keyboardHeight),
          "--acp-conversation-font-size-mobile": conversationFontSizeRem(settings.structuredMobileFontSize),
          "--acp-conversation-font-size-desktop": conversationFontSizeRem(settings.structuredDesktopFontSize),
        } as React.CSSProperties
      }
    >
      {children}
    </div>
  );
}

type Prefill = { id: string; text: string } | null;

interface ChromeProps {
  view: Props;
  ctx: AcpContext;
  showClearedTurns: boolean;
  onToggleClearedTurns: () => void;
  toolDensity: "detailed" | "compact";
  onToggleToolDensity: () => void;
}

function AcpChrome({
  view,
  ctx,
  showClearedTurns,
  onToggleClearedTurns,
  toolDensity,
  onToggleToolDensity,
}: ChromeProps) {
  const { sessionId, acpWorkerState, acpAgent } = view;
  const { state, status } = ctx;
  // Rows before the latest `/clear` divider are the hidden history.
  const hiddenCount = lastClearIndex(state.activity);
  const [primerPrefill, setPrimerPrefill] = useState<Prefill>(null);
  // A rate limit with no reported reset still needs a key to scope resume status by.
  const rateLimitResume = useRespawnSession(
    sessionId,
    state.rateLimit ? (state.rateLimit.resets_at ?? "unknown") : null,
  );
  // Phone-width only: fold the composer away for reading.
  const composerCollapsible = !useIsWideViewport();
  const [composerCollapsed, setComposerCollapsed] = useState(false);
  const {
    viewportRef,
    belowViewportRef,
    messagesContentRef,
    atBottom,
    isCoarse,
    scrollToBottom,
    requestEarlierHistory,
  } = useTranscriptScroll({
    sessionId,
    canLoadEarlierHistory: ctx.canLoadEarlierHistory,
    loadEarlierHistory: ctx.loadEarlierHistory,
    loadingEarlierHistory: ctx.loadingEarlierHistory,
    composerCollapsed,
    promptSeq: state.promptSeq,
    hasEverOpened: ctx.hasEverOpened,
    localInflight: state.inflightPromptIds.length > 0,
  });

  // An adapter that failed the compatibility check never runs, so no chat surface.
  if (state.incompatibleAgent) {
    return (
      <div className="flex h-full flex-col bg-surface-900 text-text-primary">
        <StartupErrorScreen detail={state.incompatibleAgent} sessionId={sessionId} isSandboxed={view.isSandboxed} />
      </div>
    );
  }
  return (
    <StructuredViewRoot>
      <AttentionChime approvals={state.pendingApprovals.length} elicitations={state.pendingElicitations.length} />
      <PlanStrip plan={state.plan} />

      <RateLimitRecoverySection
        sessionId={sessionId}
        currentAgent={state.agent ?? acpAgent}
        onPrefill={(text) => setPrimerPrefill({ id: `rate-limit-recovery-${Date.now()}`, text })}
      >
        {({ onSwitchAgent }) =>
          status !== "open" ||
          state.lagged ||
          state.rateLimit ||
          state.rateLimitRetriesExhausted ||
          ctx.reconnecting ? (
            <SystemNotices
              status={status}
              lagged={state.lagged}
              rateLimit={state.rateLimit}
              rateLimitAutoResume={view.rateLimitAutoResume}
              rateLimitRetriesExhausted={state.rateLimitRetriesExhausted}
              hasEverOpened={ctx.hasEverOpened}
              reconnecting={ctx.reconnecting}
              retryCount={ctx.retryCount}
              retryCountdown={ctx.retryCountdown}
              maxRetries={ctx.maxRetries}
              manualReconnect={ctx.manualReconnect}
              onSwitchAgent={onSwitchAgent}
              onResumeRateLimit={() => void rateLimitResume.respawn()}
              rateLimitResumeState={rateLimitResume.state}
              rateLimitResumeError={rateLimitResume.error}
            />
          ) : null
        }
      </RateLimitRecoverySection>

      <SessionBanners
        sessionId={sessionId}
        state={state}
        acpWorkerState={acpWorkerState}
        trashedAt={view.trashedAt}
        archivedAt={view.archivedAt}
        snoozedUntil={view.snoozedUntil}
        onRestore={view.onRestore}
        dismissError={ctx.dismissError}
      />

      <ThreadPrimitive.Root className="flex flex-1 flex-col min-h-0">
        <div className="relative flex min-h-0 flex-1 flex-col">
          <ThreadPrimitive.Viewport
            autoScroll={false}
            ref={viewportRef}
            data-testid="acp-viewport"
            className="flex-1 overflow-x-hidden overflow-y-auto [overflow-anchor:none]"
          >
            <div ref={messagesContentRef} className="mx-auto max-w-3xl xl:max-w-4xl 2xl:max-w-5xl px-4 py-6">
              <ThreadPrimitive.Empty>
                <EmptyState onPick={ctx.sendPrompt} />
              </ThreadPrimitive.Empty>

              {state.activity.length > 0 && (
                <div className="mb-2 flex">
                  <ToolDensityToggle density={toolDensity} onToggle={onToggleToolDensity} />
                </div>
              )}

              {hiddenCount > 0 && (
                <ClearedTurnsBanner
                  hiddenCount={hiddenCount}
                  expanded={showClearedTurns}
                  onToggle={onToggleClearedTurns}
                />
              )}

              {ctx.canLoadEarlierHistory && (
                <div className="mb-3 flex justify-center">
                  <button
                    type="button"
                    onClick={requestEarlierHistory}
                    disabled={ctx.loadingEarlierHistory}
                    data-testid="acp-load-earlier"
                    className="h-8 rounded-md border border-surface-700 bg-surface-800 px-3 text-xs text-text-secondary hover:bg-surface-700 hover:text-text-primary transition-colors cursor-pointer disabled:cursor-default disabled:opacity-60"
                  >
                    {ctx.loadingEarlierHistory ? "Loading…" : "Load earlier messages"}
                  </button>
                </div>
              )}

              <ThreadPrimitive.Messages components={{ UserMessage, AssistantMessage }} />

              <ThreadPrimitive.If running>
                {/* A turn parked on an approval or question is waiting on the user, not stalled. */}
                {state.pendingElicitations.length === 0 && state.pendingApprovals.length === 0 ? (
                  <div className="mt-3 ml-1">
                    <WorkingSpinner
                      thinking={state.thinking}
                      tool={state.inFlightTool?.name ?? null}
                      cancelling={state.cancelling}
                      cancelEscalatesAt={state.cancelEscalatesAt}
                      compacting={state.compacting}
                      lastActivityRef={ctx.lastActivityRef}
                      onForceEndTurn={ctx.forceEndTurn}
                    />
                  </div>
                ) : null}
              </ThreadPrimitive.If>

              {state.pendingApprovals.map((approval) => (
                <ApprovalCard
                  key={approval.nonce}
                  approval={approval}
                  onResolve={(decision, optionId) => ctx.resolveApproval(approval.nonce, decision, optionId)}
                />
              ))}

              {state.pendingElicitations.map((elicitation) => (
                <AskUserQuestionCard
                  key={elicitation.nonce}
                  elicitation={elicitation}
                  onResolve={(resolution) => ctx.resolveElicitation(elicitation.nonce, resolution)}
                />
              ))}
            </div>
          </ThreadPrimitive.Viewport>
          {isCoarse && !atBottom && (
            <button
              type="button"
              onClick={scrollToBottom}
              data-testid="acp-jump-to-bottom"
              aria-label="Jump to latest"
              title="Jump to latest"
              // Centered: the composer collapse handle owns the bottom-right corner.
              className="absolute bottom-3 left-1/2 z-10 inline-flex h-9 w-9 -translate-x-1/2 items-center justify-center rounded-full border border-surface-700 bg-surface-800/95 text-text-secondary shadow-lg backdrop-blur transition-colors hover:text-text-primary active:scale-95"
            >
              <ChevronDown className="h-5 w-5" />
            </button>
          )}
        </div>

        {/* Always mounted: the scroll observers need it even for a read-only trashed session. */}
        <div ref={belowViewportRef}>
          {!view.trashedAt && (
            <ComposerDock
              view={view}
              ctx={ctx}
              primerPrefill={primerPrefill}
              setPrimerPrefill={setPrimerPrefill}
              collapsible={composerCollapsible}
              collapsed={composerCollapsed}
              onToggleCollapsed={() => setComposerCollapsed((v) => !v)}
            />
          )}
        </div>
      </ThreadPrimitive.Root>
    </StructuredViewRoot>
  );
}

/** Queue and failure strips plus the composer. Only the composer collapses:
 *  the strips carry actionable state. */
function ComposerDock({
  view,
  ctx,
  primerPrefill,
  setPrimerPrefill,
  collapsible,
  collapsed,
  onToggleCollapsed,
}: {
  view: Props;
  ctx: AcpContext;
  primerPrefill: Prefill;
  setPrimerPrefill: (prefill: Prefill) => void;
  collapsible: boolean;
  collapsed: boolean;
  onToggleCollapsed: () => void;
}) {
  const { sessionId, acpWorkerState, acpAgent } = view;
  const { state, status } = ctx;
  return (
    <>
      <QueuedPromptsStrip
        queued={state.queuedPrompts}
        onRemove={ctx.removeQueuedPrompt}
        onEdit={ctx.editQueuedPrompt}
        onClear={ctx.clearQueue}
        onSendNow={ctx.sendQueuedNow}
        canSendNow={ctx.canSendQueuedNow}
        sendNowInterrupts={ctx.sendNowInterruptsTurn}
        pendingResume={
          status !== "open" || acpWorkerState !== "running" || state.workerStopped || state.workerRestarting
        }
      />

      <RejectedPromptsStrip
        rejected={state.rejectedPrompts}
        onRetry={ctx.sendPrompt}
        onDismiss={ctx.dismissRejectedPrompt}
        disabled={state.workerRestarting || state.workerStopped || Boolean(state.startupError)}
      />

      <ModeSwitchFailedNotice failure={state.modeSwitchFailed} onDismiss={ctx.dismissModeSwitchFailed} />

      <ConfigOptionSwitchFailedNotice
        failure={state.configOptionSwitchFailed}
        configOptions={state.configOptions}
        onDismiss={ctx.dismissConfigOptionSwitchFailed}
      />

      <ContextPrimerBanner
        sessionId={sessionId}
        available={state.contextPrimerAvailable}
        onInsertPrimer={(text) =>
          setPrimerPrefill({ id: `primer-${state.contextPrimerAvailable?.resetSeq ?? 0}-${Date.now()}`, text })
        }
        onDismiss={ctx.dismissPrimer}
      />

      <CompactionReminderBanner
        state={state}
        onCompact={() => ctx.sendPrompt("/compact")}
        onDismiss={ctx.dismissCompactionReminder}
      />

      {collapsible && (
        <ChromeCollapseHandle
          edge="bottom"
          collapsed={collapsed}
          onToggle={onToggleCollapsed}
          collapseLabel="Collapse message composer"
          expandLabel="Expand message composer"
          controlsId="conversation-composer"
          testId="composer-collapse-toggle"
        />
      )}

      <CollapsibleRegion id="conversation-composer" collapsed={collapsible && collapsed}>
        <Composer
          sessionId={sessionId}
          currentAgent={state.agent ?? acpAgent}
          availableModes={state.availableModes}
          currentModeId={state.currentModeId}
          legacyMode={state.mode}
          configOptions={state.configOptions}
          pendingConfigOption={state.pendingConfigOption}
          setConfigOption={ctx.setConfigOption}
          sessionUsage={state.sessionUsage}
          availableCommands={state.availableCommands}
          connected={status === "open" && !state.workerStopped && !state.workerRestarting}
          turnActive={state.turnActive}
          enqueuePrompt={ctx.sendPrompt}
          promptCapabilities={state.promptCapabilities}
          pendingAttachments={ctx.pendingAttachments}
          setPendingAttachments={ctx.setPendingAttachments}
          primerPrefill={primerPrefill}
          queuedPrompts={state.queuedPrompts}
          editQueuedPrompt={ctx.editQueuedPrompt}
        />
      </CollapsibleRegion>
    </>
  );
}

function EmptyState({ onPick }: { onPick: (text: string) => Promise<void> }) {
  return (
    <div className="mt-12 flex flex-col items-center gap-4 text-center">
      <div className="text-sm text-text-muted">Ask the agent anything about this workspace.</div>
      <div className="flex flex-wrap justify-center gap-2">
        {STARTER_PROMPTS.map((p) => (
          <button
            key={p}
            type="button"
            onClick={() => void onPick(p)}
            className="rounded-full border border-surface-700 bg-surface-800/60 px-3 py-1 text-xs text-text-secondary hover:border-brand-600/60 hover:bg-surface-800 hover:text-text-primary"
          >
            {p}
          </button>
        ))}
      </div>
    </div>
  );
}

function ClearedTurnsBanner({
  hiddenCount,
  expanded,
  onToggle,
}: {
  hiddenCount: number;
  expanded: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onToggle}
      className="mb-4 w-full flex items-center gap-2 px-3 py-2 rounded-md border border-surface-700 bg-surface-800 text-text-secondary hover:bg-surface-700 hover:text-text-primary cursor-pointer text-sm"
      aria-expanded={expanded}
    >
      <ChevronDown
        size={14}
        className={`shrink-0 transition-transform ${expanded ? "" : "-rotate-90"}`}
        aria-hidden="true"
      />
      <span className="flex-1 text-left">
        {expanded ? "Hide" : "Show"} {hiddenCount} earlier turn
        {hiddenCount === 1 ? "" : "s"}
        <span className="text-text-dim"> (cleared, not in the model's memory)</span>
      </span>
    </button>
  );
}

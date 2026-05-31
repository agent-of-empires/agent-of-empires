// VSCode/Cursor-style composer for the cockpit.
//
// Built on assistant-ui's `<ComposerPrimitive.Root>` plus the official
// `Unstable_TriggerPopover` family for `@` mentions and `/` slash
// commands. We provide TriggerAdapters that feed categories/items
// from our own state (the workspace file listing for `@`, a static
// command list for `/`).
//
// Icons via lucide-react.

import {
  ComposerPrimitive,
  useComposerRuntime,
  useThreadRuntime,
} from "@assistant-ui/react";
import {
  unstable_defaultDirectiveFormatter as defaultDirectiveFormatter,
  type Unstable_TriggerAdapter,
  type Unstable_TriggerItem,
} from "@assistant-ui/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowLeftRight,
  AtSign,
  ChevronUp,
  Paperclip,
  Slash,
  Square,
  X,
} from "lucide-react";

import { useFilesIndex, fuzzyFilter } from "./useFilesIndex";
import { SessionConfigControls } from "./SessionConfigControls";
import { SwitchAgentModal } from "./SwitchAgentModal";
import type {
  CockpitState,
  PromptAttachmentInput,
  PromptAttachmentKind,
  PromptCapabilities,
} from "../../lib/cockpitTypes";
import { getDraft, setDraft } from "../../lib/cockpitDrafts";
import { TOUR_ANCHORS, tourAnchor } from "../../lib/tourSteps";
import { useMobileKeyboard } from "../../hooks/useMobileKeyboard";
import { useAgentProfile } from "../../lib/agentProfileContext";
import { useFocusTerminalTarget } from "../../hooks/useFocusTerminalTarget";
import { useDictationBurstGuard } from "./useDictationBurstGuard";

export {
  DICTATION_BURST_TIMEOUT_MS,
  decideDictationAction,
  type DictationBurstState,
  type DictationDecision,
  type DictationEvent,
} from "./useDictationBurstGuard";

/** Decision returned by {@link decideEnterAction} for an Enter
 *  keystroke on the cockpit composer textarea.
 *  - `newline`: insert a newline natively, suppress the primitive's
 *    Send. Used on touch-primary devices so multi-line drafting
 *    matches WhatsApp / Slack / ChatGPT mobile conventions (#1129).
 *  - `send`: dispatch via our custom send path; covers the
 *    mid-turn queue branch (#1031) where ComposerPrimitive.Input
 *    hard-blocks Enter on its own.
 *  - `default`: let the primitive run its built-in keymap (desktop
 *    Enter-to-send, Shift+Enter for newline, etc.). */
export type EnterAction = "newline" | "send" | "default";

/** Pure decision helper for the composer's Enter keystroke. Extracted
 *  so the decision matrix can be unit-tested without mounting the
 *  whole composer + assistant-ui runtime. The textarea handler reads
 *  the same matrix at runtime. See #1129. */
export function decideEnterAction(
  event: {
    key: string;
    shiftKey: boolean;
    ctrlKey: boolean;
    metaKey: boolean;
    isComposing: boolean;
  },
  ctx: { isMobile: boolean; turnActive: boolean },
): EnterAction {
  if (event.key !== "Enter") return "default";
  if (event.isComposing) return "default";
  if (event.shiftKey || event.ctrlKey || event.metaKey) return "default";
  if (ctx.isMobile) return "newline";
  if (ctx.turnActive) return "send";
  return "default";
}

/** Decision returned by {@link decideBeforeInputAction} for a
 *  `beforeinput` event on the cockpit composer textarea.
 *  - `newline`: caller should preventDefault, stop propagation, and
 *    manually insert a literal "\n" at the caret. Used on touch-primary
 *    devices where the on-screen keyboard's Enter fires
 *    `beforeinput` with `insertLineBreak` / `insertParagraph` instead
 *    of a `keydown` (Android Chrome's GBoard / Samsung Keyboard / many
 *    others). Without this, assistant-ui's bubble-phase Send wins.
 *  - `default`: do nothing, let the primitive run. */
export type BeforeInputAction = "newline" | "default";

/** Pure decision helper for the composer's `beforeinput` event.
 *  Extracted so the matrix is unit-testable without mounting the
 *  composer. The textarea handler reads the same matrix at runtime.
 *  See #1174. */
export function decideBeforeInputAction(
  inputType: string,
  isComposing: boolean,
  ctx: { isMobile: boolean },
): BeforeInputAction {
  if (!ctx.isMobile) return "default";
  if (isComposing) return "default";
  if (inputType !== "insertLineBreak" && inputType !== "insertParagraph") {
    return "default";
  }
  return "newline";
}

/** Wrapper class + inline style for the composer's outer <div>. When the
 *  soft keyboard is open we drop the bottom padding and apply a negative
 *  bottom margin equal to the App root's safe-area-inset-bottom so the
 *  composer sits flush with the top of the keyboard instead of leaving a
 *  visible gap (the home-indicator inset is physically occluded by the
 *  keyboard anyway). Extracted as a pure helper so the layout decision
 *  can be unit-tested without mounting the whole composer. See #1143. */
export function composerWrapperLayout(opts: { keyboardOpen: boolean }): {
  className: string;
  style: React.CSSProperties | undefined;
} {
  return {
    className: [
      "border-t border-surface-800 bg-surface-900 px-4 pt-3",
      opts.keyboardOpen ? "pb-0" : "pb-3",
    ].join(" "),
    style: opts.keyboardOpen
      ? { marginBottom: "calc(-1 * env(safe-area-inset-bottom))" }
      : undefined,
  };
}

/** True when the current device is touch-primary AND no precise
 *  pointer (mouse / trackpad / stylus tip) is also attached. An iPad
 *  with a Bluetooth keyboard + Magic Keyboard trackpad reports both
 *  `(pointer: coarse)` (touchscreen) and `(any-pointer: fine)`
 *  (trackpad); treating that as desktop preserves Enter-to-send for
 *  hardware-keyboard typing. See #1129 open questions. */
function detectMobileInput(): boolean {
  if (typeof window === "undefined" || !window.matchMedia) return false;
  const coarse = window.matchMedia("(pointer: coarse)").matches;
  const anyFine = window.matchMedia("(any-pointer: fine)").matches;
  return coarse && !anyFine;
}

/** Client-side mirror of the server's per-prompt attachment cap. */
const MAX_ATTACHMENTS = 8;

/** Map a file's MIME type to the ACP attachment kind. */
function mimeToKind(mime: string): PromptAttachmentKind {
  if (mime.startsWith("image/")) return "image";
  if (mime.startsWith("audio/")) return "audio";
  return "resource";
}

/** Whether the current agent accepts the given attachment kind. */
function kindSupported(
  kind: PromptAttachmentKind,
  caps: PromptCapabilities | null,
): boolean {
  if (!caps) return false;
  if (kind === "image") return caps.image;
  if (kind === "audio") return caps.audio;
  return caps.embeddedContext;
}

/** The `accept` attribute for the file picker, narrowed to the kinds
 *  the agent advertises so the OS dialog only offers usable files. */
function acceptForCaps(caps: PromptCapabilities | null): string {
  const parts: string[] = [];
  if (caps?.image) parts.push("image/*");
  if (caps?.audio) parts.push("audio/*");
  if (caps?.embeddedContext) parts.push(".txt,.md,.json,.pdf");
  return parts.join(",");
}

/** Read a File into standard base64 (no `data:` prefix). */
function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error("read failed"));
    reader.onload = () => {
      const result = typeof reader.result === "string" ? reader.result : "";
      // `data:<mime>;base64,<b64>` → keep only the base64 tail.
      resolve(result.slice(result.indexOf(",") + 1));
    };
    reader.readAsDataURL(file);
  });
}

interface Props {
  sessionId: string;
  /** Registry key of the agent the session currently runs. Drives the
   *  "Switch agent" control's filtered target list and handoff copy. */
  currentAgent: CockpitState["agent"];
  availableModes: CockpitState["availableModes"];
  currentModeId: CockpitState["currentModeId"];
  /** Legacy enum-based mode used as fallback when the agent does not
   *  advertise modes via NewSessionResponse. */
  legacyMode: CockpitState["mode"];
  /** Per-session selectors advertised by the adapter (model,
   *  reasoning effort, future categories). Empty when the adapter
   *  does not emit `ConfigOptionUpdate`. See #1403. */
  configOptions: CockpitState["configOptions"];
  /** In-flight config-option click; drives the pending affordance
   *  on the just-clicked option. */
  pendingConfigOption: CockpitState["pendingConfigOption"];
  /** Send `session/set_config_option` for the given pair. */
  setConfigOption: (
    configId: string,
    value: string,
  ) => void | Promise<void>;
  /** Latest agent-reported context-window usage. Null until the agent
   *  has emitted at least one ACP `UsageUpdate`. */
  sessionUsage: CockpitState["sessionUsage"];
  /** Slash commands the agent advertised in its most recent
   *  AvailableCommandsUpdate. Includes plugins/skills/MCP commands.
   *  Empty until the agent emits the first list. */
  availableCommands: CockpitState["availableCommands"];
  /** True when the cockpit WS is open and the worker is healthy
   *  (running, not stopped, not restarting). When false the Send /
   *  QueueSend buttons stay clickable, but submissions take the
   *  enqueue path in `sendPrompt` so they fire on resume rather than
   *  POSTing into a non-running session. The tooltip swaps to name
   *  the queue behavior so users understand the click is not lost.
   *  See #1359. */
  connected: boolean;
  /** True while the agent is producing the current turn. When true the
   *  composer keeps its textarea editable and surfaces a queue-send
   *  button alongside Stop; sends go through the client-side queue (see
   *  #1031). When false the regular ComposerPrimitive.Send path runs as
   *  before. */
  turnActive: boolean;
  /** Number of items already enqueued for after the current turn.
   *  Drives the badge on the queue-send button. */
  queuedCount: number;
  /** Push the composer text straight onto the cockpit queue. Bypasses
   *  the ComposerPrimitive.Send path (which assistant-ui hard-disables
   *  while `thread.isRunning && !capabilities.queue`). Used by the
   *  mid-turn Send button + the Enter-while-running handler. */
  enqueuePrompt: (
    text: string,
    attachments?: PromptAttachmentInput[],
  ) => void | Promise<void>;
  /** Attachment kinds the current agent accepts, gating the paperclip
   *  / paste / drop affordances. Null until the handshake reports it.
   *  See #1000 / #965. */
  promptCapabilities: PromptCapabilities | null;
  /** Attachments staged for the next send, owned by CockpitRuntime so
   *  the assistant-ui Enter / Send path can read them on submit. */
  pendingAttachments: PromptAttachmentInput[];
  setPendingAttachments: React.Dispatch<
    React.SetStateAction<PromptAttachmentInput[]>
  >;
  /** When set, replace the current composer text with `text` and
   *  focus the textarea (cursor at end). Used by the context-primer
   *  banner to prefill a transcript recap before send. The `id` is
   *  a fresh nonce per insertion so the effect re-fires even when
   *  the same text is inserted twice. See #1004. */
  primerPrefill?: { id: string; text: string } | null;
}

export function Composer({
  sessionId,
  currentAgent,
  availableModes,
  currentModeId,
  legacyMode,
  configOptions,
  pendingConfigOption,
  setConfigOption,
  sessionUsage,
  availableCommands,
  connected,
  turnActive,
  queuedCount,
  enqueuePrompt,
  promptCapabilities,
  pendingAttachments,
  setPendingAttachments,
  primerPrefill,
}: Props) {
  const taRef = useRef<HTMLTextAreaElement | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const { files } = useFilesIndex(sessionId);

  const attachmentsEnabled =
    !!promptCapabilities &&
    (promptCapabilities.image ||
      promptCapabilities.audio ||
      promptCapabilities.embeddedContext);

  // Stage files dropped, pasted, or picked. Filters to kinds the agent
  // accepts and respects the per-prompt count cap; oversize / type
  // rejection beyond this is enforced authoritatively server-side.
  const addFiles = useCallback(
    async (files: FileList | File[]) => {
      const list = Array.from(files);
      const accepted: PromptAttachmentInput[] = [];
      for (const file of list) {
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
      setPendingAttachments((prev) =>
        prev.concat(accepted).slice(0, MAX_ATTACHMENTS),
      );
    },
    [promptCapabilities, setPendingAttachments],
  );

  const removeAttachment = useCallback(
    (index: number) => {
      setPendingAttachments((prev) => prev.filter((_, i) => i !== index));
    },
    [setPendingAttachments],
  );

  // When the soft keyboard is up the App root's safe-area-inset-bottom
  // padding reserves space for the iOS home indicator that the keyboard
  // already physically occludes, leaving a visible gap between the
  // composer and the top of the keyboard. Cancel that reservation and
  // drop our own bottom padding while the keyboard is open. See #1143.
  const { keyboardOpen } = useMobileKeyboard();

  // Touch-primary device flag for the Enter-key decision matrix.
  // Re-evaluated on `(pointer: coarse)` / `(any-pointer: fine)`
  // changes so plugging in a Bluetooth keyboard on an iPad flips
  // behavior live without a refresh. See #1129.
  const [isMobile, setIsMobile] = useState<boolean>(() => detectMobileInput());
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const coarseMql = window.matchMedia("(pointer: coarse)");
    const fineMql = window.matchMedia("(any-pointer: fine)");
    const onChange = () => setIsMobile(detectMobileInput());
    coarseMql.addEventListener("change", onChange);
    fineMql.addEventListener("change", onChange);
    return () => {
      coarseMql.removeEventListener("change", onChange);
      fineMql.removeEventListener("change", onChange);
    };
  }, []);

  // Adapter for the @ file picker. We deliberately skip the
  // category step (return []) so the popover lands directly in
  // search-results mode — the resource short-circuits to
  // adapter.search() when there are no categories. That gives us a
  // single-pane file list instead of a "Files" category drill-down.
  const fileAdapter: Unstable_TriggerAdapter = useMemo(
    () => ({
      categories: () => [],
      categoryItems: () => [],
      search: (query) => {
        const items = files.map((path) => ({
          id: path,
          type: "file",
          label: path,
          description: extDescription(path),
        }));
        return fuzzyFilter(items, query, 30);
      },
    }),
    [files],
  );

  // Slash commands: built from the agent's AvailableCommandsUpdate, plus
  // any profile-declared clear aliases the agent doesn't advertise
  // itself (codex / opencode emit `/new` as a UI affordance but their
  // ACP servers don't list it in `available_commands_update`, so the
  // palette would otherwise be missing the very command we detect
  // server-side as a session-clear boundary). See #1133 + multi-agent
  // parity follow-up.
  const profile = useAgentProfile();
  const slashItems: Unstable_TriggerItem[] = useMemo(() => {
    const advertised = new Set(availableCommands.map((c) => c.name));
    const items: Unstable_TriggerItem[] = availableCommands.map((c) => ({
      id: c.name,
      type: "command",
      label: `/${c.name}`,
      description: c.description,
      acceptsInput: c.accepts_input,
    }));
    for (const alias of profile.clearAliases ?? []) {
      const name = alias.startsWith("/") ? alias.slice(1) : alias;
      if (!name || advertised.has(name)) continue;
      const item = {
        id: name,
        type: "command" as const,
        label: `/${name}`,
        description: "clear conversation",
        acceptsInput: false,
      } as Unstable_TriggerItem;
      items.push(item);
      advertised.add(name);
    }
    return items;
  }, [availableCommands, profile]);
  const slashAdapter: Unstable_TriggerAdapter = useMemo(
    () => ({
      categories: () => [],
      categoryItems: () => [],
      search: (query) => fuzzyFilter(slashItems, query, 30),
    }),
    [slashItems],
  );

  const composerRuntime = useComposerRuntime();

  // Unified submit for the custom Send / QueueSend buttons and the
  // mid-turn Enter path: drains the textarea text + staged attachments
  // through `enqueuePrompt` (which is the cockpit `sendPrompt`), then
  // clears both. The idle assistant-ui Enter path submits via the
  // runtime's `onNew`, which reads the same staged attachments from
  // CockpitRuntime. See #1000 / #965.
  const submitComposer = useCallback(() => {
    void sendFromTextarea(
      taRef,
      composerRuntime,
      enqueuePrompt,
      pendingAttachments,
      () => setPendingAttachments([]),
    );
  }, [composerRuntime, enqueuePrompt, pendingAttachments, setPendingAttachments]);

  // Manual agent switch dialog, opened from the toolbar control. Unlike
  // the rate-limit recovery path (which lives up in CockpitView), this
  // is available at any time so a user can hand back to, say, claude
  // after a rate-limit handoff to codex.
  const [switchAgentOpen, setSwitchAgentOpen] = useState(false);

  // iOS Safari native dictation (#1431): WebKit fires `beforeinput` /
  // `input` with `inputType: "insertReplacementText"` per partial
  // recognition and tracks a private range pointer into the textarea
  // that is invalidated by any controlled-value re-render. The guard
  // suspends assistant-ui's `setText` flush for the burst, buffers the
  // textarea value, and drains it back into the runtime once the burst
  // ends (1200 ms timeout, blur, or non-replacement input).
  const dictationGuard = useDictationBurstGuard((text) => {
    composerRuntime.setText(text);
  });

  // Context-primer prefill: when the parent passes a `primerPrefill`
  // payload (after the user clicked "Resume with prior context" on the
  // banner), replace the composer text with the primer + focus the
  // textarea + position the cursor at the end. Keyed on `id` so the
  // effect re-fires for repeat insertions, but not for unrelated
  // parent re-renders that recreate the wrapping object. See #1004.
  const primerId = primerPrefill?.id ?? null;
  const primerText = primerPrefill?.text ?? null;
  useEffect(() => {
    if (!primerId || primerText == null) return;
    composerRuntime.setText(primerText);
    requestAnimationFrame(() => {
      const el = taRef.current;
      if (!el) return;
      el.focus();
      const len = el.value.length;
      try {
        el.setSelectionRange(len, len);
      } catch {
        // ignore: non-text inputs can throw here
      }
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
    });
    // primerText is intentionally a captured snapshot read via the
    // ref above; we don't want this effect to re-fire on a text-only
    // change (only id changes count as a new prefill action).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [composerRuntime, primerId]);

  // Auto-grow the textarea up to ~6 visible lines.
  const onInput = (e: React.FormEvent<HTMLTextAreaElement>) => {
    const el = e.currentTarget;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  };

  // Per-session draft persistence: keep an unsent prompt across
  // sidebar navigation / route changes by mirroring composer text into
  // localStorage. The CockpitView unmounts when the user switches to
  // another session, so without this the draft is gone on return.
  // Keyed by sessionId; cleared when the text goes empty (user deleted
  // it, or the runtime cleared after a successful send).
  useEffect(() => {
    const saved = getDraft(sessionId);
    if (saved && composerRuntime.getState().text === "") {
      composerRuntime.setText(saved);
      // setText doesn't fire the textarea's onInput, so the auto-grow
      // never runs for the restored value. Resize manually once the DOM
      // has the seeded text.
      requestAnimationFrame(() => {
        const el = taRef.current;
        if (el) {
          el.style.height = "auto";
          el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
        }
      });
    }

    let writeTimer: number | null = null;
    const flush = () => {
      if (writeTimer !== null) {
        window.clearTimeout(writeTimer);
        writeTimer = null;
      }
      setDraft(sessionId, composerRuntime.getState().text);
    };
    const unsub = composerRuntime.subscribe(() => {
      if (writeTimer !== null) window.clearTimeout(writeTimer);
      writeTimer = window.setTimeout(flush, 250);
    });
    // Page-unload flush. Effect cleanup runs on React unmount (sidebar
    // navigation) but not on a full reload, PWA cold start, or mobile
    // OS evicting the tab. Without these listeners, whatever sits in
    // writeTimer at the moment the page dies is lost; on a fast typer
    // that's the last sentence or two of the draft (#1358).
    // visibilitychange covers iOS Safari, which fires pagehide only on
    // real unload, not on app-switch.
    const onHidden = () => {
      if (document.visibilityState === "hidden") flush();
    };
    window.addEventListener("beforeunload", flush);
    window.addEventListener("pagehide", flush);
    document.addEventListener("visibilitychange", onHidden);
    return () => {
      unsub();
      window.removeEventListener("beforeunload", flush);
      window.removeEventListener("pagehide", flush);
      document.removeEventListener("visibilitychange", onHidden);
      flush();
    };
  }, [composerRuntime, sessionId]);

  // xterm.js's autoFocus path in the right pane focuses its hidden
  // textarea ~200-500ms after mount and steals focus from us. Re-claim
  // a couple of times so the agent input wins; only when focus is on
  // body or inside .xterm so an intentional click into the host shell
  // sticks.
  //
  // Mobile skips this entirely (#1178): auto-focusing the textarea pops
  // the soft keyboard on every session open / switch, which is the wrong
  // default for the read-traffic that dominates mobile usage. Users tap
  // the composer when they want to type.
  useEffect(() => {
    if (isMobile) return;
    const el = taRef.current;
    if (!el) return;
    el.focus();
    const reclaim = () => {
      const active = document.activeElement as HTMLElement | null;
      if (!active || active === document.body || active === el) {
        el.focus();
        return;
      }
      if (active.closest?.(".xterm")) {
        el.focus();
      }
    };
    const t1 = window.setTimeout(reclaim, 250);
    const t2 = window.setTimeout(reclaim, 700);
    return () => {
      window.clearTimeout(t1);
      window.clearTimeout(t2);
    };
  }, [isMobile]);

  // Explicit focus from sidebar session selection (#1454). Lands focus on
  // the composer even when it is already mounted (re-selecting the active
  // session, where the keyed remount above never fires); the latch inside
  // the hook covers the first-open race. App only dispatches "composer" on
  // desktop, so no coarse-pointer gate is needed here.
  useFocusTerminalTarget("composer", taRef);

  const wrapperLayout = composerWrapperLayout({ keyboardOpen });
  return (
    <div className={wrapperLayout.className} style={wrapperLayout.style}>
      <div
        {...tourAnchor(TOUR_ANCHORS.composer)}
        className="mx-auto max-w-3xl xl:max-w-4xl 2xl:max-w-5xl"
        onDragOver={(e) => {
          const hasFiles = Array.from(e.dataTransfer?.types ?? []).includes("Files");
          if (!hasFiles) return;
          e.preventDefault();
        }}
        onDrop={(e) => {
          const dropped = e.dataTransfer?.files;
          if (!dropped || dropped.length === 0) return;
          e.preventDefault();
          if (!attachmentsEnabled) return;
          void addFiles(dropped);
        }}
      >
        <ComposerPrimitive.Unstable_TriggerPopoverRoot>
          <ComposerPrimitive.Root
            className={[
              "group relative flex flex-col gap-2 rounded-xl border border-surface-700 bg-surface-850",
              "shadow-[inset_0_1px_0_rgba(255,255,255,0.02)]",
              "focus-within:border-brand-600/70 focus-within:shadow-[inset_0_1px_0_rgba(255,255,255,0.02),0_0_0_3px_rgba(217,119,6,0.12)]",
              "transition-colors duration-150",
            ].join(" ")}
          >
            {/* @ file picker — Directive behavior chips the path into
                the prompt text using the default formatter. */}
            <ComposerPrimitive.Unstable_TriggerPopover
              char="@"
              adapter={fileAdapter}
              className="absolute bottom-full left-0 right-0 mb-2 z-30 overflow-hidden rounded-lg border border-surface-700 bg-surface-850 shadow-xl"
            >
              <ComposerPrimitive.Unstable_TriggerPopover.Directive
                formatter={defaultDirectiveFormatter}
              />
              <PopoverItems trigger="@" />
            </ComposerPrimitive.Unstable_TriggerPopover>

            {/* / slash commands — Action behavior fires a handler and
                strips the `/cmd` text from the input. */}
            <ComposerPrimitive.Unstable_TriggerPopover
              char="/"
              adapter={slashAdapter}
              className="absolute bottom-full left-0 right-0 mb-2 z-30 overflow-hidden rounded-lg border border-surface-700 bg-surface-850 shadow-xl"
            >
              <ComposerPrimitive.Unstable_TriggerPopover.Action
                onExecute={(item) => insertSlashCommand(composerRuntime, item)}
                removeOnExecute
              />
              <PopoverItems trigger="/" />
            </ComposerPrimitive.Unstable_TriggerPopover>

            {/* Input area — tall by default, grows up to 200px */}
            <ComposerPrimitive.Input
              ref={taRef}
              rows={2}
              // assistant-ui's default Escape binding cancels the active
              // run (see ComposerPrimitive.Input's `cancelOnEscape`
              // default). The cockpit deliberately keeps cancel behind
              // an explicit gesture, the Stop button, because Claude
              // Code CLI also hijacks Escape for cancel and a stray
              // press would lose work the user did not mean to abort.
              cancelOnEscape={false}
              placeholder={
                turnActive
                  ? "Queue a follow-up… (sent when current turn ends)"
                  : "Send a message…  Type @ for files, / for commands"
              }
              onInput={onInput}
              onFocus={() => {
                // Defensive scrollIntoView for mobile soft-keyboard cycles.
                // The App root no longer pins height for cockpit (#1177),
                // so `h-dvh` shrinks with the keyboard and the composer
                // should naturally lift into view; this is a belt-and-
                // braces hop after the keyboard animation completes so
                // any UA that lags the layout-viewport update still ends
                // up scrolled to the composer.
                if (!isMobile) return;
                window.setTimeout(() => {
                  taRef.current?.scrollIntoView({
                    block: "end",
                    behavior: "smooth",
                  });
                }, 300);
              }}
              onBeforeInput={(e) => {
                // Android Chrome's GBoard / Samsung Keyboard often fire
                // `beforeinput` with `insertLineBreak` / `insertParagraph`
                // for the on-screen Enter key WITHOUT a usable `keydown`
                // (key is "Unidentified" or keyCode 229). The keydown
                // matrix below misses those, so assistant-ui's bubble-
                // phase Send wins and sends the message. Intercept here
                // for mobile, insert a literal newline at the caret, and
                // let the keydown matrix handle the platforms that do
                // fire a real Enter keydown. See #1174.
                const ne = e.nativeEvent as InputEvent;
                const newlineAction = decideBeforeInputAction(
                  ne.inputType,
                  ne.isComposing,
                  { isMobile },
                );
                if (newlineAction === "newline") {
                  e.preventDefault();
                  e.stopPropagation();
                  insertNewlineAtCaret(taRef);
                  return;
                }
                // iOS native dictation burst detection (#1431). Driven
                // from `beforeinput` rather than `input` so the burst
                // flag is set before assistant-ui's onChange (composed
                // by radix) gets a chance to run and call `setText`.
                dictationGuard.observeInputType(ne.inputType, Date.now());
              }}
              onChange={(e) => {
                // Suppress assistant-ui's controlled-input flush while
                // an iOS dictation burst is active (#1431). Radix's
                // `composeEventHandlers` (used by ComposerPrimitive.Input)
                // skips the downstream handler when `defaultPrevented`
                // is set on the SyntheticEvent.
                if (dictationGuard.shouldSuppressUpstream(e.currentTarget.value)) {
                  e.preventDefault();
                }
              }}
              onBlur={() => {
                // Tapping Send (or anywhere outside the textarea) blurs
                // the iOS soft-keyboard-owned field; flush the dictation
                // buffer first so the pending text lands in
                // assistant-ui state before the Send click reads it.
                dictationGuard.flushOnBlur();
              }}
              onKeyDown={(e) => {
                // Three-way Enter dispatch. See decideEnterAction for
                // the full matrix; the inline branches below handle
                // each outcome:
                //   - "newline": touch-primary device, plain Enter.
                //     Stop assistant-ui's bubble-phase Send and let
                //     the textarea insert a newline natively (no
                //     preventDefault). Mobile users tap the Send
                //     button to dispatch.
                //   - "send": desktop, mid-turn, plain Enter.
                //     ComposerPrimitive.Input hard-blocks Enter while
                //     thread.isRunning && !queue (#1031), so we
                //     intercept and route through our queue path.
                //   - "default": modifier keys, IME compose, non-Enter
                //     keys, or desktop idle Enter; let the primitive's
                //     built-in keymap run.
                const action = decideEnterAction(
                  {
                    key: e.key,
                    shiftKey: e.shiftKey,
                    ctrlKey: e.ctrlKey,
                    metaKey: e.metaKey,
                    isComposing: e.nativeEvent.isComposing,
                  },
                  { isMobile, turnActive },
                );
                if (action === "default") return;
                if (action === "newline") {
                  e.stopPropagation();
                  return;
                }
                // action === "send"
                e.preventDefault();
                e.stopPropagation();
                submitComposer();
              }}
              onPaste={(e) => {
                // Cmd/Ctrl+V of an image (screenshot) lands here as a
                // clipboard file item. Capture supported files and stage
                // them; let text paste fall through untouched. See #965.
                if (!attachmentsEnabled) return;
                const items = Array.from(e.clipboardData?.items ?? []);
                const files = items
                  .filter((it) => it.kind === "file")
                  .map((it) => it.getAsFile())
                  .filter((f): f is File => f != null);
                if (files.length === 0) return;
                e.preventDefault();
                void addFiles(files);
              }}
              autoFocus={!isMobile}
              className={[
                "min-h-[56px] max-h-[200px] resize-none bg-transparent",
                "px-4 pt-3 pb-1 text-sm leading-6 text-text-primary",
                "placeholder:text-text-dim focus:outline-none",
              ].join(" ")}
            />

            {/* Staged attachments — thumbnails for images, labelled
                chips for audio / resources. Removable before send. */}
            {pendingAttachments.length > 0 && (
              <div className="flex flex-wrap gap-2 px-3 pt-1">
                {pendingAttachments.map((att, i) => (
                  <div
                    key={`${att.name ?? att.kind}-${i}`}
                    className="group/att relative flex items-center gap-2 rounded-md border border-surface-700 bg-surface-800 py-1 pl-1 pr-2 text-[11px] text-text-secondary"
                  >
                    {att.kind === "image" ? (
                      <img
                        src={`data:${att.mimeType};base64,${att.dataB64}`}
                        alt={att.name ?? "attachment"}
                        className="h-8 w-8 rounded object-cover"
                      />
                    ) : (
                      <span className="flex h-8 w-8 items-center justify-center rounded bg-surface-700">
                        <Paperclip className="h-3.5 w-3.5" />
                      </span>
                    )}
                    <span className="max-w-[120px] truncate">
                      {att.name ?? att.kind}
                    </span>
                    <button
                      type="button"
                      aria-label={`Remove ${att.name ?? "attachment"}`}
                      title="Remove attachment"
                      onClick={() => removeAttachment(i)}
                      className="rounded p-0.5 text-text-dim hover:bg-surface-700 hover:text-text-secondary"
                    >
                      <X className="h-3 w-3" />
                    </button>
                  </div>
                ))}
              </div>
            )}

            {/* Footer strip — affordances on the left, send/stop on the right */}
            <div className="flex items-center justify-between gap-2 border-t border-surface-800/60 px-2 pb-2 pt-1.5">
              <div className="flex items-center gap-0.5">
                <ToolbarButton
                  icon={<AtSign className="h-3.5 w-3.5" />}
                  label="Add file context (@)"
                  hint="@"
                  onClick={() => insertAtCaret(taRef, "@")}
                />
                <ToolbarButton
                  icon={<Slash className="h-3.5 w-3.5" />}
                  label="Slash command (/)"
                  hint="/"
                  onClick={() => insertAtCaret(taRef, "/")}
                />
                <ToolbarButton
                  icon={<Paperclip className="h-3.5 w-3.5" />}
                  label={
                    attachmentsEnabled
                      ? "Attach files (image / audio / resource)"
                      : promptCapabilities
                        ? "This agent does not accept attachments"
                        : "Waiting for agent capabilities…"
                  }
                  disabled={!attachmentsEnabled}
                  onClick={() => fileInputRef.current?.click()}
                />
                <input
                  ref={fileInputRef}
                  type="file"
                  multiple
                  accept={acceptForCaps(promptCapabilities)}
                  className="hidden"
                  onChange={(e) => {
                    const picked = e.target.files;
                    if (picked && picked.length > 0) void addFiles(picked);
                    // Reset so re-picking the same file fires onChange.
                    e.target.value = "";
                  }}
                />
                <span className="mx-1 h-4 w-px bg-surface-700" aria-hidden />
                <ModePicker
                  sessionId={sessionId}
                  availableModes={availableModes}
                  currentModeId={currentModeId}
                  legacyMode={legacyMode}
                />
                <SessionConfigControls
                  configOptions={configOptions}
                  pendingConfigOption={pendingConfigOption}
                  onSetConfigOption={setConfigOption}
                />
                <span className="mx-1 h-4 w-px bg-surface-700" aria-hidden />
                <ToolbarButton
                  icon={<ArrowLeftRight className="h-3.5 w-3.5" />}
                  label="Switch agent"
                  onClick={() => setSwitchAgentOpen(true)}
                />
              </div>

              <div className="flex items-center gap-2">
                <UsageHint usage={sessionUsage} />
                {turnActive ? (
                  <>
                    <StopButton />
                    <QueueSendButton
                      connected={connected}
                      queuedCount={queuedCount}
                      onSend={submitComposer}
                    />
                  </>
                ) : (
                  <SendButton connected={connected} onSend={submitComposer} />
                )}
              </div>
            </div>
          </ComposerPrimitive.Root>
        </ComposerPrimitive.Unstable_TriggerPopoverRoot>
      </div>
      <SwitchAgentModal
        open={switchAgentOpen}
        sessionId={sessionId}
        currentAgent={currentAgent}
        onClose={() => setSwitchAgentOpen(false)}
        onPrefill={(text) => {
          composerRuntime.setText(text);
          requestAnimationFrame(() => {
            const el = taRef.current;
            if (!el) return;
            el.focus();
            const len = el.value.length;
            try {
              el.setSelectionRange(len, len);
            } catch {
              // ignore: non-text inputs can throw here
            }
            el.style.height = "auto";
            el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
          });
        }}
        trigger="manual"
      />
    </div>
  );
}

/** Popover items list — same render shape for @ and / since both
 *  have a single category and we surface a flat list. */
function PopoverItems({ trigger }: { trigger: string }) {
  return (
    <ComposerPrimitive.Unstable_TriggerPopoverItems className="max-h-64 overflow-y-auto">
      {(items) =>
        items.length === 0 ? (
          <div className="px-3 py-2 text-xs italic text-text-dim">
            No matches
          </div>
        ) : (
          items.map((item, i) => (
            <ComposerPrimitive.Unstable_TriggerPopoverItem
              key={item.id}
              item={item}
              index={i}
              className={[
                "flex w-full items-start gap-2 px-3 py-2 text-left text-xs",
                "hover:bg-surface-800/60",
                "data-[highlighted=true]:bg-surface-800",
              ].join(" ")}
            >
              <span className="font-mono text-text-dim">{trigger}</span>
              <span className="min-w-0 flex-1">
                <span className="block truncate font-medium text-text-primary">
                  {item.label}
                </span>
                {item.description && (
                  <span className="block truncate text-[11px] text-text-dim">
                    {item.description}
                  </span>
                )}
              </span>
            </ComposerPrimitive.Unstable_TriggerPopoverItem>
          ))
        )
      }
    </ComposerPrimitive.Unstable_TriggerPopoverItems>
  );
}

/** Insert the picked slash command into the composer text. The Action
 *  popover already stripped the user's `/<typed>` from the input via
 *  `removeOnExecute`, so we set the canonical `/<name>` form and add
 *  a trailing space. The trailing space halts assistant-ui's
 *  `detectTrigger` backward scan (which keys off whitespace as the
 *  trigger boundary) so the popover does not immediately re-open on
 *  the inserted `/<name>` and consume the next Enter as a re-pick;
 *  it also positions the cursor for free-form arg typing when the
 *  agent advertised the command as `acceptsInput=true`. See #1512. */
export function insertSlashCommand(
  runtime: ReturnType<typeof useComposerRuntime>,
  item: Unstable_TriggerItem,
) {
  if (!runtime) return;
  const current = runtime.getState().text;
  const suffix = " ";
  // Preserve any text that was already in the buffer (e.g. user typed
  // a long prompt then ran `/foo` mid-message). We just append the
  // command at the end; the typed `/typed` token has already been
  // removed by removeOnExecute, so trailing whitespace is rare.
  const sep = current.length > 0 && !current.endsWith(" ") ? " " : "";
  runtime.setText(`${current}${sep}/${item.id}${suffix}`);
}

/** Insert a literal "\n" at the textarea's caret. Used by the
 *  `beforeinput` interception path for mobile Enter (#1174): when
 *  Android's on-screen keyboard fires `beforeinput` with
 *  `insertLineBreak` / `insertParagraph` (and possibly no usable
 *  `keydown`), we preventDefault on the synthetic line-break and
 *  insert one ourselves so the cursor lands one position past the
 *  newline. Skips the trigger-detection whitespace padding that
 *  `insertAtCaret` does for `@` / `/`; a newline doesn't need it. */
export function insertNewlineAtCaret(
  ref: React.RefObject<HTMLTextAreaElement | null>,
): void {
  const ta = ref.current;
  if (!ta) return;
  const start = ta.selectionStart ?? ta.value.length;
  const end = ta.selectionEnd ?? start;
  const before = ta.value.slice(0, start);
  const after = ta.value.slice(end);
  const next = `${before}\n${after}`;
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    "value",
  )?.set;
  setter?.call(ta, next);
  ta.dispatchEvent(
    new InputEvent("input", {
      bubbles: true,
      inputType: "insertLineBreak",
    }),
  );
  const pos = before.length + 1;
  ta.setSelectionRange(pos, pos);
}

/** Insert `text` at the textarea's caret and re-focus. The toolbar
 *  buttons use this to inject `@` or `/` so the trigger popover opens
 *  without forcing the user to grab the keyboard.
 *
 *  Exported for tests. We dispatch a real `InputEvent` (not a generic
 *  `Event`) so assistant-ui's `Unstable_TriggerPopover` sees the
 *  `inputType: "insertText"` + `data: text` fields it relies on for
 *  trigger detection. Without those, the popover library treats the
 *  toolbar-injected character as untracked text and a subsequent
 *  `removeOnExecute` cannot find the trigger to strip, leaving a
 *  duplicate `@@` / `//` in the input (#1149). */
export function insertAtCaret(
  ref: React.RefObject<HTMLTextAreaElement | null>,
  text: string,
) {
  const ta = ref.current;
  if (!ta) return;
  const start = ta.selectionStart ?? ta.value.length;
  const end = ta.selectionEnd ?? start;
  const before = ta.value.slice(0, start);
  // Trigger detection requires whitespace (or start-of-string) before
  // the trigger char; pad if we're mid-word.
  const needsSpace =
    before.length > 0 && !/[\s\n\t]$/.test(before) ? " " : "";
  const next = before + needsSpace + text + ta.value.slice(end);
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    "value",
  )?.set;
  setter?.call(ta, next);
  ta.dispatchEvent(
    new InputEvent("input", {
      bubbles: true,
      inputType: "insertText",
      data: text,
    }),
  );
  const pos = before.length + needsSpace.length + text.length;
  ta.focus();
  ta.setSelectionRange(pos, pos);
}

function extDescription(path: string): string | undefined {
  const m = path.match(/\.([a-z0-9]+)$/i);
  return m?.[1]?.toLowerCase();
}

/* ── Toolbar buttons ─────────────────────────────────────────────── */

function ToolbarButton({
  icon,
  label,
  hint,
  disabled,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  hint?: string;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
      className={[
        "inline-flex items-center gap-1 rounded-md px-2 py-1 text-[11px] text-text-dim",
        "hover:bg-surface-800 hover:text-text-secondary",
        "disabled:cursor-not-allowed disabled:opacity-60 disabled:hover:bg-transparent disabled:hover:text-text-dim",
        "transition-colors",
      ].join(" ")}
    >
      {icon}
      {hint && <span className="font-mono">{hint}</span>}
    </button>
  );
}

/* ── Mode picker ─────────────────────────────────────────────────── */

const LEGACY_MODES: ReadonlyArray<{
  id: string;
  legacyId: CockpitState["mode"];
  name: string;
  description: string;
}> = [
  { id: "default", legacyId: "Default", name: "Default", description: "Approve each tool individually" },
  { id: "plan", legacyId: "Plan", name: "Plan", description: "Plan first, no edits applied" },
  { id: "accept_edits", legacyId: "AcceptEdits", name: "Accept edits", description: "Auto-approve safe file edits" },
  { id: "bypass_permissions", legacyId: "BypassPermissions", name: "Yolo", description: "Skip all approvals (destructive)" },
];

interface ModePickerProps {
  sessionId: string;
  availableModes: CockpitState["availableModes"];
  currentModeId: string | null;
  legacyMode: CockpitState["mode"];
}

function ModePicker({
  sessionId,
  availableModes,
  currentModeId,
  legacyMode,
}: ModePickerProps) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement | null>(null);

  // Use real agent-advertised modes when available, otherwise fall
  // back to the four-mode taxonomy. Even with agent modes, we still
  // tint by id pattern (default/plan/accept/bypass) because Claude's
  // adapter happens to use those tokens.
  const usingAgentModes = availableModes.length > 0;
  const modes = usingAgentModes
    ? availableModes.map((m) => ({
        id: m.id,
        name: m.name,
        description: m.description ?? "",
      }))
    : LEGACY_MODES.map((m) => ({
        id: m.id,
        name: m.name,
        description: m.description,
      }));

  // Pick "current": agent-reported id wins; else map legacyMode → id.
  const fallbackId =
    LEGACY_MODES.find((m) => m.legacyId === legacyMode)?.id ?? "default";
  const activeId = currentModeId ?? fallbackId;
  const current = modes.find((m) => m.id === activeId) ?? modes[0]!;

  // Tint the chip by id pattern so destructive modes are visually loud.
  const tone = toneForId(activeId);

  // Close on outside click / Esc.
  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const select = async (id: string) => {
    setOpen(false);
    if (id === activeId) return;
    try {
      await fetch(
        `/api/sessions/${encodeURIComponent(sessionId)}/cockpit/mode`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ mode_id: id }),
        },
      );
    } catch {
      // The agent broadcasts CurrentModeChanged on success; if the
      // request fails the UI stays on the current mode.
    }
  };

  return (
    <div ref={ref} {...tourAnchor(TOUR_ANCHORS.modePicker)} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        title={current.description || `Mode: ${current.name}`}
        className={[
          "inline-flex items-center gap-1 rounded-md border px-2 py-1 text-[11px] font-medium",
          "transition-colors",
          tone,
        ].join(" ")}
      >
        <span>{current.name}</span>
        <ChevronUp className="h-3 w-3 opacity-70" />
      </button>
      {open && (
        <div
          className="absolute bottom-full left-0 z-30 mb-1 w-56 overflow-hidden rounded-md border border-surface-700 bg-surface-850 shadow-xl"
          role="menu"
        >
          <div className="border-b border-surface-800 px-3 py-1.5 text-[10px] uppercase tracking-wider text-text-dim">
            {usingAgentModes ? "Agent modes" : "Modes"}
          </div>
          {modes.map((opt) => (
            <button
              key={opt.id}
              type="button"
              role="menuitem"
              onClick={() => void select(opt.id)}
              className={[
                "flex w-full items-start gap-2 px-3 py-2 text-left text-xs hover:bg-surface-800",
                opt.id === activeId ? "bg-surface-800/60" : "",
              ].join(" ")}
            >
              <span
                className={[
                  "mt-0.5 inline-block h-3 w-3 shrink-0 rounded-full border",
                  opt.id === activeId
                    ? "border-brand-500 bg-brand-500"
                    : "border-surface-700",
                ].join(" ")}
              />
              <span className="min-w-0 flex-1">
                <span className="block font-medium text-text-primary">
                  {opt.name}
                </span>
                {opt.description && (
                  <span className="block text-[11px] text-text-dim">
                    {opt.description}
                  </span>
                )}
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

function toneForId(id: string): string {
  if (/bypass|yolo/i.test(id))
    return "border-rose-700/50 bg-rose-950/30 text-rose-300 hover:border-rose-700";
  if (/accept/i.test(id))
    return "border-amber-700/50 bg-amber-950/30 text-amber-300 hover:border-amber-700";
  if (/plan/i.test(id))
    return "border-cyan-800/50 bg-cyan-950/30 text-cyan-300 hover:border-cyan-700";
  return "border-surface-700 bg-surface-800 text-text-secondary hover:border-surface-600";
}

/* ── Usage hint ──────────────────────────────────────────────────── */

function UsageHint({ usage }: { usage: CockpitState["sessionUsage"] }) {
  if (!usage || usage.size <= 0) return null;
  const pct = Math.min(100, Math.round((usage.used / usage.size) * 100));
  const tone =
    pct >= 90
      ? "text-rose-400"
      : pct >= 75
        ? "text-amber-400"
        : "text-text-dim";
  const usedLabel = formatTokens(usage.used);
  const sizeLabel = formatTokens(usage.size);
  const cost = usage.cost
    ? formatCost(usage.cost.amount, usage.cost.currency)
    : null;
  const title =
    `Context: ${usage.used.toLocaleString()} / ${usage.size.toLocaleString()} tokens (${pct}%)` +
    (cost ? ` · session cost ${cost}` : "");
  return (
    <span
      className={`hidden sm:inline-flex items-center gap-1 text-[11px] tabular-nums ${tone}`}
      title={title}
      aria-label={title}
    >
      <span>
        {usedLabel}/{sizeLabel}
      </span>
      <span className="opacity-70">({pct}%)</span>
      {cost ? <span className="opacity-70">· {cost}</span> : null}
    </span>
  );
}

function formatTokens(n: number): string {
  if (n < 1_000) return String(n);
  if (n < 1_000_000) return `${(n / 1_000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(n < 10_000_000 ? 2 : 1)}M`;
}

function formatCost(amount: number, currency: string): string {
  try {
    return new Intl.NumberFormat(undefined, {
      style: "currency",
      currency,
      maximumFractionDigits: amount < 1 ? 4 : 2,
    }).format(amount);
  } catch {
    return `${amount.toFixed(amount < 1 ? 4 : 2)} ${currency}`;
  }
}

/* ── Send / Stop ─────────────────────────────────────────────────── */

function SendButton({
  connected = true,
  onSend,
}: {
  connected?: boolean;
  onSend: () => void;
}) {
  // When the session is inactive (WS closed, worker stopped, worker
  // restarting) we leave the button clickable: `sendPrompt` routes the
  // text into the local queue and the drain effect fires it on resume.
  // The tooltip swaps so users can tell the click queued rather than
  // sent. A custom button (not ComposerPrimitive.Send) drives submit so
  // the staged attachments ride along and an attachment-only prompt can
  // send even with empty text. See #1359 / #1000.
  const title = connected
    ? "Send, Enter"
    : "Session not active, will send on resume";
  const label = connected ? "Send message" : "Queue message until session resumes";
  return (
    <button
      type="button"
      aria-label={label}
      title={title}
      onClick={onSend}
      className={[
        "group/send inline-flex items-center justify-center gap-1",
        "rounded-lg bg-brand-600 px-2.5 py-1.5 text-white shadow-sm",
        "hover:bg-brand-500 active:scale-[0.98]",
        "transition-all duration-100",
      ].join(" ")}
    >
      <PaperPlaneIcon />
    </button>
  );
}

function StopButton() {
  const runtime = useThreadRuntime();
  return (
    <button
      type="button"
      aria-label="Stop"
      title="Stop the agent"
      onClick={() => runtime.cancelRun()}
      className={[
        "inline-flex items-center justify-center gap-1.5",
        "rounded-lg border border-surface-600 bg-surface-800",
        "px-2.5 py-1.5 text-[12px] font-medium text-text-secondary",
        "hover:border-rose-700/60 hover:bg-rose-950/30 hover:text-rose-300",
        "active:scale-[0.98] transition-all duration-100",
      ].join(" ")}
    >
      <Square className="h-3.5 w-3.5 fill-current" strokeWidth={0} />
      <span>Stop</span>
    </button>
  );
}

/** Send button rendered alongside Stop while a turn is in flight.
 *  Bypasses ComposerPrimitive.Send (which is disabled by the SDK when
 *  the thread is running). Shows a small badge with the current queue
 *  length so users can see at a glance how many follow-ups are stacked
 *  up. See #1031. Inactive sessions (WS closed, worker stopped /
 *  restarting) keep the button clickable and swap the tooltip; the
 *  click routes through `sendPrompt`'s enqueue branch instead of
 *  POSTing. See #1359. */
function QueueSendButton({
  connected,
  queuedCount,
  onSend,
}: {
  connected: boolean;
  queuedCount: number;
  onSend: () => void;
}) {
  const title = !connected
    ? queuedCount > 0
      ? `Queue follow-up (${queuedCount} pending), will send on resume, Enter`
      : "Queue follow-up, will send on resume, Enter"
    : queuedCount > 0
      ? `Queue follow-up (${queuedCount} pending), Enter`
      : "Queue follow-up (sent when current turn ends), Enter";
  return (
    <button
      type="button"
      aria-label="Queue follow-up message"
      {...tourAnchor(TOUR_ANCHORS.queueSend)}
      title={title}
      onClick={onSend}
      className={[
        "group/send relative inline-flex items-center justify-center gap-1",
        "rounded-lg bg-brand-600 px-2.5 py-1.5 text-white shadow-sm",
        "hover:bg-brand-500 active:scale-[0.98]",
        "transition-all duration-100",
      ].join(" ")}
    >
      <PaperPlaneIcon />
      {queuedCount > 0 && (
        <span
          aria-hidden
          className={[
            "absolute -right-1.5 -top-1.5 inline-flex h-4 min-w-[16px] items-center justify-center",
            "rounded-full bg-sky-500 px-1 text-[10px] font-semibold text-surface-900",
            "ring-2 ring-surface-900",
          ].join(" ")}
        >
          {queuedCount}
        </span>
      )}
    </button>
  );
}

/** Pulls current composer text, hands it to the cockpit queue, then
 *  clears the textarea + persisted draft. Shared by the mid-turn Send
 *  button and the Enter-while-running keyboard handler. */
function sendFromTextarea(
  taRef: React.RefObject<HTMLTextAreaElement | null>,
  composerRuntime: ReturnType<typeof useComposerRuntime>,
  enqueuePrompt: (
    text: string,
    attachments?: PromptAttachmentInput[],
  ) => void | Promise<void>,
  attachments: PromptAttachmentInput[] = [],
  clearAttachments?: () => void,
): void {
  if (!composerRuntime) return;
  const text = composerRuntime.getState().text.trim();
  // Allow an attachment-only prompt (e.g. a pasted screenshot with no
  // text); otherwise require some text.
  if (!text && attachments.length === 0) return;
  void enqueuePrompt(text, attachments.length > 0 ? attachments : undefined);
  composerRuntime.setText("");
  clearAttachments?.();
  // Manually reset the textarea height; auto-grow runs on input events
  // and we cleared the value without firing one.
  const el = taRef.current;
  if (el) el.style.height = "auto";
}

function PaperPlaneIcon() {
  return (
    <svg
      viewBox="0 0 24 24"
      width="14"
      height="14"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M22 2 11 13" />
      <path d="M22 2 15 22l-4-9-9-4 20-7Z" />
    </svg>
  );
}

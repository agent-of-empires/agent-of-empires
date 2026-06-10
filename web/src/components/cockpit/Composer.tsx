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
import { useEffect, useMemo, useRef, useState } from "react";
import {
  AtSign,
  ChevronUp,
  ClipboardPaste,
  Send,
  Slash,
  Square,
} from "lucide-react";

import { useFilesIndex, fuzzyFilter } from "./useFilesIndex";
import type { CockpitState } from "../../lib/cockpitTypes";
import { getDraft, setDraft } from "../../lib/cockpitDrafts";
import { useMobileKeyboard } from "../../hooks/useMobileKeyboard";
import { useAgentProfile } from "../../lib/agentProfileContext";
import { toastBus } from "../../lib/toastBus";

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

interface Props {
  sessionId: string;
  availableModes: CockpitState["availableModes"];
  currentModeId: CockpitState["currentModeId"];
  /** Legacy enum-based mode used as fallback when the agent does not
   *  advertise modes via NewSessionResponse. */
  legacyMode: CockpitState["mode"];
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
  enqueuePrompt: (text: string) => void | Promise<void>;
  /** When set, replace the current composer text with `text` and
   *  focus the textarea (cursor at end). Used by the context-primer
   *  banner to prefill a transcript recap before send. The `id` is
   *  a fresh nonce per insertion so the effect re-fires even when
   *  the same text is inserted twice. See #1004. */
  primerPrefill?: { id: string; text: string } | null;
}

export function Composer({
  sessionId,
  availableModes,
  currentModeId,
  legacyMode,
  sessionUsage,
  availableCommands,
  connected,
  turnActive,
  queuedCount,
  enqueuePrompt,
  primerPrefill,
}: Props) {
  const taRef = useRef<HTMLTextAreaElement | null>(null);
  const { files } = useFilesIndex(sessionId);

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

  const wrapperLayout = composerWrapperLayout({ keyboardOpen });
  return (
    <div className={wrapperLayout.className} style={wrapperLayout.style}>
      <div className="mx-auto max-w-3xl xl:max-w-4xl 2xl:max-w-5xl">
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
                const action = decideBeforeInputAction(
                  ne.inputType,
                  ne.isComposing,
                  { isMobile },
                );
                if (action === "default") return;
                e.preventDefault();
                e.stopPropagation();
                insertNewlineAtCaret(taRef);
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
                void sendFromTextarea(taRef, composerRuntime, enqueuePrompt);
              }}
              autoFocus={!isMobile}
              enterKeyHint={isMobile ? "enter" : undefined}
              inputMode="text"
              className={[
                "min-h-[56px] max-h-[200px] resize-none bg-transparent",
                "px-4 pt-3 pb-1 text-sm leading-6 text-text-primary",
                "placeholder:text-text-dim focus:outline-none",
              ].join(" ")}
            />

            {/* Footer strip — affordances on the left, send/stop on the right */}
            <div className="flex flex-wrap items-center justify-between gap-2 border-t border-surface-800/60 px-2 pb-2 pt-1.5">
              <div className="flex min-w-0 flex-wrap items-center gap-0.5">
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
                  icon={<ClipboardPaste className="h-3.5 w-3.5" />}
                  label="Paste from clipboard"
                  onClick={() => {
                    void pasteClipboardAtCaret(taRef).then((ok) => {
                      if (!ok) {
                        toastBus.handler?.error(
                          "Couldn't read clipboard. Use HTTPS or paste from the keyboard menu.",
                        );
                        taRef.current?.focus();
                      }
                    });
                  }}
                />
                <span className="mx-1 h-4 w-px bg-surface-700" aria-hidden />
                <ModePicker
                  sessionId={sessionId}
                  availableModes={availableModes}
                  currentModeId={currentModeId}
                  legacyMode={legacyMode}
                />
              </div>

              <div className="ml-auto flex items-center gap-2">
                <UsageHint usage={sessionUsage} />
                {turnActive ? (
                  <>
                    <StopButton />
                    <QueueSendButton
                      connected={connected}
                      queuedCount={queuedCount}
                      onSend={() => sendFromTextarea(taRef, composerRuntime, enqueuePrompt)}
                    />
                  </>
                ) : (
                  <SendButton connected={connected} />
                )}
              </div>
            </div>
          </ComposerPrimitive.Root>
        </ComposerPrimitive.Unstable_TriggerPopoverRoot>
      </div>
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
 *  a trailing space when the agent advertised that the command takes
 *  free-form arguments. The user is then free to type args and hit
 *  Enter to send, or hit Enter immediately for a no-arg command. */
function insertSlashCommand(
  runtime: ReturnType<typeof useComposerRuntime>,
  item: Unstable_TriggerItem,
) {
  if (!runtime) return;
  const accepts = (item as { acceptsInput?: boolean }).acceptsInput === true;
  const current = runtime.getState().text;
  const suffix = accepts ? " " : "";
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

/** Insert arbitrary pasted text without trigger-specific whitespace
 *  padding. Kept separate from `insertAtCaret` because @ and / toolbar
 *  injections need trigger detection, while pasted code or shell
 *  snippets must stay byte-for-byte as copied. */
export function insertPlainTextAtCaret(
  ref: React.RefObject<HTMLTextAreaElement | null>,
  text: string,
) {
  const ta = ref.current;
  if (!ta || text.length === 0) return;
  const start = ta.selectionStart ?? ta.value.length;
  const end = ta.selectionEnd ?? start;
  const before = ta.value.slice(0, start);
  const next = before + text + ta.value.slice(end);
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    "value",
  )?.set;
  setter?.call(ta, next);
  ta.dispatchEvent(
    new InputEvent("input", {
      bubbles: true,
      inputType: "insertFromPaste",
      data: text,
    }),
  );
  const pos = before.length + text.length;
  ta.focus();
  ta.setSelectionRange(pos, pos);
}

export async function pasteClipboardAtCaret(
  ref: React.RefObject<HTMLTextAreaElement | null>,
): Promise<boolean> {
  if (typeof window === "undefined") return false;
  if (!window.isSecureContext) return false;
  const readText = navigator.clipboard?.readText;
  if (!readText) return false;
  try {
    const text = await readText.call(navigator.clipboard);
    if (!text) return false;
    insertPlainTextAtCaret(ref, text);
    return true;
  } catch {
    return false;
  }
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
      onMouseDown={(e) => e.preventDefault()}
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
    <div ref={ref} className="relative">
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

function SendButton({ connected = true }: { connected?: boolean }) {
  // When the session is inactive (WS closed, worker stopped, worker
  // restarting) we leave the button clickable: `sendPrompt` routes the
  // text into the local queue and the drain effect fires it on resume.
  // The tooltip swaps so users can tell the click queued rather than
  // sent. ComposerPrimitive.Send still drives the assistant-ui submit
  // flow; it does not look at our `connected` flag. See #1359.
  const title = connected
    ? "Send, Enter"
    : "Session not active, will send on resume";
  const label = connected ? "Send message" : "Queue message until session resumes";
  return (
    <ComposerPrimitive.Send asChild>
      <button
        type="submit"
        aria-label={label}
        title={title}
        className={[
          "group/send inline-flex items-center justify-center gap-1",
          "rounded-lg bg-brand-600 px-2.5 py-1.5 text-white shadow-sm",
          "hover:bg-brand-500 active:scale-[0.98]",
          "transition-all duration-100",
        ].join(" ")}
      >
        <Send className="h-3.5 w-3.5" aria-hidden="true" />
      </button>
    </ComposerPrimitive.Send>
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
      title={title}
      onClick={onSend}
      className={[
        "group/send relative inline-flex items-center justify-center gap-1",
        "rounded-lg bg-brand-600 px-2.5 py-1.5 text-white shadow-sm",
        "hover:bg-brand-500 active:scale-[0.98]",
        "transition-all duration-100",
      ].join(" ")}
    >
      <Send className="h-3.5 w-3.5" aria-hidden="true" />
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
  enqueuePrompt: (text: string) => void | Promise<void>,
): void {
  if (!composerRuntime) return;
  const text = composerRuntime.getState().text.trim();
  if (!text) return;
  void enqueuePrompt(text);
  composerRuntime.setText("");
  // Manually reset the textarea height; auto-grow runs on input events
  // and we cleared the value without firing one.
  const el = taRef.current;
  if (el) el.style.height = "auto";
}

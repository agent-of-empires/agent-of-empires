// @vitest-environment jsdom
//
// Regression test for #3418, driving the real assistant-ui popover
// through a mounted Composer.
//
// Two symptoms, one cause. `insertSlashCommand` used to write the
// completion with `runtime.setText`, which updates assistant-ui's
// composer text but not the `cursorPosition` state its trigger
// detection reads. Detection kept scanning a stale prefix, so the
// popover stayed open, `triggerKeyboardResource` claimed the user's
// next Enter, and a second `selectItem` sliced the live text with the
// stale `trigger.query`, leaving `ess-pr-comments /address-pr-comments`.
// The same write also appended at the end of the buffer, so a command
// picked mid-message landed in the wrong place.
//
// jsdom is the tier where this is deterministic. A real browser often
// papers over the stale cursor: assigning a textarea's value moves the
// caret, which fires `selectionchange`, which React's onSelect polyfill
// turns into `setCursorPosition`. jsdom does not do that, so the stale
// state persists and the defect is reproducible instead of racy. That
// premise is asserted below rather than assumed, so a future jsdom that
// does implement it fails loudly instead of silently testing nothing.
//
// Items are picked by click, not Enter: assistant-ui routes keyboard
// selection through a composer input plugin that does not receive
// synthetic keydowns under jsdom. Both paths funnel through the same
// `selectItem` -> `onExecute` -> insert code, so the invariants here
// hold for either. Keyboard accept plus the following Enter is covered
// in tests/live/acp-stories/composer-slash-pick-no-arg.spec.ts.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";

import { Composer } from "./Composer";

const COMMANDS = [
  { name: "address-pr-comments", description: "Address PR comments", accepts_input: false },
  { name: "help", description: "Show help", accepts_input: false },
];

function Harness() {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: false,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        sessionId="sess-slash"
        currentAgent="claude"
        availableModes={[]}
        currentModeId={null}
        legacyMode="Default"
        configOptions={[]}
        pendingConfigOption={null}
        setConfigOption={() => {}}
        sessionUsage={null}
        availableCommands={COMMANDS}
        connected
        turnActive={false}
        enqueuePrompt={() => {}}
        promptCapabilities={null}
        pendingAttachments={[]}
        setPendingAttachments={() => {}}
      />
    </AssistantRuntimeProvider>
  );
}

/** Type `value` with the caret at `caret`, the way a user would: the
 *  caret is positioned before the input event so assistant-ui's
 *  `ComposerInput.onChange` reads the right `selectionStart`. RTL's
 *  `fireEvent.change` cannot express this, because assigning `value`
 *  parks the caret at the end. */
async function typeAt(ta: HTMLTextAreaElement, value: string, caret: number) {
  const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
  setter?.call(ta, value);
  ta.setSelectionRange(caret, caret);
  fireEvent.input(ta);
  await flush();
}

/** assistant-ui applies composer writes on a scheduled task, so nothing
 *  is observable in the DOM until that task has run. */
async function flush() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

async function pick(label: RegExp) {
  const [item] = option(label);
  if (!item) throw new Error(`popover item ${label} not rendered`);
  fireEvent.click(item);
  await flush();
}

function textarea(container: HTMLElement): HTMLTextAreaElement {
  const ta = container.querySelector("textarea");
  if (!ta) throw new Error("composer textarea not rendered");
  return ta;
}

const option = (text: RegExp) => screen.queryAllByRole("option").filter((el) => text.test(el.textContent ?? ""));

beforeEach(() => {
  window.localStorage.clear();
  vi.stubGlobal(
    "matchMedia",
    vi.fn().mockImplementation((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    })),
  );
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, json: async () => ({ files: [] }) }));
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  window.localStorage.clear();
});

describe("slash command completion (#3418)", () => {
  it("closes the popover on accept, so the next keystroke is not swallowed", async () => {
    const { container } = render(<Harness />);
    const ta = textarea(container);

    await typeAt(ta, "/addr", 5);
    expect(option(/address-pr-comments/)).toHaveLength(1);

    // Premise check: jsdom must not be resyncing the caret for us. In a
    // real browser it usually does, which is what makes the corruption
    // in this issue intermittent rather than constant.
    let selectionChanges = 0;
    const count = () => selectionChanges++;
    document.addEventListener("selectionchange", count);
    await pick(/address-pr-comments/);
    document.removeEventListener("selectionchange", count);

    expect(ta.value).toBe("/address-pr-comments ");
    expect(selectionChanges, "jsdom now resyncs the caret; this test no longer reproduces #3418").toBe(0);

    // The load-bearing assertion. While the popover is open its keyboard
    // handler owns Enter and Tab, so the user's send keystroke re-picks
    // the highlighted item instead, and that second selection slices the
    // live text with a stale trigger range, which is what produced
    // `ess-pr-comments /address-pr-comments`.
    expect(option(/address-pr-comments/)).toHaveLength(0);
  });

  it("inserts the command at the caret, not at the end of the buffer", async () => {
    const { container } = render(<Harness />);
    const ta = textarea(container);

    await typeAt(ta, "fix /he the bug", 7);
    await pick(/\/help/);

    expect(ta.value).toBe("fix /help the bug");
    expect(ta.selectionStart).toBe(10);
  });
});

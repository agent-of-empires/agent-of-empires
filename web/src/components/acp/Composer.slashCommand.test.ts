// @vitest-environment jsdom
//
// Regression test for #1512: picking a no-arg slash command (e.g.
// `/help`) used to leave the composer text as `/help` with no trailing
// whitespace. assistant-ui's `detectTrigger` scans backward from the
// cursor and halts on whitespace; without a trailing space the cursor
// sits inside the `/help` range and the popover re-opens immediately,
// trapping the next Enter as "re-pick highlighted item" instead of
// sending the prompt. The fix in `insertSlashCommand` is to always
// append a trailing space, for both `acceptsInput=true` (cosmetic,
// positions cursor for arg typing) and `acceptsInput=false` (forces
// the trigger detector to give up so Enter routes to the send path).
//
// Also locks the args-command behaviour so a future refactor cannot
// silently regress the cursor-at-end-after-space contract for the
// `acceptsInput=true` branch.

import { describe, expect, it, vi } from "vitest";

import { insertSlashCommand, type ComposerClient } from "./Composer";

type RuntimeStub = ComposerClient;

function makeRuntime(initialText: string) {
  const setText = vi.fn<(s: string) => void>();
  const runtime = {
    getState: () => ({ text: initialText }),
    setText,
  } as unknown as RuntimeStub;
  return { runtime, setText };
}

function makeItem(id: string, acceptsInput: boolean) {
  return {
    id,
    type: "command" as const,
    label: `/${id}`,
    description: "",
    acceptsInput,
  } as unknown as Parameters<typeof insertSlashCommand>[1];
}

describe("insertSlashCommand (#1512)", () => {
  it("appends a trailing space when picking a no-arg command (acceptsInput=false)", () => {
    const { runtime, setText } = makeRuntime("");
    insertSlashCommand(runtime, makeItem("help", false));
    expect(setText).toHaveBeenCalledExactlyOnceWith("/help ");
  });

  it("appends a trailing space when picking an args command (acceptsInput=true)", () => {
    const { runtime, setText } = makeRuntime("");
    insertSlashCommand(runtime, makeItem("review", true));
    expect(setText).toHaveBeenCalledExactlyOnceWith("/review ");
  });

  it("preserves prior buffer text and inserts a separator space when needed", () => {
    const { runtime, setText } = makeRuntime("draft note");
    insertSlashCommand(runtime, makeItem("clear", false));
    expect(setText).toHaveBeenCalledExactlyOnceWith("draft note /clear ");
  });

  it("does not double up the separator when prior text already ends in a space", () => {
    const { runtime, setText } = makeRuntime("draft note ");
    insertSlashCommand(runtime, makeItem("clear", false));
    expect(setText).toHaveBeenCalledExactlyOnceWith("draft note /clear ");
  });

  // @assistant-ui/react 0.15 applies composer writes on a scheduled task, so
  // the popover's `removeOnExecute` strip has not landed yet when this runs
  // and the snapshot still carries the typed `/<query>`. Dropping it here is
  // what keeps the result `/help ` rather than `/he /help `.
  it("drops a typed query token the popover has not stripped from the snapshot yet", () => {
    const cases: [string, string][] = [
      ["/he", "/help "],
      ["draft note /he", "draft note /help "],
      ["draft note /he ", "draft note /help "],
    ];
    for (const [initial, expected] of cases) {
      const { runtime, setText } = makeRuntime(initial);
      insertSlashCommand(runtime, makeItem("help", false));
      expect(setText, initial).toHaveBeenCalledExactlyOnceWith(expected);
    }
  });

  it("is a no-op when the runtime is null", () => {
    const setText = vi.fn();
    insertSlashCommand(null as unknown as RuntimeStub, makeItem("help", false));
    expect(setText).not.toHaveBeenCalled();
  });
});

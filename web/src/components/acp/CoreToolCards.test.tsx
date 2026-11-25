// @vitest-environment jsdom

import { afterEach, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";

vi.mock("../../lib/diffPair", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/diffPair")>();
  return { ...actual, diffPair: vi.fn(actual.diffPair) };
});
vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "dark-plus", appearance: "dark" }),
}));

import { diffPair } from "../../lib/diffPair";
import { AgentProfileProvider } from "../../lib/agentProfileContext";
import { AcpFileRefContext } from "./AcpFileRefContext";
import { EditToolCard } from "./CoreToolCards";
import { makeToolCall } from "./__fixtures__/toolCalls";

afterEach(cleanup);

const edit = (text: string) => {
  const diffs = [{ path: "a.rs", old_text: "a", new_text: text, created_at: "t" }];
  return makeToolCall({ kind: "edit", args_preview: JSON.stringify({ _aoe_diffs: diffs }), diffs });
};

const ui = (tool: ReturnType<typeof edit>) => (
  <AcpFileRefContext.Provider value={{ fileRefSession: null, onOpenFileRef: undefined }}>
    <AgentProfileProvider toolKey={null}>
      <EditToolCard tool={tool} />
    </AgentProfileProvider>
  </AcpFileRefContext.Provider>
);

it("does not re-run diffPair for a new-but-equal diffs array, and does when args_preview changes", () => {
  const { rerender } = render(ui(edit("b")));
  expect(diffPair).toHaveBeenCalledTimes(1);
  rerender(ui(edit("b")));
  expect(diffPair).toHaveBeenCalledTimes(1);
  rerender(ui(edit("bc")));
  expect(diffPair).toHaveBeenCalledTimes(2);
});

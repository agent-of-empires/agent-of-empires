// @vitest-environment jsdom
//
// #3094 / #3087: the idle desktop Enter path does NOT go through the
// composer's `sendFromTextarea`. `decideEnterAction` returns "default" when
// the turn is idle, so assistant-ui's own keymap submits through the runtime's
// `onNew`. That path used to leave the persisted text draft to the composer's
// 250ms debounced flush, so a remount racing the resume/queue churn re-seeded
// the just-sent text. `onNew` must drop the draft itself.

import { act, render } from "@testing-library/react";
import { useThreadRuntime } from "@assistant-ui/react";
import { describe, expect, it, vi } from "vitest";

import { emptyAcpState } from "../../../lib/acpTypes";

// Resolves only when the test releases it, so the assertions can run while the
// send is still pending: that is the unmount-during-send window where a draft
// left behind would rehydrate on remount.
let releaseSend: (() => void) | null = null;
const sendPrompt = vi.fn(
  () =>
    new Promise<void>((resolve) => {
      releaseSend = resolve;
    }),
);

// AcpRuntime drives everything off the useAcpSession store; mock it so the
// test exercises the runtime's onNew wiring, not the WS machinery.
vi.mock("../../../hooks/useAcpSession", () => ({
  useAcpSession: () => ({
    state: emptyAcpState(),
    status: "open",
    hasEverOpened: true,
    reconnecting: false,
    retryCount: 0,
    retryCountdown: 0,
    maxRetries: 5,
    manualReconnect: () => {},
    resolveApproval: async () => {},
    resolveElicitation: async () => {},
    sendPrompt,
    cancelPrompt: async () => {},
    forceEndTurn: async () => {},
    lastActivityRef: { current: 0 },
    dismissError: () => {},
    dismissPrimer: () => {},
    removeQueuedPrompt: () => {},
    editQueuedPrompt: () => {},
    clearQueue: () => {},
    dismissRejectedPrompt: () => {},
    dismissModeSwitchFailed: () => {},
    setConfigOption: async () => {},
    dismissConfigOptionSwitchFailed: () => {},
  }),
}));

import { AcpRuntime } from "../AcpRuntime";

// Drives the runtime's onNew the same way assistant-ui's built-in
// Enter-to-send keymap does.
function Sender({ text }: { text: string }) {
  const thread = useThreadRuntime();
  return (
    <button
      type="button"
      onClick={() => {
        void thread.append({ role: "user", content: [{ type: "text", text }] });
      }}
    >
      append
    </button>
  );
}

describe("AcpRuntime onNew draft clearing (#3094/#3087)", () => {
  it("clears the persisted text draft and staged attachments before the send resolves", async () => {
    window.localStorage.setItem("acp:draft:sess-onnew", "sent via idle enter");
    window.localStorage.setItem(
      "acp:draft-attachments:sess-onnew",
      JSON.stringify([{ kind: "image", mimeType: "image/png", dataB64: "aA==", name: "shot.png" }]),
    );

    const view = render(<AcpRuntime sessionId="sess-onnew">{() => <Sender text="sent via idle enter" />}</AcpRuntime>);

    await act(async () => {
      view.getByText("append").click();
    });

    // Still mid-send: both keys must already be gone, otherwise an unmount
    // here would rehydrate the just-sent text and image on remount.
    expect(sendPrompt).toHaveBeenCalledOnce();
    expect(releaseSend).not.toBeNull();
    expect(window.localStorage.getItem("acp:draft:sess-onnew")).toBeNull();
    expect(window.localStorage.getItem("acp:draft-attachments:sess-onnew")).toBeNull();
    // The staged attachment still rides the send itself.
    expect(sendPrompt.mock.calls[0]?.[1]).toHaveLength(1);

    await act(async () => {
      releaseSend?.();
    });
    window.localStorage.clear();
  });
});

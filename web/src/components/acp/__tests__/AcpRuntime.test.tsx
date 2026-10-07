// @vitest-environment jsdom
import { act, render } from "@testing-library/react";
import { useAui } from "@assistant-ui/react";
import { useEffect } from "react";
import { describe, expect, it, vi } from "vitest";

import { emptyAcpState, type ActivityRow } from "../../../lib/acpTypes";

const session = vi.hoisted(() => ({
  activity: [] as ActivityRow[],
  releaseSend: null as (() => void) | null,
}));
const sendPrompt = vi.hoisted(() =>
  vi.fn(
    () =>
      new Promise<void>((resolve) => {
        session.releaseSend = resolve;
      }),
  ),
);

vi.mock("../../../hooks/useAcpSession", () => ({
  useAcpSession: () => ({
    state: { ...emptyAcpState(), activity: session.activity },
    sendPrompt,
    cancelPrompt: async () => {},
    forceEndTurn: async () => {},
  }),
}));

import { AcpRuntime } from "../AcpRuntime";

const row = (id: string, kind: ActivityRow["kind"], text: string): ActivityRow => ({
  id,
  kind,
  text,
  at: "2026-08-31T12:00:00Z",
});

describe("AcpRuntime", () => {
  // A `/clear` shortens a kept message's parts; a stale store would read past them.
  it("rebuilds the runtime when a clear lands, and keeps it across an ordinary turn", async () => {
    let mounts = 0;
    const CountMounts = () => {
      useEffect(() => {
        mounts += 1;
      }, []);
      return null;
    };
    const Harness = () => <AcpRuntime sessionId="s1">{() => <CountMounts />}</AcpRuntime>;
    session.activity = [row("u1", "user_prompt", "q"), row("a1", "message", "a")];
    const { rerender } = render(<Harness />);
    const initial = mounts;

    session.activity = [...session.activity, row("a2", "message", "more")];
    await act(async () => rerender(<Harness />));
    expect(mounts, "an ordinary turn must not rebuild the runtime").toBe(initial);

    session.activity = [...session.activity, row("c1", "session_cleared", "Conversation cleared")];
    await act(async () => rerender(<Harness />));
    expect(mounts, "a clear must rebuild the runtime").toBe(initial + 1);
  });

  // The idle Enter path submits through onNew, not the composer, and an unmount
  // mid-send must not rehydrate the sent text or image.
  it("clears the text draft and staged attachments before the send resolves", async () => {
    session.activity = [];
    window.localStorage.setItem("acp:draft:sess-onnew", "sent via idle enter");
    window.localStorage.setItem(
      "acp:draft-attachments:sess-onnew",
      JSON.stringify([{ kind: "image", mimeType: "image/png", dataB64: "aA==", name: "shot.png" }]),
    );
    function Sender() {
      const thread = useAui().thread;
      return (
        <button
          type="button"
          onClick={() => void thread.append({ role: "user", content: [{ type: "text", text: "sent via idle enter" }] })}
        >
          append
        </button>
      );
    }
    const view = render(<AcpRuntime sessionId="sess-onnew">{() => <Sender />}</AcpRuntime>);
    await act(async () => view.getByText("append").click());

    expect(sendPrompt).toHaveBeenCalledOnce();
    expect(session.releaseSend).not.toBeNull();
    expect(window.localStorage.getItem("acp:draft:sess-onnew")).toBeNull();
    expect(window.localStorage.getItem("acp:draft-attachments:sess-onnew")).toBeNull();
    expect(sendPrompt.mock.calls[0]?.[1]).toHaveLength(1);

    await act(async () => session.releaseSend?.());
    window.localStorage.clear();
  });
});

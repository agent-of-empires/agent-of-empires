// @vitest-environment jsdom
import { act, render } from "@testing-library/react";
import { useAui } from "@assistant-ui/react";
import { useEffect } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { setDraftAttachments, getDraftAttachments } from "../../../lib/acpDrafts";
import { toastBus } from "../../../lib/toastBus";
import { emptyAcpState, type ActivityRow, type PromptAttachmentInput } from "../../../lib/acpTypes";

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

// Gates and counts the restore read/save so ordering is controlled, never slept on.
const store = vi.hoisted(() => ({
  gate: null as Promise<void> | null,
  fail: false,
  reads: 0,
  saves: 0,
  lastRead: Promise.resolve() as Promise<unknown>,
}));
vi.mock("../../../lib/attachmentStore", async (importOriginal) => {
  const real = await importOriginal<typeof import("../../../lib/attachmentStore")>();
  return {
    ...real,
    loadAttachments: (id: string) => {
      store.reads += 1;
      const read = (async () => {
        await store.gate;
        if (store.fail) throw new Error("read failed");
        return real.loadAttachments(id);
      })();
      store.lastRead = read.catch(() => {});
      return read;
    },
    saveAttachments: (...args: Parameters<typeof real.saveAttachments>) => {
      store.saves += 1;
      return real.saveAttachments(...args);
    },
  };
});

vi.mock("../../../hooks/useAcpSession", () => ({
  useAcpSession: () => ({
    state: { ...emptyAcpState(), activity: session.activity },
    sendPrompt,
    cancelPrompt: async () => true,
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

afterEach(async () => {
  store.gate = null;
  store.fail = false;
  store.reads = 0;
  store.saves = 0;
  toastBus.handler = null;
  window.localStorage.clear();
  const { listAttachmentSessionIds, deleteAttachments } = await import("../../../lib/attachmentStore");
  await Promise.all((await listAttachmentSessionIds()).map((id) => deleteAttachments(id)));
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

  const stored = { kind: "image" as const, mimeType: "image/png", dataB64: "c3RvcmVk", name: "stored.png" };
  const staged = { kind: "image" as const, mimeType: "image/png", dataB64: "c3RhZ2Vk", name: "staged.png" };

  type Ctx = {
    pendingAttachments: PromptAttachmentInput[];
    setPendingAttachments: (u: (p: PromptAttachmentInput[]) => PromptAttachmentInput[]) => void;
  };
  function Panel({ ctx }: { ctx: Ctx }) {
    const thread = useAui().thread;
    return (
      <>
        <span data-testid="names">{ctx.pendingAttachments.map((a) => a.name).join(",")}</span>
        <button
          type="button"
          onClick={() => void thread.append({ role: "user", content: [{ type: "text", text: "go" }] })}
        >
          append
        </button>
        <button type="button" onClick={() => ctx.setPendingAttachments((p) => [...p, staged])}>
          stage
        </button>
      </>
    );
  }
  const mount = (id: string) => render(<AcpRuntime sessionId={id}>{(ctx) => <Panel ctx={ctx} />}</AcpRuntime>);
  const names = (view: ReturnType<typeof mount>) => view.getByTestId("names").textContent;
  const click = (view: ReturnType<typeof mount>, label: string) => act(async () => view.getByText(label).click());
  // The next microtasks and the macrotask after them: no IndexedDB or React work can still be pending.
  const flush = () =>
    act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
  const holdReads = () => {
    let release: () => void = () => {};
    store.gate = new Promise<void>((resolve) => (release = resolve));
    return () => {
      store.gate = null;
      release();
    };
  };

  // The idle Enter path submits through onNew, not the composer, and an unmount
  // mid-send must not rehydrate the sent text or image.
  it("clears the text draft and the restored attachments before the send resolves", async () => {
    session.activity = [];
    sendPrompt.mockClear();
    window.localStorage.setItem("acp:draft:sess-onnew", "sent via idle enter");
    await setDraftAttachments("sess-onnew", [stored]);
    const view = mount("sess-onnew");
    await vi.waitFor(() => expect(names(view)).toBe("stored.png"));
    await click(view, "append");

    expect(sendPrompt).toHaveBeenCalledOnce();
    expect(session.releaseSend).not.toBeNull();
    expect(window.localStorage.getItem("acp:draft:sess-onnew")).toBeNull();
    expect(window.localStorage.getItem("acp:draft-attachments-v2:sess-onnew")).not.toMatch(/^S:/);
    expect(sendPrompt.mock.calls[0]?.[1]).toEqual([stored]);
    await vi.waitFor(async () => expect(await getDraftAttachments("sess-onnew")).toEqual([]));
    await act(async () => session.releaseSend?.());
  });

  it("keeps an unseen stored draft when a send lands before the restore read resolves", async () => {
    session.activity = [];
    sendPrompt.mockClear();
    await setDraftAttachments("sess-early", [stored]);
    const release = holdReads();
    const view = mount("sess-early");
    await click(view, "append");
    expect(sendPrompt.mock.calls[0]?.[1]).toEqual([]);

    release();
    await vi.waitFor(() => expect(names(view)).toBe("stored.png"));
    expect(window.localStorage.getItem("acp:draft-attachments-v2:sess-early")).toMatch(/^S:/);
    expect(await getDraftAttachments("sess-early")).toEqual([stored]);

    await click(view, "append");
    expect(sendPrompt.mock.calls[1]?.[1]).toEqual([stored]);
    await act(async () => session.releaseSend?.());
  });

  it("restores stored attachments ahead of ones staged while the read was in flight", async () => {
    session.activity = [];
    await setDraftAttachments("sess-race", [stored]);
    const release = holdReads();
    const view = mount("sess-race");
    await click(view, "stage");
    expect(names(view)).toBe("staged.png");

    release();
    await vi.waitFor(() => expect(names(view)).toBe("stored.png,staged.png"));
    await vi.waitFor(async () =>
      expect((await getDraftAttachments("sess-race"))?.map((a) => a.name)).toEqual(["stored.png", "staged.png"]),
    );
  });

  it("persists a file staged before navigating away mid-restore", async () => {
    session.activity = [];
    await setDraftAttachments("sess-away", [stored]);
    const release = holdReads();
    const first = mount("sess-away");
    await click(first, "stage");
    first.unmount();
    release();
    await vi.waitFor(async () =>
      expect((await getDraftAttachments("sess-away"))?.map((a) => a.name)).toEqual(["stored.png", "staged.png"]),
    );
    const again = mount("sess-away");
    await vi.waitFor(() => expect(names(again)).toBe("stored.png,staged.png"));
  });

  it("a replacement mount waits for the first mount's reconciliation, so its next send carries both", async () => {
    session.activity = [];
    sendPrompt.mockClear();
    await setDraftAttachments("sess-remount", [stored]);
    const release = holdReads();
    const first = mount("sess-remount");
    await click(first, "stage");
    first.unmount();
    // The replacement mounts while the first read is still pending: its restore must queue behind it.
    const second = mount("sess-remount");
    release();
    await vi.waitFor(() => expect(names(second)).toBe("stored.png,staged.png"));
    await click(second, "append");
    expect(sendPrompt.mock.calls[0]?.[1]).toEqual([stored, staged]);
    await act(async () => session.releaseSend?.());
  });

  it("keeps what was just staged when the stored draft is near the cap", async () => {
    session.activity = [];
    const older = Array.from({ length: 7 }, (_, i) => ({ ...stored, name: `old${i}.png` }));
    await setDraftAttachments("sess-cap", older);
    const release = holdReads();
    const view = mount("sess-cap");
    await click(view, "stage");
    await click(view, "stage");
    release();
    await vi.waitFor(() =>
      expect(names(view)).toBe("old0.png,old1.png,old2.png,old3.png,old4.png,old5.png,staged.png,staged.png"),
    );
  });

  it("does not overwrite the stored draft after a failed restore read", async () => {
    session.activity = [];
    await setDraftAttachments("sess-readfail", [stored]);
    store.fail = true;
    const errors: string[] = [];
    toastBus.handler = { push: () => {}, error: (m) => void errors.push(m), info: () => {}, openLink: () => {} };
    const view = mount("sess-readfail");
    await vi.waitFor(() => expect(store.reads).toBe(1));
    await store.lastRead;
    await flush();
    const savesBefore = store.saves;

    await click(view, "stage");
    await flush();
    expect(store.saves).toBe(savesBefore);
    expect(errors).toEqual([expect.stringMatching(/could not load saved attachments/i)]);
    store.fail = false;
    expect((await getDraftAttachments("sess-readfail"))?.map((a) => a.name)).toEqual(["stored.png"]);
  });
});

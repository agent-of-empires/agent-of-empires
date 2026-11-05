// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { clearAcpCache } from "../../../hooks/useAcpSession";
import {
  currentControlSnapshot,
  flushAsync,
  installAcpFakes,
  json,
  lastSocket,
  profileWrapper,
  type Call,
  type Route,
} from "../../../hooks/__tests__/acpHarness";
import type * as AttachmentStore from "../../../lib/attachmentStore";
import { deleteAttachments, listAttachmentSessionIds } from "../../../lib/attachmentStore";
import { getDraft, getDraftAttachments, setDraftAttachments } from "../../../lib/acpDrafts";
import { canSubmitPromptIntent, nativeInputAvailable, type SessionRecovery } from "../../../lib/acpTypes";
import { AcpRuntime, type AcpContext } from "../AcpRuntime";
import { Composer } from "../Composer";

const hydration = vi.hoisted(() => ({
  gate: null as Promise<void> | null,
  entered: null as (() => void) | null,
  finished: null as Promise<unknown> | null,
  release: null as (() => void) | null,
}));
vi.mock("../../../lib/attachmentStore", async (importOriginal) => {
  const real = await importOriginal<typeof AttachmentStore>();
  return {
    ...real,
    loadAttachments: (id: string) => {
      const gate = hydration.gate;
      const read = (async () => {
        const stored = await real.loadAttachments(id);
        hydration.entered?.();
        await gate;
        return stored;
      })();
      hydration.finished = read;
      return read;
    },
  };
});

const recovery: SessionRecovery = {
  reason: "operation_failed",
  diagnostic: "Provider rejected the operation.",
  evidence: null,
  runtime: "acp_transport_ready",
  work_in_progress: false,
  uncertainty: null,
  guidance: "Compose explicit input for the existing Agent.",
  actions: { attach: true, retry_operation: true, restart_same: false, stop: true, fresh: false },
  expected: "original-selection",
};
const stored = { kind: "image" as const, mimeType: "image/png", dataB64: "b2xk", name: "stored.png" };
let calls: Call[];
let current: AcpContext;
let promptRoute: Route;
const queued = [{ id: "queued-original", seq: 1, text: "server queued input", created_at: "2026-01-01T00:00:00Z" }];
const posts = () => calls.filter((call) => call.method === "POST" && call.url.endsWith("/acp/prompt"));
const queueWrites = () => calls.filter((call) => call.method !== "GET" && call.url.includes("/queue"));

beforeEach(() => {
  localStorage.clear();
  clearAcpCache();
  promptRoute = () => json({ disposition: "sent" }, 202);
  calls = installAcpFakes((call) => {
    if (call.method === "POST" && call.url.endsWith("/acp/prompt")) return promptRoute(call);
    if (call.method === "GET" && call.url.endsWith("/queue")) return json(queued);
    if (call.url === "/api/agents") return json([]);
    if (call.url.endsWith("/files")) return json({ files: [] });
    return undefined;
  });
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener() {},
    removeEventListener() {},
    addListener() {},
    removeListener() {},
    dispatchEvent() {
      return false;
    },
  }));
});

afterEach(async () => {
  cleanup();
  hydration.release?.();
  await hydration.finished;
  hydration.release = null;
  hydration.gate = null;
  hydration.entered = null;
  hydration.finished = null;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  clearAcpCache();
  await Promise.all((await listAttachmentSessionIds()).map((id) => deleteAttachments(id)));
  localStorage.clear();
});

async function mount(id: string) {
  const view = render(
    <AcpRuntime sessionId={id}>
      {(ctx) => {
        current = ctx;
        return (
          <>
            <button type="button" onClick={ctx.beginOperationRetry}>
              Retry
            </button>
            {ctx.operationRetryActive && (
              <button type="button" onClick={ctx.cancelOperationRetry}>
                Cancel retry
              </button>
            )}
            <Composer
              sessionId={id}
              currentAgent={ctx.state.agent}
              currentProvider={null}
              availableModes={ctx.state.availableModes}
              currentModeId={ctx.state.currentModeId}
              legacyMode={ctx.state.mode}
              configOptions={ctx.state.configOptions}
              pendingConfigOption={ctx.state.pendingConfigOption}
              setConfigOption={ctx.setConfigOption}
              sessionUsage={ctx.state.sessionUsage}
              authStatus={ctx.state.authStatus}
              availableCommands={ctx.state.availableCommands}
              connected={ctx.status === "open" && nativeInputAvailable(ctx.state)}
              canForceEndTurn={ctx.canForceEndTurn}
              stopTurn={ctx.stopTurn}
              canSubmit={ctx.status === "open" && canSubmitPromptIntent(ctx.state)}
              turnActive={ctx.state.turnActive}
              enqueuePrompt={ctx.sendPrompt}
              promptCapabilities={{ image: true, audio: false, embeddedContext: false }}
              pendingAttachments={ctx.pendingAttachments}
              setPendingAttachments={ctx.setPendingAttachments}
              operationRetryActive={ctx.operationRetryActive}
              compositionControlRef={ctx.compositionControlRef}
              attachmentReadGenerationRef={ctx.attachmentReadGenerationRef}
              queuedPrompts={ctx.state.queuedPrompts}
              editQueuedPrompt={ctx.editQueuedPrompt}
            />
          </>
        );
      }}
    </AcpRuntime>,
    { wrapper: profileWrapper },
  );
  await flushAsync();
  lastSocket().open();
  await flushAsync();
  lastSocket().message(currentControlSnapshot(current.state, { ready: true, recovery }));
  await flushAsync();
  await waitFor(() => expect(current.state.queuedPrompts.map((entry) => entry.id)).toEqual(["queued-original"]));
  return { ...view, textarea: () => view.container.querySelector("textarea")! };
}

function holdFileReads() {
  const reads: { reader: FileReader; file: Blob }[] = [];
  vi.spyOn(FileReader.prototype, "readAsDataURL").mockImplementation(function (this: FileReader, file: Blob) {
    reads.push({ reader: this, file });
  });
  return {
    reads,
    release: async (index: number, data: string) => {
      const { reader } = reads[index]!;
      await act(async () => {
        Object.defineProperty(reader, "result", { configurable: true, value: `data:image/png;base64,${data}` });
        reader.dispatchEvent(new ProgressEvent("load"));
      });
    },
  };
}

function paste(textarea: HTMLTextAreaElement, name: string) {
  const file = new File([name], name, { type: "image/png" });
  fireEvent.paste(textarea, { clipboardData: { items: [{ kind: "file", getAsFile: () => file }] } });
}

it("isolates Retry from already-read stored files and unfinished file encoding, but accepts deliberate new files", async () => {
  const id = "retry-attachment-isolation";
  await setDraftAttachments(id, [stored]);
  const entered = Promise.withResolvers<void>();
  const held = Promise.withResolvers<void>();
  hydration.gate = held.promise;
  hydration.release = held.resolve;
  hydration.entered = entered.resolve;
  const files = holdFileReads();
  const view = await mount(id);
  await entered.promise;
  fireEvent.change(view.textarea(), { target: { value: "old unsent text" } });
  paste(view.textarea(), "old-reading.png");
  expect(files.reads).toHaveLength(1);

  fireEvent.click(view.getByRole("button", { name: "Retry", exact: true }));
  expect(view.textarea().value).toBe("");
  expect(current.pendingAttachments).toEqual([]);
  expect(posts()).toEqual([]);
  paste(view.textarea(), "new.png");
  expect(files.reads).toHaveLength(2);
  await files.release(1, "bmV3");
  await waitFor(() => expect(current.pendingAttachments.map((file) => file.name)).toEqual(["new.png"]));
  await files.release(0, "b2xk");
  await act(async () => {
    hydration.gate = null;
    held.resolve();
    await hydration.finished;
  });
  await flushAsync();
  expect(current.pendingAttachments.map((file) => file.name)).toEqual(["new.png"]);
  expect((await getDraftAttachments(id))?.map((file) => file.name)).toEqual(["new.png"]);

  fireEvent.change(view.textarea(), { target: { value: "new retry input" } });
  fireEvent.keyDown(view.textarea(), { key: "Enter" });
  await waitFor(() => expect(current.operationRetryActive).toBe(false));
  expect(posts()).toHaveLength(1);
  const body = JSON.parse(posts()[0]!.body!);
  expect(body.text).toBe("new retry input");
  expect(body.no_revive).toBe(true);
  expect(body.attachments).toEqual([{ kind: "image", mime_type: "image/png", name: "new.png", data: "bmV3" }]);
  expect(queueWrites()).toEqual([]);
  expect(await getDraftAttachments(id)).toEqual([]);
});

it("leaves queue recall before Retry and never edits or removes the recalled server entry", async () => {
  const view = await mount("retry-queue-recall");
  fireEvent.change(view.textarea(), { target: { value: "ordinary draft" } });
  view.textarea().setSelectionRange(0, 0);
  fireEvent.keyDown(view.textarea(), { key: "ArrowUp" });
  await waitFor(() => expect(view.textarea().value).toBe("server queued input"));
  fireEvent.click(view.getByRole("button", { name: "Retry", exact: true }));
  expect(view.textarea().value).toBe("");
  fireEvent.keyDown(view.textarea(), { key: "ArrowUp" });
  expect(view.textarea().value).toBe("");
  fireEvent.change(view.textarea(), { target: { value: "explicit retry instead of queue edit" } });
  fireEvent.click(view.getByRole("button", { name: "Send message" }));
  await waitFor(() => expect(current.operationRetryActive).toBe(false));
  expect(posts()).toHaveLength(1);
  expect(JSON.parse(posts()[0]!.body!).text).toBe("explicit retry instead of queue edit");
  expect(queueWrites()).toEqual([]);
  expect(current.state.queuedPrompts.map(({ id, text }) => ({ id, text }))).toEqual([
    { id: "queued-original", text: "server queued input" },
  ]);
  fireEvent.change(view.textarea(), { target: { value: "ordinary input after retry" } });
  fireEvent.keyDown(view.textarea(), { key: "Enter" });
  await waitFor(() => expect(posts()).toHaveLength(2));
  expect(JSON.parse(posts()[1]!.body!).text).toBe("ordinary input after retry");
  expect(queueWrites()).toEqual([]);
});

it("retains a refused Retry until explicit cancel, then permits ordinary input without replay", async () => {
  const response = Promise.withResolvers<Response>();
  promptRoute = () => response.promise;
  const files = holdFileReads();
  const id = "retry-refusal-cancel";
  const view = await mount(id);
  fireEvent.click(view.getByRole("button", { name: "Retry", exact: true }));
  fireEvent.change(view.textarea(), { target: { value: "keep this refused retry" } });
  paste(view.textarea(), "retry.png");
  await files.release(0, "cmV0cnk=");
  fireEvent.click(view.getByRole("button", { name: "Send message" }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(view.textarea().value).toBe("keep this refused retry");
  expect(current.pendingAttachments.map((file) => file.name)).toEqual(["retry.png"]);
  await act(async () => {
    response.resolve(new Response("worker_not_ready: existing Agent unavailable", { status: 503 }));
    await response.promise;
  });
  await waitFor(() => expect(current.state.interactionError).not.toBeNull());
  expect(view.textarea().value).toBe("keep this refused retry");
  expect(current.pendingAttachments.map((file) => file.name)).toEqual(["retry.png"]);
  fireEvent(window, new Event("pagehide"));
  expect(getDraft(id)).toBe("keep this refused retry");
  expect((await getDraftAttachments(id))?.map((file) => file.name)).toEqual(["retry.png"]);

  lastSocket().message(currentControlSnapshot(current.state, { ready: true, recovery: null }));
  await flushAsync();
  fireEvent.keyDown(view.textarea(), { key: "Enter" });
  await flushAsync();
  expect(posts()).toHaveLength(1);
  expect(current.operationRetryActive).toBe(true);
  expect(view.textarea().value).toBe("keep this refused retry");
  expect(current.pendingAttachments.map((file) => file.name)).toEqual(["retry.png"]);
  expect(queueWrites()).toEqual([]);
  expect(calls.filter((call) => call.method === "PATCH" && call.url.endsWith("/snooze"))).toEqual([]);

  paste(view.textarea(), "cancelled-read.png");
  fireEvent.click(view.getByRole("button", { name: "Cancel retry" }));
  await files.release(1, "Y2FuY2VsbGVk");
  expect(current.operationRetryActive).toBe(false);
  expect(view.textarea().value).toBe("");
  expect(current.pendingAttachments).toEqual([]);
  expect(getDraft(id)).toBe("");
  expect(await getDraftAttachments(id)).toEqual([]);
  expect(posts()).toHaveLength(1);
  expect(queueWrites()).toEqual([]);

  promptRoute = () => json({ disposition: "sent" }, 202);
  fireEvent.change(view.textarea(), { target: { value: "ordinary new input" } });
  fireEvent.keyDown(view.textarea(), { key: "Enter" });
  await waitFor(() => expect(posts()).toHaveLength(2));
  expect(JSON.parse(posts()[1]!.body!).text).toBe("ordinary new input");
  expect(JSON.parse(posts()[1]!.body!).attachments ?? []).toEqual([]);
  expect(queueWrites()).toEqual([]);
});

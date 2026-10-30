// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LiveTerminalView } from "../LiveTerminalView";
import { TerminalView } from "../TerminalView";
import { PairedShellPane } from "../PairedTerminal";
import { installResizeObserver } from "./liveTerminalHarness";
import { clearMobileKeyboardProxyInput, deliverMobileKeyboardProxyInput } from "../../lib/mobileKeyboardProxy";
import { makeSession } from "./fixtures";

const { ensureSession, ensureTerminal, pasteImage, reportTelemetrySeen } = vi.hoisted(() => ({
  ensureSession: vi.fn<(...args: unknown[]) => Promise<{ ok: boolean; message?: string | null }>>(async () => ({
    ok: true,
    message: null,
  })),
  ensureTerminal: vi.fn(async () => ({ ok: true })),
  pasteImage: vi.fn<(id: string, file: File, canUpload: () => boolean) => Promise<string | null>>(),
  reportTelemetrySeen: vi.fn(),
}));

vi.mock("../../lib/token", () => ({ getToken: () => null }));
vi.mock("../../lib/deviceBinding", () => ({ getOrCreateDeviceBindingSecret: () => "test-secret" }));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => true }));
vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => ({ keyboardHeight: 0, keyboardOpen: false }),
}));
vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return {
    ensureSession,
    ensureTerminal,
    pasteImage: pasteImage.mockImplementation(actual.pasteImage),
    isStartRefusal: () => false,
    reportTelemetrySeen,
  };
});
const resize = installResizeObserver();

class FakeWS {
  static sockets: FakeWS[] = [];
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  readyState = 0;
  binaryType = "blob";
  onopen: ((e: Event) => void) | null = null;
  onmessage: ((e: MessageEvent) => void) | null = null;
  onclose: ((e: CloseEvent) => void) | null = null;
  sent: Array<string | ArrayBufferView<ArrayBuffer>> = [];
  constructor(readonly url: string) {
    FakeWS.sockets.push(this);
  }
  send(data: string | ArrayBufferView<ArrayBuffer>) {
    this.sent.push(data);
  }
  open() {
    act(() => {
      this.readyState = FakeWS.OPEN;
      this.onopen?.({} as Event);
    });
  }
  deliver(payload: unknown) {
    act(() => this.onmessage?.({ data: JSON.stringify(payload) } as MessageEvent));
  }
  controls() {
    return this.sent
      .filter((data): data is string => typeof data === "string")
      .map((data) => JSON.parse(data) as Record<string, unknown>);
  }
  close() {
    this.readyState = FakeWS.CLOSED;
  }
}

beforeEach(() => {
  FakeWS.sockets.length = 0;
  ensureSession.mockReset().mockResolvedValue({ ok: true, message: null });
  ensureTerminal.mockClear();
  pasteImage.mockClear();
  reportTelemetrySeen.mockClear();
  vi.stubGlobal("WebSocket", FakeWS);
});
afterEach(() => {
  cleanup();
  resize.clear();
  delete (window as { isSecureContext?: boolean }).isSecureContext;
  delete (navigator as { clipboard?: unknown }).clipboard;
  vi.unstubAllGlobals();
});

describe("LiveTerminalView Restart with the real hook", () => {
  it("dials a new socket only after ensure resolves, and clears the exited strip once it opens", async () => {
    render(<LiveTerminalView readOnly={false} session={makeSession({ id: "restart" })} />);
    await waitFor(() => expect(FakeWS.sockets).toHaveLength(1));
    const dead = FakeWS.sockets[0]!;
    act(() => {
      dead.readyState = FakeWS.CLOSED;
      dead.onclose?.(new CloseEvent("close", { code: 4001 }));
    });

    fireEvent.click(await screen.findByRole("button", { name: "Restart" }));
    await waitFor(() => expect(ensureSession).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(FakeWS.sockets).toHaveLength(2));

    const fresh = FakeWS.sockets[1]!;
    act(() => {
      fresh.readyState = FakeWS.OPEN;
      fresh.onopen?.({} as Event);
    });
    await waitFor(() => expect(screen.queryByText("Agent exited")).toBeNull());
  });
});

const frame = (content: string, seq = 1) => ({
  type: "frame",
  seq,
  content: `${content}\n`,
  rows: 3,
  history: 0,
  cursor: null,
  altScreen: false,
  mouse: false,
  mouseSgr: false,
  pane0: null,
});
const mutationControls = (socket: FakeWS) =>
  socket.controls().filter(({ type }) => ["claim", "claim_if_vacant", "resize", "paste"].includes(String(type)));

describe("read-only live consumers with the real socket and terminal", () => {
  it.each(["agent", "host", "container"] as const)(
    "observes %s without ensuring or exposing writes",
    async (surface) => {
      const session = makeSession({ id: `observer-${surface}`, is_sandboxed: true });
      if (surface === "agent") render(<TerminalView session={session} readOnly />);
      else render(<PairedShellPane session={session} sessionId={session.id} readOnly />);
      if (surface === "container") fireEvent.click(screen.getByRole("button", { name: "Container" }));
      await waitFor(() => expect(FakeWS.sockets.length).toBe(surface === "container" ? 2 : 1));
      const socket = FakeWS.sockets.at(-1)!;
      expect(socket.url).toContain(
        surface === "agent"
          ? "/live-ws"
          : surface === "host"
            ? "/terminal/live-ws?index=0"
            : "/container-terminal/live-ws?index=0",
      );
      expect(ensureSession).not.toHaveBeenCalled();
      expect(ensureTerminal).not.toHaveBeenCalled();
      socket.open();
      socket.deliver({ type: "size_owner", is_owner: false });
      socket.deliver(frame(`passive-${surface}`));
      expect(await screen.findByText(`passive-${surface}`)).toBeTruthy();
      expect(screen.queryByRole("button", { name: /Take over/ })).toBeNull();
      expect(screen.queryByRole("button", { name: "Compose" })).toBeNull();
      expect(screen.queryByRole("button", { name: "Open keyboard" })).toBeNull();
      const input = screen.getByLabelText<HTMLTextAreaElement>("Live terminal input");
      expect(input.readOnly).toBe(true);
      fireEvent.keyDown(input, { key: "Enter" });
      fireEvent.paste(input, { clipboardData: { getData: () => "must not write", items: [] } });
      expect(mutationControls(socket)).toEqual([]);
      expect(socket.sent.every((data) => typeof data === "string")).toBe(true);
      expect(reportTelemetrySeen).not.toHaveBeenCalled();
      act(() => {
        socket.readyState = FakeWS.CLOSED;
        socket.onclose?.(new CloseEvent("close", { code: 4001 }));
      });
      expect(await screen.findByText("Agent exited")).toBeTruthy();
      expect(screen.queryByRole("button", { name: "Restart" })).toBeNull();
      expect(ensureSession).not.toHaveBeenCalled();
      expect(ensureTerminal).not.toHaveBeenCalled();
    },
  );

  it("keeps a compose draft through policy revocation and sends only a fresh writable action", async () => {
    const session = makeSession({ id: "policy-draft" });
    const view = render(<TerminalView session={session} readOnly={false} />);
    await waitFor(() => expect(FakeWS.sockets).toHaveLength(1));
    const socket = FakeWS.sockets[0]!;
    socket.open();
    socket.deliver({ type: "size_owner", is_owner: true });
    socket.deliver(frame("before-policy"));
    expect(await screen.findByRole("button", { name: "Open keyboard" })).toBeTruthy();
    fireEvent.click(await screen.findByRole("button", { name: "Compose" }));
    fireEvent.input(screen.getByLabelText("Message"), { target: { value: "keep this draft" } });
    const sent = socket.sent.length;
    view.rerender(<TerminalView session={session} readOnly />);
    expect(screen.queryByRole("button", { name: "Open keyboard" })).toBeNull();
    socket.deliver({ type: "size_owner", is_owner: false });
    socket.deliver(frame("after-policy", 2));
    expect(await screen.findByText("after-policy")).toBeTruthy();
    expect(screen.getByLabelText<HTMLTextAreaElement>("Message").value).toBe("keep this draft");
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Send" }).disabled).toBe(true);
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Insert" }).disabled).toBe(true);
    fireEvent.keyDown(screen.getByLabelText("Message"), { key: "Enter", ctrlKey: true });
    expect(socket.sent).toHaveLength(sent);
    expect(screen.queryByRole("button", { name: /Take over/ })).toBeNull();
    expect(ensureSession).toHaveBeenCalledTimes(1);
    view.rerender(<TerminalView session={session} readOnly={false} />);
    expect(socket.sent).toHaveLength(sent);
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(socket.controls().filter(({ type }) => type === "paste")).toEqual([]);
    expect(screen.getByLabelText<HTMLTextAreaElement>("Message").value).toBe("keep this draft");
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    fireEvent.click(screen.getByRole("button", { name: /Take over/ }));
    expect(socket.controls().at(-1)).toEqual({ type: "claim" });
    socket.deliver({ type: "size_owner", is_owner: true });
    fireEvent.click(screen.getByRole("button", { name: "Compose" }));
    expect(screen.getByLabelText<HTMLTextAreaElement>("Message").value).toBe("keep this draft");
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(socket.controls().filter(({ type }) => type === "paste")).toEqual([
      { type: "paste", text: "keep this draft", submit: true },
    ]);
    expect(screen.queryByRole("dialog", { name: "Compose" })).toBeNull();
  });

  it("refuses proxy edits during revocation instead of buffering them for writable restoration", async () => {
    clearMobileKeyboardProxyInput();
    const proxy = document.createElement("textarea");
    proxy.dataset.keyboardProxy = "";
    document.body.append(proxy);
    try {
      const session = makeSession({ id: "policy-proxy" });
      const view = render(<TerminalView session={session} readOnly={false} />);
      await waitFor(() => expect(FakeWS.sockets).toHaveLength(1));
      const socket = FakeWS.sockets[0]!;
      socket.open();
      socket.deliver({ type: "size_owner", is_owner: true });
      const sent = socket.sent.length;
      view.rerender(<TerminalView session={session} readOnly />);
      act(() => {
        expect(deliverMobileKeyboardProxyInput({ inputType: "edit", deleted: 0, data: "revoked" })).toBe(false);
      });
      expect(socket.sent).toHaveLength(sent);
      view.rerender(<TerminalView session={session} readOnly={false} />);
      socket.deliver({ type: "size_owner", is_owner: true });
      expect(socket.sent).toHaveLength(sent);
      act(() => {
        expect(deliverMobileKeyboardProxyInput({ inputType: "edit", deleted: 0, data: "fresh" })).toBe(true);
      });
      const bytes = socket.sent
        .filter((data): data is ArrayBufferView<ArrayBuffer> => typeof data !== "string")
        .map((data) => new TextDecoder().decode(data));
      expect(bytes).toEqual(["fresh"]);
    } finally {
      proxy.remove();
      clearMobileKeyboardProxyInput();
    }
  });

  it.each(["decoding", "response"] as const)(
    "never replays a revoked image gesture held during %s after writable restoration",
    async (phase) => {
      let finishDecode!: () => void;
      let decoded!: () => void;
      const decodingStarted = new Promise<void>((resolve) => {
        decoded = resolve;
      });
      vi.stubGlobal(
        "FileReader",
        class {
          result = "data:image/png;base64,aGk=";
          onload: (() => void) | null = null;
          readAsDataURL() {
            finishDecode = () => this.onload?.();
            decoded();
          }
        },
      );
      let finishResponse!: (response: Response) => void;
      let dispatched!: () => void;
      const httpStarted = new Promise<void>((resolve) => {
        dispatched = resolve;
      });
      const fetchSpy = vi.fn<typeof fetch>();
      fetchSpy.mockImplementationOnce(() => {
        dispatched();
        return new Promise<Response>((resolve) => {
          finishResponse = resolve;
        });
      });
      vi.stubGlobal("fetch", fetchSpy);
      const session = makeSession({ id: `revoked-image-${phase}` });
      const view = render(<TerminalView session={session} readOnly={false} />);
      await waitFor(() => expect(FakeWS.sockets).toHaveLength(1));
      const socket = FakeWS.sockets[0]!;
      socket.open();
      socket.deliver({ type: "size_owner", is_owner: true });
      const file = new File(["hi"], "img.png", { type: "image/png" });
      const imageClipboard = (text: string) => ({
        clipboardData: {
          getData: () => text,
          items: [{ kind: "file", type: file.type, getAsFile: () => file }],
        },
      });
      fireEvent.paste(screen.getByLabelText("Live terminal input"), imageClipboard("revoked caption"));
      await decodingStarted;
      const pendingUpload = pasteImage.mock.results.at(-1)!.value as Promise<string | null>;
      if (phase === "response") {
        await act(async () => {
          finishDecode();
          await httpStarted;
        });
      }
      view.rerender(<TerminalView session={session} readOnly />);
      view.rerender(<TerminalView session={session} readOnly={false} />);
      await act(async () => {
        if (phase === "decoding") finishDecode();
        else finishResponse(new Response(JSON.stringify({ path: "/wt/accepted.png" }), { status: 200 }));
        await pendingUpload;
      });
      expect(await pendingUpload).toBe(phase === "decoding" ? null : "/wt/accepted.png");
      expect(fetchSpy).toHaveBeenCalledTimes(phase === "decoding" ? 0 : 1);
      expect(socket.controls().filter(({ type }) => type === "paste")).toEqual([]);
      expect(FakeWS.sockets).toHaveLength(1);

      // A new gesture has new admission; an accepted old upload never grants a later paste.
      fetchSpy.mockReset().mockResolvedValue(new Response(JSON.stringify({ path: "/wt/fresh.png" }), { status: 200 }));
      fireEvent.paste(screen.getByLabelText("Live terminal input"), imageClipboard("fresh caption"));
      await act(async () => {
        finishDecode();
      });
      await waitFor(() =>
        expect(socket.controls().filter(({ type }) => type === "paste")).toEqual([
          { type: "paste", text: " fresh caption /wt/fresh.png ", submit: false },
        ]),
      );
      expect(fetchSpy).toHaveBeenCalledTimes(1);
    },
  );

  it.each(["revoked clipboard", ""])(
    "drops delayed clipboard result %j across a read-only round trip",
    async (oldText) => {
      Object.defineProperty(window, "isSecureContext", { value: true, configurable: true });
      let finish!: (text: string) => void;
      let started!: () => void;
      const clipboardStarted = new Promise<void>((resolve) => {
        started = resolve;
      });
      const readText = vi.fn<() => Promise<string>>().mockImplementationOnce(() => {
        started();
        return new Promise<string>((resolve) => {
          finish = resolve;
        });
      });
      Object.defineProperty(navigator, "clipboard", { value: { readText }, configurable: true });
      const session = makeSession({ id: `revoked-clipboard-${oldText.length}` });
      const view = render(<TerminalView session={session} readOnly={false} />);
      await waitFor(() => expect(FakeWS.sockets).toHaveLength(1));
      const socket = FakeWS.sockets[0]!;
      socket.open();
      socket.deliver({ type: "size_owner", is_owner: true });
      fireEvent.click(screen.getByRole("button", { name: "Paste from clipboard" }));
      await clipboardStarted;
      view.rerender(<TerminalView session={session} readOnly />);
      view.rerender(<TerminalView session={session} readOnly={false} />);
      await act(async () => {
        finish(oldText);
      });
      expect(socket.controls().filter(({ type }) => type === "paste")).toEqual([]);
      expect(screen.queryByRole("dialog", { name: "Compose" })).toBeNull();
      expect(FakeWS.sockets).toHaveLength(1);
      readText.mockResolvedValueOnce("fresh clipboard");
      fireEvent.click(screen.getByRole("button", { name: "Paste from clipboard" }));
      await waitFor(() =>
        expect(socket.controls().filter(({ type }) => type === "paste")).toEqual([
          { type: "paste", text: "fresh clipboard", submit: false },
        ]),
      );
    },
  );

  it("adopts a dispatched ensure result across revocation without ensuring again", async () => {
    let finish!: (result: { ok: boolean; message?: string | null }) => void;
    ensureSession.mockReturnValueOnce(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    const session = makeSession({ id: "accepted-ensure" });
    const view = render(<TerminalView session={session} readOnly={false} />);
    await waitFor(() => expect(ensureSession).toHaveBeenCalledTimes(1));
    expect(FakeWS.sockets).toHaveLength(0);
    view.rerender(<TerminalView session={session} readOnly />);
    await waitFor(() => expect(FakeWS.sockets).toHaveLength(1));
    const observer = FakeWS.sockets[0]!;
    observer.open();
    observer.deliver(frame("observe while ensure settles"));
    await act(async () => {
      finish({ ok: true, message: "Accepted restart" });
    });
    expect(await screen.findByText("Accepted restart")).toBeTruthy();
    expect(mutationControls(observer)).toEqual([]);
    view.rerender(<TerminalView session={session} readOnly={false} />);
    expect(ensureSession).toHaveBeenCalledTimes(1);
    expect(FakeWS.sockets).toHaveLength(1);
  });

  it("does not turn a read-only Start status edge into a restart", async () => {
    const stopped = makeSession({ id: "readonly-start", status: "Stopped" });
    const view = render(<TerminalView session={stopped} readOnly />);
    await waitFor(() => expect(FakeWS.sockets).toHaveLength(1));
    const socket = FakeWS.sockets[0]!;
    act(() => {
      socket.readyState = FakeWS.CLOSED;
      socket.onclose?.(new CloseEvent("close", { code: 4001 }));
    });
    view.rerender(<TerminalView session={{ ...stopped, status: "Running" }} readOnly />);
    expect(await screen.findByText("Agent exited")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Restart" })).toBeNull();
    expect(ensureSession).not.toHaveBeenCalled();
    expect(FakeWS.sockets).toHaveLength(1);
  });
});

// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { AboutModal } from "../AboutModal";
import { toastBus } from "../../lib/toastBus";

vi.mock("../../lib/api", () => ({
  fetchAbout: vi.fn().mockResolvedValue(null),
}));

function setSecureContext(secure: boolean) {
  Object.defineProperty(window, "isSecureContext", { value: secure, configurable: true });
}

function stubClipboard() {
  setSecureContext(true);
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
  return writeText;
}

function stubToasts() {
  const info = vi.fn();
  const error = vi.fn();
  toastBus.handler = { push: vi.fn(), info, error };
  return { info, error };
}

// writeClipboard awaits a promise before toasting; flush microtasks so the
// toast assertion sees the settled result.
async function flushMicrotasks() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

afterEach(() => {
  vi.restoreAllMocks();
  toastBus.handler = null;
});

describe("AboutModal session id", () => {
  it("copies the session id and reports success", async () => {
    const writeText = stubClipboard();
    const { info } = stubToasts();
    render(<AboutModal onClose={() => {}} sessionId="my-project-20250622" />);

    fireEvent.click(screen.getByRole("button", { name: /copy session id/i }));
    await flushMicrotasks();

    expect(writeText).toHaveBeenCalledWith("my-project-20250622");
    expect(info).toHaveBeenCalledWith("Copied session id");
  });

  it("falls back to execCommand without navigator.clipboard and reports its result", async () => {
    setSecureContext(false);
    Object.defineProperty(navigator, "clipboard", { value: undefined, configurable: true });
    for (const ok of [true, false]) {
      const exec = vi.fn().mockReturnValue(ok);
      Object.defineProperty(document, "execCommand", { value: exec, configurable: true });
      const { info, error } = stubToasts();
      const { unmount } = render(<AboutModal onClose={() => {}} sessionId="sess-fallback" />);

      fireEvent.click(screen.getByRole("button", { name: /copy session id/i }));
      await flushMicrotasks();

      expect(exec).toHaveBeenCalledWith("copy");
      if (ok) expect(info).toHaveBeenCalledWith("Copied session id");
      else {
        expect(error).toHaveBeenCalledWith("Copy failed");
        expect(info).not.toHaveBeenCalled();
      }
      unmount();
    }
  });
});

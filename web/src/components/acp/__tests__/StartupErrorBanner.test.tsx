// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { StartupErrorBanner } from "../StartupErrorBanner";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const NATIVE_BINARY_MSG =
  'agent spawn failed: ACP connection failed: Internal error: { "details": "Claude Code native binary at /usr/lib/node_modules/.../claude exists but failed to launch." }';

function stubLog(body: { exists: boolean; tail: string; truncated?: boolean }) {
  const fetchSpy = vi.fn().mockResolvedValue({ ok: true, status: 200, json: async () => body });
  vi.stubGlobal("fetch", fetchSpy);
  return fetchSpy;
}

function renderBanner(message = NATIVE_BINARY_MSG, sessionId = "s-1") {
  return render(<StartupErrorBanner sessionId={sessionId} message={message} />);
}

describe("StartupErrorBanner remediation", () => {
  it.each([
    ["native binary", NATIVE_BINARY_MSG, ["Architecture mismatch", "dynamic loader", "bind-mounted into a container"]],
    // A respawn-budget park embeds the ProjectPathMissing text when the cwd moved.
    [
      "moved project path",
      "Structured view worker failed to stay up after 5 restart attempts in 60s; auto-respawn paused. project path no longer exists: /Users/me/aoe/worktrees/Burmese",
      ["working directory no longer exists", "/Users/me/aoe/worktrees/Burmese"],
    ],
  ])("routes %s to its own copy, not doctor --fix", (_label, message, copy) => {
    stubLog({ exists: false, tail: "" });
    const { container } = renderBanner(message);
    for (const text of copy) expect(container.textContent).toContain(text);
    expect(container.textContent).not.toContain("aoe acp doctor --fix");
  });

  it("links the native-binary docs anchor", () => {
    stubLog({ exists: false, tail: "" });
    const anchor = renderBanner().container.querySelector("a[href*='structured-view']");
    expect(anchor?.getAttribute("href")).toContain("native-binary-launch-failure");
  });

  it("falls back to the doctor --fix copy on a generic failure", () => {
    stubLog({ exists: false, tail: "" });
    expect(renderBanner("some unknown failure").container.textContent).toContain("aoe acp doctor --fix");
  });
});

describe("AgentLogDisclosure", () => {
  it("fetches the worker log only on first open and renders the tail", async () => {
    const fetchSpy = stubLog({ exists: true, tail: "ERROR acp.acp: spawn failed\nclaude execve: ENOEXEC" });
    const { getByTestId } = renderBanner(NATIVE_BINARY_MSG, "abc-123");
    expect(fetchSpy).not.toHaveBeenCalled();
    fireEvent.click(getByTestId("acp-agent-log-toggle"));
    await waitFor(() => expect(getByTestId("acp-agent-log-pre").textContent).toContain("ENOEXEC"));
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    expect(fetchSpy.mock.calls[0]?.[0]).toContain("/api/sessions/abc-123/acp/worker-log?tail=200");
  });

  it.each([
    ["No log output yet", { exists: false, tail: "" }],
    ["Log file exists but is empty", { exists: true, tail: "" }],
    ["Log is large; showing the tail", { exists: true, tail: "tail content", truncated: true }],
  ])("renders %s", async (expected, body) => {
    stubLog(body);
    const { getByTestId, container } = renderBanner();
    fireEvent.click(getByTestId("acp-agent-log-toggle"));
    await waitFor(() => expect(container.textContent).toContain(expected));
  });

  it("shows an error message when the fetch fails", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 500, text: async () => "boom" }));
    const { getByTestId, container } = renderBanner();
    fireEvent.click(getByTestId("acp-agent-log-toggle"));
    await waitFor(() => {
      expect(container.textContent).toContain("Could not load log");
      expect(container.textContent).toContain("500");
    });
  });

  it("hides the body on a second toggle and re-fetches on Refresh", async () => {
    const fetchSpy = stubLog({ exists: true, tail: "abc" });
    const { getByTestId, queryByTestId } = renderBanner();
    const toggle = getByTestId("acp-agent-log-toggle");
    fireEvent.click(toggle);
    await waitFor(() => expect(queryByTestId("acp-agent-log-pre")).not.toBeNull());
    fireEvent.click(getByTestId("acp-agent-log-refresh"));
    await waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(2));
    fireEvent.click(toggle);
    expect(queryByTestId("acp-agent-log-pre")).toBeNull();
  });
});

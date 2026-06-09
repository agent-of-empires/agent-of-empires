// @vitest-environment jsdom
//
// Switch-view action button + confirm dialog. The live spec
// covers the round-trip; this spec pins the button states (label
// flip by current view, ACP-disabled hint, offline disable),
// the confirm dialog routing, and the POST endpoint shape.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { SwitchViewAction } from "./SwitchViewAction";

let mockOffline = false;
vi.mock("../../lib/connectionState", () => ({
  useServerDown: () => mockOffline,
  OFFLINE_TITLE: "Disconnected",
}));

function mockOkFetch(): ReturnType<typeof vi.fn> {
  const fn = vi.fn().mockResolvedValue({
    ok: true,
    status: 200,
    text: async () => "",
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

function mockBadFetch(body = "boom", status = 500): ReturnType<typeof vi.fn> {
  const fn = vi.fn().mockResolvedValue({
    ok: false,
    status,
    text: async () => body,
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

beforeEach(() => {
  mockOffline = false;
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("SwitchViewAction trigger", () => {
  it("labels the icon as 'Switch to structured view' when current view is terminal", () => {
    render(<SwitchViewAction sessionId="s-1" structuredView={false} />);
    const btns = document.querySelectorAll("button[aria-label='Switch to structured view']");
    expect(btns.length).toBeGreaterThan(0);
  });

  it("labels the icon as 'Switch to terminal view' when current view is structured view", () => {
    render(<SwitchViewAction sessionId="s-1" structuredView={true} />);
    const btns = document.querySelectorAll("button[aria-label='Switch to terminal view']");
    expect(btns.length).toBeGreaterThan(0);
  });

  it("renders 'Switch to terminal' text in button variant when structuredView is true", () => {
    const { getByText } = render(<SwitchViewAction sessionId="s-1" structuredView={true} variant="button" />);
    expect(getByText("Switch to terminal view")).toBeTruthy();
  });

  it("is disabled when the target is structured view and the agent is not ACP-capable", () => {
    const { getByLabelText } = render(<SwitchViewAction sessionId="s-1" structuredView={false} acpCapable={false} />);
    const btn = getByLabelText("Switch to structured view") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(btn.title).toMatch(/no ACP adapter/i);
  });

  it("is disabled when the server is offline", () => {
    mockOffline = true;
    const { getByLabelText } = render(<SwitchViewAction sessionId="s-1" structuredView={false} />);
    const btn = getByLabelText("Switch to structured view") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(btn.title).toMatch(/Disconnected/i);
  });
});

describe("SwitchViewAction confirm dialog", () => {
  it("opens the confirm dialog on trigger click", () => {
    mockOkFetch();
    const { getByLabelText, getByRole } = render(<SwitchViewAction sessionId="s-1" structuredView={false} />);
    fireEvent.click(getByLabelText("Switch to structured view"));
    expect(getByRole("dialog")).toBeTruthy();
  });

  it("cancel closes the dialog without firing fetch", () => {
    const fetchFn = mockOkFetch();
    const { getByLabelText, getByText, queryByRole } = render(
      <SwitchViewAction sessionId="s-1" structuredView={false} />,
    );
    fireEvent.click(getByLabelText("Switch to structured view"));
    fireEvent.click(getByText("Cancel"));
    expect(queryByRole("dialog")).toBeNull();
    expect(fetchFn).not.toHaveBeenCalled();
  });

  it("escape closes the dialog", () => {
    mockOkFetch();
    const { getByLabelText, queryByRole } = render(<SwitchViewAction sessionId="s-1" structuredView={false} />);
    fireEvent.click(getByLabelText("Switch to structured view"));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(queryByRole("dialog")).toBeNull();
  });

  it("POSTs to /acp/enable when switching FROM terminal TO structured view", async () => {
    const fetchFn = mockOkFetch();
    const { getByLabelText, getByText } = render(<SwitchViewAction sessionId="s-1" structuredView={false} />);
    fireEvent.click(getByLabelText("Switch to structured view"));
    fireEvent.click(getByText("Switch"));
    await waitFor(() => expect(fetchFn).toHaveBeenCalledTimes(1));
    expect(fetchFn.mock.calls[0]?.[0]).toBe("/api/sessions/s-1/acp/enable");
    expect(fetchFn.mock.calls[0]?.[1]).toMatchObject({ method: "POST" });
  });

  it("POSTs to /acp/disable when switching FROM structured view TO terminal", async () => {
    const fetchFn = mockOkFetch();
    const { getByLabelText, getByText } = render(<SwitchViewAction sessionId="s-1" structuredView={true} />);
    fireEvent.click(getByLabelText("Switch to terminal view"));
    fireEvent.click(getByText("Switch"));
    await waitFor(() => expect(fetchFn).toHaveBeenCalledTimes(1));
    expect(fetchFn.mock.calls[0]?.[0]).toBe("/api/sessions/s-1/acp/disable");
  });

  it("URL-encodes the session id in the endpoint", async () => {
    const fetchFn = mockOkFetch();
    const { getByLabelText, getByText } = render(<SwitchViewAction sessionId="weird/id" structuredView={false} />);
    fireEvent.click(getByLabelText("Switch to structured view"));
    fireEvent.click(getByText("Switch"));
    await waitFor(() => expect(fetchFn).toHaveBeenCalled());
    expect(fetchFn.mock.calls[0]?.[0]).toBe("/api/sessions/weird%2Fid/acp/enable");
  });

  it("surfaces a server error response in the dialog", async () => {
    mockBadFetch("session not found", 404);
    const { getByLabelText, getByText, findByText, queryByRole } = render(
      <SwitchViewAction sessionId="s-1" structuredView={false} />,
    );
    fireEvent.click(getByLabelText("Switch to structured view"));
    fireEvent.click(getByText("Switch"));
    await findByText(/session not found/i);
    // Dialog stays open on error so the user can retry.
    expect(queryByRole("dialog")).not.toBeNull();
  });

  it("falls back to 'HTTP <status>' when the error body is empty", async () => {
    mockBadFetch("", 500);
    const { getByLabelText, getByText, findByText } = render(
      <SwitchViewAction sessionId="s-1" structuredView={false} />,
    );
    fireEvent.click(getByLabelText("Switch to structured view"));
    fireEvent.click(getByText("Switch"));
    await findByText(/HTTP 500/);
  });

  it("surfaces network rejection as the dialog error", async () => {
    const fn = vi.fn().mockRejectedValue(new Error("offline"));
    vi.stubGlobal("fetch", fn);
    const { getByLabelText, getByText, findByText } = render(
      <SwitchViewAction sessionId="s-1" structuredView={false} />,
    );
    fireEvent.click(getByLabelText("Switch to structured view"));
    fireEvent.click(getByText("Switch"));
    await findByText(/offline/);
  });
});

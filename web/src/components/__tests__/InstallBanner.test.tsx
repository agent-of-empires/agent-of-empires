// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";

import type { PushState } from "../../hooks/usePushSubscription";
import type { InstallHint } from "../../lib/platform";
import { INSTALL_STEPS } from "../../lib/tourSteps";
import { noteDaemonChanged, setServerDown } from "../../lib/connectionState";

const mocks = vi.hoisted(() => ({
  hint: null as InstallHint | null,
  state: { kind: "off" } as PushState,
  serverEnabled: true as boolean | "fail",
}));

vi.mock("../../lib/platform", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/platform")>()),
  installHint: () => mocks.hint,
}));
vi.mock("../../hooks/usePushSubscription", () => ({
  usePushSubscription: () => ({ state: mocks.state, health: "unknown" }),
}));

import {
  INSTALL_BANNER_DISMISSED_KEY,
  INSTALL_BANNER_HIDE_LABEL,
  INSTALL_BANNER_HIDE_TIP,
  InstallBanner,
} from "../InstallBanner";

beforeEach(() => {
  localStorage.clear();
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      if (url !== "/api/push/status") throw new Error(`unexpected ${url}`);
      if (mocks.serverEnabled === "fail") return { ok: false, json: async () => ({}) };
      return { ok: true, json: async () => ({ enabled: mocks.serverEnabled }) };
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  mocks.hint = null;
  mocks.state = { kind: "off" };
  mocks.serverEnabled = true;
});

async function mount(hidden = false) {
  const view = render(
    <MemoryRouter initialEntries={["/"]}>
      <Routes>
        <Route path="/" element={<InstallBanner hidden={hidden} />} />
        <Route path="/settings/notifications" element={<p>notifications settings</p>} />
      </Routes>
    </MemoryRouter>,
  );
  // Lets the server push status settle.
  await act(async () => {
    for (let i = 0; i < 5; i++) await Promise.resolve();
  });
  return view;
}

const region = () => screen.queryByRole("region", { name: "Get notifications on this device" });

describe("InstallBanner", () => {
  type Row = {
    name: string;
    hint: InstallHint | null;
    state?: PushState;
    server?: boolean | "fail";
    hidden?: boolean;
    dismissed?: boolean;
    shown: boolean;
  };
  it.each<Row>([
    { name: "an iPhone tab", hint: "ios", state: { kind: "unsupported", reason: "ios-not-standalone" }, shown: true },
    { name: "a tab that can push, with push off", hint: "push", shown: true },
    { name: "a tab whose push errored", hint: "push", state: { kind: "error", message: "boom" }, shown: true },
    { name: "a touch tab without push", hint: "other", state: { kind: "unsupported", reason: "no-api" }, shown: true },
    // A failed status request counts as on, so an outage doesn't hide the steps.
    { name: "an iPhone tab whose status request fails", hint: "ios", server: "fail", shown: true },
    { name: "a tab with push already on", hint: "push", state: { kind: "enabled" }, shown: false },
    { name: "a tab with permission denied", hint: "push", state: { kind: "denied" }, shown: false },
    {
      name: "a push tab with push off on the server",
      hint: "push",
      state: { kind: "disabled-by-server" },
      shown: false,
    },
    { name: "an iPhone tab with push off on the server", hint: "ios", server: false, shown: false },
    { name: "a touch tab without push, push off on the server", hint: "other", server: false, shown: false },
    { name: "a push tab still loading its state", hint: "push", state: { kind: "loading" }, shown: false },
    { name: "an installed app, desktop or insecure origin", hint: null, shown: false },
    { name: "a dismissed iPhone tab", hint: "ios", dismissed: true, shown: false },
    { name: "a dismissed push tab", hint: "push", dismissed: true, shown: false },
    { name: "an iPhone tab while hidden (tour or Disconnected)", hint: "ios", hidden: true, shown: false },
    { name: "a push tab while hidden (tour or Disconnected)", hint: "push", hidden: true, shown: false },
  ])("$name: shown $shown", async ({ hint, state, server, hidden, dismissed, shown }) => {
    mocks.hint = hint;
    if (state) mocks.state = state;
    if (server !== undefined) mocks.serverEnabled = server;
    if (dismissed) localStorage.setItem(INSTALL_BANNER_DISMISSED_KEY, "1");
    await mount(hidden);
    expect(region() !== null).toBe(shown);
  });

  it.each<{ hint: "ios" | "other"; text: string }>([
    {
      hint: "ios",
      text: "Get notifications on this device. Tap Share › Add to Home Screen. " + INSTALL_STEPS.ios.banner.text,
    },
    { hint: "other", text: "Get notifications on this device. " + INSTALL_STEPS.other.banner.text },
  ])("gives $hint its install steps", async ({ hint, text }) => {
    mocks.hint = hint;
    await mount();
    expect(region()?.textContent?.replace("×", "")).toBe(text);
    expect(region()?.querySelector("a")).toBeNull();
  });

  it("links a tab that can push to Settings › Notifications", async () => {
    mocks.hint = "push";
    await mount();
    expect(region()?.textContent?.replace("×", "")).toBe(
      "Get notifications on this device. " + INSTALL_STEPS.push.banner.text,
    );
    const link = screen.getByRole("link", { name: "Settings › Notifications" });
    expect(link.getAttribute("href")).toBe("/settings/notifications");
    fireEvent.click(link);
    expect(screen.getByText("notifications settings")).toBeTruthy();
  });

  it("hides for good on this device when × is pressed", async () => {
    mocks.hint = "ios";
    const view = await mount();
    const close = screen.getByRole("button", { name: INSTALL_BANNER_HIDE_LABEL, description: INSTALL_BANNER_HIDE_TIP });
    expect(close.getAttribute("title")).toBe(INSTALL_BANNER_HIDE_TIP);
    fireEvent.click(close);
    expect(region()).toBeNull();
    view.unmount();
    await mount();
    expect(region()).toBeNull();
  });

  it("still hides for this page when storage is blocked", async () => {
    mocks.hint = "push";
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    await mount();
    fireEvent.click(
      screen.getByRole("button", { name: INSTALL_BANNER_HIDE_LABEL, description: INSTALL_BANNER_HIDE_TIP }),
    );
    expect(region()).toBeNull();
  });

  it.each([
    { from: true, to: false, via: "reconnect", name: "hides the steps once a restarted Daemon has push off" },
    { from: false, to: true, via: "reconnect", name: "shows the steps once a restarted Daemon has push on" },
    { from: true, to: false, via: "epoch", name: "hides the steps after a restart seen only as a new Daemon epoch" },
  ])("$name", async ({ from, to, via }) => {
    mocks.hint = "ios";
    mocks.serverEnabled = from;
    await mount();
    expect(region() !== null).toBe(from);
    mocks.serverEnabled = to;
    await act(async () => {
      if (via === "reconnect") {
        setServerDown(true);
        setServerDown(false);
      } else noteDaemonChanged();
      for (let i = 0; i < 5; i++) await Promise.resolve();
    });
    expect(region() !== null).toBe(to);
  });

  it("asks the server about push only for a tab that can't subscribe", async () => {
    mocks.hint = "push";
    await mount();
    expect(fetch).not.toHaveBeenCalled();
  });
});

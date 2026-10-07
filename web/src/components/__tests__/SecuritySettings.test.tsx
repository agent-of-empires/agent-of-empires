// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { render, waitFor } from "@testing-library/react";

import type { ServerAbout } from "../../lib/api";

const fetchAbout = vi.fn();
vi.mock("../../lib/api", () => ({
  fetchAbout: () => fetchAbout(),
}));

import { SecuritySettings } from "../SecuritySettings";

function makeAbout(overrides: Partial<ServerAbout> = {}): ServerAbout {
  return {
    version: "1.2.3",
    auth_required: true,
    passphrase_enabled: false,
    auth_mode: "token",
    read_only: false,
    behind_tunnel: false,
    profile: "default",
    ...overrides,
  } as ServerAbout;
}

function renderWith(about: ServerAbout | null) {
  fetchAbout.mockResolvedValue(about);
  return render(<SecuritySettings />).container;
}

afterEach(() => {
  fetchAbout.mockReset();
});

describe("SecuritySettings", () => {
  it.each([
    ["token auth badge", { auth_mode: "token" }, "--auth=token"],
    ["passphrase auth badge", { auth_mode: "passphrase" }, "--auth=passphrase"],
    ["no-auth warning badge", { auth_mode: "none" }, "--auth=none"],
    ["passphrase 'required' badge", { passphrase_enabled: true }, "required"],
    ["passphrase 'not set' badge", { passphrase_enabled: false }, "not set"],
    ["read-only badge", { read_only: true }, "terminal input blocked"],
    ["cloudflared badge", { behind_tunnel: true }, "cloudflared"],
    ["version with a leading 'v'", { version: "9.9.9" }, "v9.9.9"],
  ] as [string, Partial<ServerAbout>, string][])("shows the %s", async (_name, about, text) => {
    const container = renderWith(makeAbout(about));
    await waitFor(() => {
      expect(container.textContent).toContain(text);
    });
  });

  it("shows 'off' for read_only=false", async () => {
    const container = renderWith(makeAbout({ read_only: false }));
    await waitFor(() => {
      // The Read-only Row renders the literal text 'off'.
      const cells = container.querySelectorAll("span");
      const offCell = Array.from(cells).find((c) => c.textContent?.trim().toLowerCase() === "off");
      expect(offCell).toBeDefined();
    });
  });

  it("renders the load-error message when fetchAbout returns null", async () => {
    const container = renderWith(null);
    await waitFor(() => {
      expect(container.textContent).toContain("Could not load server status");
    });
  });
});

// @vitest-environment jsdom
//
// Contract test for the SecuritySettings panel. SecuritySettings is
// purely a read-only view over the /api/about response; this suite
// mocks fetchAbout and asserts the rendered badges match each
// permutation of auth_mode, passphrase_enabled, read_only, behind_tunnel,
// and version. Part of #1217.

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

afterEach(() => {
  fetchAbout.mockReset();
});

describe("SecuritySettings", () => {
  it("shows the token auth badge when auth_mode='token'", async () => {
    fetchAbout.mockResolvedValue(makeAbout({ auth_mode: "token" }));
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      expect(container.textContent).toContain("--auth=token");
    });
  });

  it("shows the passphrase auth badge when auth_mode='passphrase'", async () => {
    fetchAbout.mockResolvedValue(makeAbout({ auth_mode: "passphrase" }));
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      expect(container.textContent).toContain("--auth=passphrase");
    });
  });

  it("shows the no-auth warning badge when auth_mode='none'", async () => {
    fetchAbout.mockResolvedValue(makeAbout({ auth_mode: "none" }));
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      expect(container.textContent).toContain("--auth=none");
    });
  });

  it("shows passphrase 'required' badge when passphrase_enabled=true", async () => {
    fetchAbout.mockResolvedValue(makeAbout({ passphrase_enabled: true }));
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      expect(container.textContent).toContain("required");
    });
  });

  it("shows passphrase 'not set' badge when passphrase_enabled=false", async () => {
    fetchAbout.mockResolvedValue(makeAbout({ passphrase_enabled: false }));
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      expect(container.textContent).toContain("not set");
    });
  });

  it("shows the read-only badge when read_only=true", async () => {
    fetchAbout.mockResolvedValue(makeAbout({ read_only: true }));
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      expect(container.textContent).toContain("terminal input blocked");
    });
  });

  it("shows 'off' for read_only=false", async () => {
    fetchAbout.mockResolvedValue(makeAbout({ read_only: false }));
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      // The Read-only Row renders the literal text 'off'.
      const cells = container.querySelectorAll("span");
      const offCell = Array.from(cells).find((c) => c.textContent?.trim().toLowerCase() === "off");
      expect(offCell).toBeDefined();
    });
  });

  it("shows the cloudflared badge when behind_tunnel=true", async () => {
    fetchAbout.mockResolvedValue(makeAbout({ behind_tunnel: true }));
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      expect(container.textContent).toContain("cloudflared");
    });
  });

  it("renders the version with a leading 'v'", async () => {
    fetchAbout.mockResolvedValue(makeAbout({ version: "9.9.9" }));
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      expect(container.textContent).toContain("v9.9.9");
    });
  });

  it("renders the load-error message when fetchAbout returns null", async () => {
    fetchAbout.mockResolvedValue(null);
    const { container } = render(<SecuritySettings />);
    await waitFor(() => {
      expect(container.textContent).toContain("Could not load server status");
    });
  });
});

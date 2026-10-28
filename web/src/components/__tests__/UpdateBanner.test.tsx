// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { UpdateBanner } from "../UpdateBanner";
import type { InstallMethod } from "../../lib/api";
import { useUpdateBanner } from "../../hooks/useUpdateBanner";
import type { UpdateStatus } from "../../lib/api";
import { setServerDown } from "../../lib/connectionState";

const fetchUpdateStatus = vi.fn();
const dismissUpdate = vi.fn();

const writeClipboard = vi.fn();
vi.mock("../../lib/clipboard", () => ({
  writeClipboard: (...args: unknown[]) => writeClipboard(...args),
}));

vi.mock("../../lib/api", () => ({
  fetchUpdateStatus: (...args: unknown[]) => fetchUpdateStatus(...args),
  dismissUpdate: (...args: unknown[]) => dismissUpdate(...args),
}));

function makeStatus(overrides?: Partial<UpdateStatus>): UpdateStatus {
  return {
    update_check_mode: "notify",
    current_version: "1.0.0",
    latest_version: "1.1.0",
    update_available: true,
    release_url: "https://example.com/releases/1.1.0",
    error: null,
    dismissed_version: null,
    ...overrides,
  };
}

beforeEach(() => {
  fetchUpdateStatus.mockReset();
  dismissUpdate.mockReset();
  dismissUpdate.mockResolvedValue(true);
});

afterEach(() => {
  cleanup();
});

function Banner() {
  const update = useUpdateBanner();
  return update && <UpdateBanner status={update.status} onDismiss={update.dismiss} />;
}

describe("UpdateBanner", () => {
  it("renders the banner with both versions and the release link only when a URL is set", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus());
    render(<Banner />);

    const banner = await screen.findByRole("status", { name: /^Update available/ });
    expect(banner.getAttribute("aria-label")).toBe("Update available: v1.1.0");
    expect(banner.textContent).toContain("v1.0.0");
    expect(banner.textContent).toContain("v1.1.0");
    const link = screen.getByText("Release notes") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://example.com/releases/1.1.0");
    cleanup();

    fetchUpdateStatus.mockResolvedValue(makeStatus({ release_url: null }));
    render(<Banner />);
    await screen.findByRole("status", { name: /^Update available/ });
    expect(screen.queryByText("Release notes")).toBeNull();
  });

  it("says how to update for each install method, with a copy target per command", () => {
    const restart = "aoe serve --restart";
    const cargo = "cargo install --git https://github.com/agent-of-empires/agent-of-empires aoe";
    const nix = "nix run github:agent-of-empires/agent-of-empires";
    // (install_method, update_command, text that must show, second line, commands with a copy button)
    const cases: [InstallMethod | undefined, string | undefined, string, string | null, string[]][] = [
      [
        "homebrew",
        "aoe update",
        "",
        "then run aoe serve --restart or restart it through your service manager",
        ["aoe update", restart],
      ],
      ["release", "aoe update", "", "It offers to restart the Daemon for you when it's done.", ["aoe update"]],
      ["cargo", cargo, "", "then run aoe serve --restart or restart it through your service manager", [cargo, restart]],
      [
        "nix",
        nix,
        "or rebuild your flake input",
        "then run aoe serve --restart or restart it through your service manager",
        [nix, restart],
      ],
      ["unknown", "aoe update", "to see how to update this install.", null, ["aoe update"]],
      // A Daemon older than the field.
      [undefined, undefined, "to see how to update this install.", null, ["aoe update"]],
    ];
    for (const [install_method, update_command, extra, second, commands] of cases) {
      const { container, unmount } = render(
        <UpdateBanner status={makeStatus({ install_method, update_command })} onDismiss={() => {}} />,
      );
      // Each line's pieces are flex items, so join them the way they read.
      const lines = Array.from(container.querySelectorAll("[aria-label^='Update available'] > div > div")).map((line) =>
        Array.from(line.children, (piece) => piece.textContent).join(" "),
      );
      const label = String(install_method);
      expect(lines[0], label).toContain("Update available: v1.0.0 → v1.1.0 On the machine running AoE, run");
      expect(lines[0], label).toContain(`run ${commands[0]}${extra ? ` ${extra}` : ""}`);
      expect(lines[1] ?? null, label).toBe(second);
      const copies = Array.from(container.querySelectorAll("button[aria-label^='Copy ']")).map((b) =>
        b.getAttribute("aria-label"),
      );
      expect(copies, label).toEqual(commands.map((c) => `Copy ${c}`));
      unmount();
    }
  });

  it("confirms a copy for two seconds, and selects the command when copying fails", async () => {
    vi.useFakeTimers();
    try {
      render(<UpdateBanner status={makeStatus({ install_method: "homebrew" })} onDismiss={() => {}} />);
      const button = screen.getByRole("button", { name: "Copy aoe update" });

      writeClipboard.mockResolvedValueOnce(true);
      await act(async () => fireEvent.click(button));
      expect(writeClipboard).toHaveBeenLastCalledWith("aoe update");
      const feedback = button.parentElement!.querySelector("[role='status']")!;
      expect(feedback.textContent).toBe("Copied");
      // The feedback is small text on surface-900, where resolved.rs holds text-primary at AA;
      // status-running is 2.96:1 there in Latte. The state colour stays on the icon.
      const textToken = () => feedback.className.match(/(?:^|\s)(text-[a-z-]+)/g)?.map((c) => c.trim());
      expect(textToken()).toEqual(["text-text-primary"]);
      await act(async () => vi.advanceTimersByTime(2000));
      expect(screen.queryByText("Copied")).toBeNull();

      writeClipboard.mockResolvedValueOnce(false);
      await act(async () => fireEvent.click(button));
      expect(feedback.textContent).toBe("Couldn't copy. Select and copy it.");
      expect(textToken()).toEqual(["text-text-primary"]);
      expect(window.getSelection()?.toString()).toBe("aoe update");
    } finally {
      vi.useRealTimers();
    }
  });

  it("renders nothing before the first poll resolves", () => {
    fetchUpdateStatus.mockReturnValue(new Promise(() => {}));
    const { container } = render(<Banner />);
    expect(container.firstChild).toBeNull();
  });

  // "off" mode has no separate client path: the server reports update_available: false, so only "auto" is
  // special-cased here.
  it.each([
    ["no update is available", { update_available: false }],
    ["auto mode handles the install", { update_check_mode: "auto" }],
    ["the version was already dismissed server-side", { dismissed_version: "1.1.0" }],
  ] as [string, Partial<UpdateStatus>][])("renders nothing when %s", async (_name, overrides) => {
    fetchUpdateStatus.mockResolvedValue(makeStatus(overrides));
    const { container } = render(<Banner />);
    await waitFor(() => expect(fetchUpdateStatus).toHaveBeenCalled());
    expect(container.querySelector('[aria-label^="Update available"]')).toBeNull();
  });

  it("overlapping polls leave a single hourly re-poll", async () => {
    vi.useFakeTimers();
    try {
      fetchUpdateStatus.mockResolvedValue(makeStatus());
      render(<Banner />);
      await act(() => vi.advanceTimersByTimeAsync(0));
      // A reconnect and a visibility flip both start a poll before either resolves.
      act(() => {
        setServerDown(true);
        setServerDown(false);
        document.dispatchEvent(new Event("visibilitychange"));
      });
      await act(() => vi.advanceTimersByTimeAsync(0));
      const before = fetchUpdateStatus.mock.calls.length;
      await act(() => vi.advanceTimersByTimeAsync(60 * 60_000));
      expect(fetchUpdateStatus.mock.calls.length - before).toBe(1);
    } finally {
      vi.useRealTimers();
      setServerDown(false);
    }
  });

  it("a slower older poll can't restore an offer a newer poll withdrew", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus());
    render(<Banner />);
    await screen.findByRole("status", { name: /^Update available/ });

    const replies: ((s: UpdateStatus) => void)[] = [];
    fetchUpdateStatus.mockImplementation(() => new Promise((resolve) => replies.push(resolve)));
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
      setServerDown(true);
      setServerDown(false);
    });
    expect(replies).toHaveLength(2);
    await act(async () => replies[1]!(makeStatus({ update_available: false })));
    await act(async () => replies[0]!(makeStatus()));
    expect(screen.queryByRole("status", { name: /^Update available/ })).toBeNull();
    setServerDown(false);
  });

  it("dismiss hides the banner optimistically and persists via dismissUpdate", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus());
    const { container } = render(<Banner />);

    await screen.findByRole("status", { name: /^Update available/ });
    const dismiss = screen.getByLabelText("Dismiss update notice");
    expect(dismiss.className).toContain("text-text-subtle");
    expect(dismiss.className).not.toMatch(/text-text-(muted|secondary)/);
    fireEvent.click(dismiss);

    expect(dismissUpdate).toHaveBeenCalledTimes(1);
    expect(dismissUpdate).toHaveBeenCalledWith("1.1.0");
    expect(container.querySelector('[aria-label^="Update available"]')).toBeNull();
  });
});

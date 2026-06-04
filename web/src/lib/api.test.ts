// Contract test for the `fetchAbout` wrapper and the `ServerAbout`
// interface it shapes. Both halves of the new `build_flavor` discriminator
// (`"debug"` | `"release"`) are exercised so the runtime branch the topbar
// reads (`serverAbout?.build_flavor === "debug"`) is locked in here at the
// API-client layer instead of only in App.tsx. Part of #1055.
//
// `fetchAbout` is otherwise only exercised by Playwright (the App boots and
// reads `/api/about` at startup); when Playwright is gated off (Vitest-only
// CI lanes, local dev) this test keeps the api-client surface for the badge
// covered.
//
// The bigger picture: every `ServerAbout` discriminator (`auth_mode`,
// `acp_queue_drain_mode`, `build_flavor`) needs to ride through the
// real `fetchAbout` -> `fetchJson` path so source maps register the
// interface body as hit, not just the function call.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  fetchAbout,
  isDebugBuild,
  markWebTourSeen,
  setSessionArchive,
  setSessionPin,
  setSessionSnooze,
  updateProfileSettings,
  PROFILE_WRITABLE_SECTIONS,
  updateSessionGroup,
  type ServerAbout,
} from "./api";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function makeAbout(overrides: Partial<ServerAbout> = {}): ServerAbout {
  return {
    version: "1.2.3",
    auth_required: false,
    passphrase_enabled: false,
    auth_mode: "none",
    read_only: false,
    behind_tunnel: false,
    profile: "default",
    acp_show_tool_durations: true,
    acp_queue_drain_mode: "combined",
    acp_max_concurrent_resumes: 4,
    acp_force_end_turn_threshold_secs: 30,
    acp_replay_events: 0,
    build_flavor: "release",
    ...overrides,
  };
}

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("fetchAbout", () => {
  it("returns the parsed ServerAbout payload on 200", async () => {
    const payload = makeAbout({ build_flavor: "debug" });
    fetchSpy.mockResolvedValueOnce(jsonResponse(payload));

    const about = await fetchAbout();
    expect(about).not.toBeNull();
    // Drive the same `build_flavor === "debug"` discriminator the topbar
    // uses (App.tsx -> `isDevBuild={serverAbout?.build_flavor === "debug"}`)
    // so the interface field is exercised through both branches.
    expect(about?.build_flavor).toBe("debug");
    expect(about?.build_flavor === "debug").toBe(true);
  });

  it("surfaces the release flavor unchanged", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse(makeAbout()));

    const about = await fetchAbout();
    expect(about?.build_flavor).toBe("release");
    expect(about?.build_flavor === "debug").toBe(false);
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchAbout()).toBeNull();
  });

  it("returns null on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await fetchAbout()).toBeNull();
  });

  it("hits the `/api/about` endpoint", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse(makeAbout()));
    await fetchAbout();
    expect(fetchSpy).toHaveBeenCalledWith("/api/about", undefined);
  });
});

describe("setSessionPin", () => {
  it("PATCHes /api/sessions/{id}/pin with the pinned bool", async () => {
    fetchSpy.mockResolvedValueOnce(
      jsonResponse({ id: "sess-1", pinned_at: "2026-01-01T00:00:00Z" }),
    );
    await setSessionPin("sess-1", true);
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-1/pin");
    expect(init?.method).toBe("PATCH");
    expect(JSON.parse(init!.body as string)).toEqual({ pinned: true });
  });

  it("forwards `pinned: false` for the unpin path", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "sess-1" }));
    await setSessionPin("sess-1", false);
    expect(JSON.parse(fetchSpy.mock.calls[0]![1]!.body as string)).toEqual({
      pinned: false,
    });
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await setSessionPin("sess-1", true)).toBeNull();
  });

  it("returns null on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await setSessionPin("sess-1", true)).toBeNull();
  });
});

describe("setSessionArchive", () => {
  it("defaults kill_pane to true (TUI/CLI parity)", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "sess-1" }));
    await setSessionArchive("sess-1", true);
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-1/archive");
    expect(JSON.parse(init!.body as string)).toEqual({
      archived: true,
      kill_pane: true,
    });
  });

  it("forwards an explicit kill_pane=false", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "sess-1" }));
    await setSessionArchive("sess-1", true, false);
    expect(JSON.parse(fetchSpy.mock.calls[0]![1]!.body as string)).toEqual({
      archived: true,
      kill_pane: false,
    });
  });

  it("PATCHes archived=false to unarchive", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "sess-1" }));
    await setSessionArchive("sess-1", false);
    expect(JSON.parse(fetchSpy.mock.calls[0]![1]!.body as string)).toEqual({
      archived: false,
      kill_pane: true,
    });
  });
});

describe("setSessionSnooze", () => {
  it("PATCHes minutes as a positive integer", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "sess-1" }));
    await setSessionSnooze("sess-1", 60);
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-1/snooze");
    expect(JSON.parse(init!.body as string)).toEqual({ minutes: 60 });
  });

  it("PATCHes minutes=null to unsnooze", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "sess-1" }));
    await setSessionSnooze("sess-1", null);
    expect(JSON.parse(fetchSpy.mock.calls[0]![1]!.body as string)).toEqual({
      minutes: null,
    });
  });

  it("returns null on 400 (server rejected an out-of-range duration)", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 400 }));
    expect(await setSessionSnooze("sess-1", 0)).toBeNull();
  });
});

describe("updateProfileSettings write guard", () => {
  // PROFILE_WRITABLE_SECTIONS mirrors the server's
  // ALLOWED_PROFILE_SETTINGS_SECTIONS (src/server/api/mod.rs). This literal
  // pin fails CI if the client list drifts; keep it in sync with the Rust
  // constant and its pinned regression test by hand.
  it("pins the allowlist to the server's writable sections", () => {
    expect([...PROFILE_WRITABLE_SECTIONS]).toEqual([
      "theme",
      "session",
      "tmux",
      "updates",
      "sound",
      "sandbox",
      "worktree",
      "web",
      "logging",
      "acp",
      "description",
    ]);
  });

  it("refuses to send a body containing the blocked `hooks` section", async () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const ok = await updateProfileSettings("work", {
      hooks: { on_create: ["rm -rf /"] },
    });
    expect(ok).toBe(false);
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(errSpy).toHaveBeenCalled();
    errSpy.mockRestore();
  });

  it("refuses any unknown/blocked key even alongside an allowed one", async () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const ok = await updateProfileSettings("work", {
      theme: { name: "empire" },
      custom_agents: { evil: "ssh host claude" },
    });
    expect(ok).toBe(false);
    expect(fetchSpy).not.toHaveBeenCalled();
    errSpy.mockRestore();
  });

  it("PATCHes an allowed section through to the server", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await updateProfileSettings("work", {
      description: "my profile",
    });
    expect(ok).toBe(true);
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/profiles/work/settings");
    expect(init?.method).toBe("PATCH");
    expect(JSON.parse(init!.body as string)).toEqual({
      description: "my profile",
    });
  });
});

describe("updateSessionGroup", () => {
  it("PATCHes /api/sessions/{id}/group with the group path", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "sess-1" }));
    await updateSessionGroup("sess-1", "team/alpha");
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-1/group");
    expect(init?.method).toBe("PATCH");
    expect(JSON.parse(init!.body as string)).toEqual({ group: "team/alpha" });
  });

  it("sends an empty string to ungroup (no null on the wire)", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "sess-1" }));
    await updateSessionGroup("sess-1", "");
    expect(JSON.parse(fetchSpy.mock.calls[0]![1]!.body as string)).toEqual({
      group: "",
    });
  });

  it("encodes the session id in the path", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "a/b" }));
    await updateSessionGroup("a/b", "g");
    expect(fetchSpy.mock.calls[0]![0]).toBe("/api/sessions/a%2Fb/group");
  });

  it("returns false on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 403 }));
    expect(await updateSessionGroup("sess-1", "g")).toBe(false);
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await updateSessionGroup("sess-1", "g")).toBe(false);
  });
});

describe("markWebTourSeen", () => {
  it("POSTs /api/app-state/web-tour-seen with no body", async () => {
    fetchSpy.mockResolvedValueOnce(
      jsonResponse({ has_seen_web_tour: true }),
    );
    const ok = await markWebTourSeen();
    expect(ok).toBe(true);
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/app-state/web-tour-seen");
    expect(init?.method).toBe("POST");
    expect(init?.body).toBeUndefined();
  });

  it("returns false on a read-only 403 (nonfatal)", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 403 }));
    expect(await markWebTourSeen()).toBe(false);
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await markWebTourSeen()).toBe(false);
  });
});

describe("isDebugBuild", () => {
  it("returns true for a debug-flavored payload", () => {
    expect(isDebugBuild(makeAbout({ build_flavor: "debug" }))).toBe(true);
  });

  it("returns false for a release-flavored payload", () => {
    expect(isDebugBuild(makeAbout({ build_flavor: "release" }))).toBe(false);
  });

  it("returns false when the about payload is null", () => {
    expect(isDebugBuild(null)).toBe(false);
  });

  it("returns false when the about payload is undefined", () => {
    expect(isDebugBuild(undefined)).toBe(false);
  });
});

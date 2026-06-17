// @vitest-environment jsdom
//
// Coverage-lifting contract tests for the many api.ts helpers that the
// existing api.test.ts / api.acp.test.ts suites do not touch. Each helper is
// exercised on both its success path (200 / expected JSON) and its failure
// path (non-2xx -> null/false/error, or a thrown network error caught), and
// the request URL, method, and JSON body are asserted where the helper builds
// a payload. jsdom is used so the device-binding (login/elevateLogin) and the
// dynamic import in logout resolve.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  fetchSessions,
  updateWorkspaceOrdering,
  ensureSession,
  ensureTerminal,
  getSessionDiffFiles,
  getSessionFileContents,
  fetchSettings,
  getWebUiState,
  patchWebUiState,
  fetchVolumeIgnoresPreview,
  markVolumeIgnoresGlobsAcknowledged,
  createProfile,
  deleteProfile,
  renameProfile,
  setDefaultProfile,
  getProfileSettings,
  fetchThemes,
  fetchResolvedTheme,
  fetchCurrentTheme,
  fetchSounds,
  fetchSoundBlob,
  fetchTelemetryStatus,
  setTelemetryConsent,
  reportTelemetrySeen,
  reportAcpInteraction,
  fetchUpdateStatus,
  dismissUpdate,
  fetchBranches,
  fetchContextPrimer,
  fetchDevices,
  revokeDevice,
  signOutAllDevices,
  fetchAgents,
  fetchProfiles,
  getHomePath,
  browseFilesystem,
  fetchGroups,
  fetchProjects,
  createProject,
  deleteProject,
  updateProject,
  setProjectPinned,
  fetchDockerStatus,
  createSession,
  cloneRepo,
  loginStatus,
  verifyToken,
  login,
  elevateLogin,
  logout,
  renameSession,
  setWorktreeName,
  setSessionNotifications,
  setSessionDiffBase,
  stopSession,
  startSession,
  deleteSession,
  fetchMcpServers,
  resolveMcpConflict,
  keepMcpServer,
  dropMcpServer,
} from "./api";
import type { CreateSessionRequest } from "./types";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function lastCall() {
  return fetchSpy.mock.calls[fetchSpy.mock.calls.length - 1]!;
}

function bodyOf(init: RequestInit | undefined): unknown {
  return JSON.parse(init!.body as string);
}

// Sessions

describe("fetchSessions", () => {
  it("GETs /api/sessions and returns the envelope", async () => {
    const env = { sessions: [{ id: "s1" }], workspace_ordering: ["s1"] };
    fetchSpy.mockResolvedValueOnce(jsonResponse(env));
    const result = await fetchSessions();
    expect(result).toEqual(env);
    expect(lastCall()[0]).toBe("/api/sessions");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchSessions()).toBeNull();
  });

  it("returns null on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await fetchSessions()).toBeNull();
  });
});

describe("updateWorkspaceOrdering", () => {
  it("PUTs the order array to /api/workspace-ordering", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await updateWorkspaceOrdering(["a", "b"]);
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/workspace-ordering");
    expect(init?.method).toBe("PUT");
    expect(bodyOf(init)).toEqual({ order: ["a", "b"] });
  });

  it("returns false on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 403 }));
    expect(await updateWorkspaceOrdering([])).toBe(false);
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await updateWorkspaceOrdering([])).toBe(false);
  });
});

describe("ensureSession", () => {
  it("POSTs /api/sessions/:id/ensure and returns ok + status on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ status: "restarted" }));
    const result = await ensureSession("s1");
    expect(result).toEqual({ ok: true, status: "restarted" });
    const [url, init] = lastCall();
    expect(url).toBe("/api/sessions/s1/ensure");
    expect(init?.method).toBe("POST");
  });

  it("surfaces server error/message on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ error: "boom", message: "no good" }, 500));
    const result = await ensureSession("s1");
    expect(result.ok).toBe(false);
    expect(result.error).toBe("boom");
    expect(result.message).toBe("no good");
  });

  it("falls back to a status-coded message when body has none", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 502 }));
    const result = await ensureSession("s1");
    expect(result.ok).toBe(false);
    expect(result.message).toBe("Server error (502)");
  });

  it("returns aborted on AbortError", async () => {
    fetchSpy.mockRejectedValueOnce(Object.assign(new Error("x"), { name: "AbortError" }));
    expect(await ensureSession("s1")).toEqual({ ok: false, error: "aborted" });
  });

  it("returns the error message on a generic network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    const result = await ensureSession("s1");
    expect(result.ok).toBe(false);
    expect(result.message).toBe("offline");
  });
});

describe("ensureTerminal", () => {
  it("POSTs the terminal path by default", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    expect(await ensureTerminal("s1")).toBe(true);
    expect(lastCall()[0]).toBe("/api/sessions/s1/terminal");
  });

  it("POSTs the container-terminal path when container=true", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    await ensureTerminal("s1", true);
    expect(lastCall()[0]).toBe("/api/sessions/s1/container-terminal");
  });

  it("returns false on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await ensureTerminal("s1")).toBe(false);
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await ensureTerminal("s1")).toBe(false);
  });
});

describe("getSessionDiffFiles", () => {
  it("GETs the diff files endpoint", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ files: [] }));
    const result = await getSessionDiffFiles("s1");
    expect(result).toEqual({ files: [] });
    expect(lastCall()[0]).toBe("/api/sessions/s1/diff/files");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 404 }));
    expect(await getSessionDiffFiles("s1")).toBeNull();
  });
});

describe("getSessionFileContents", () => {
  it("encodes the path param and omits repo when absent", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ patch: "" }));
    await getSessionFileContents("s1", "src/a b.ts");
    const url = String(lastCall()[0]);
    expect(url).toContain("/api/sessions/s1/diff/file?");
    expect(url).toContain("path=src%2Fa+b.ts");
    expect(url).not.toContain("repo=");
  });

  it("adds the repo param when given", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ patch: "" }));
    await getSessionFileContents("s1", "a.ts", "myrepo");
    expect(String(lastCall()[0])).toContain("repo=myrepo");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await getSessionFileContents("s1", "a.ts")).toBeNull();
  });
});

// Settings

describe("fetchSettings", () => {
  it("GETs /api/settings with no profile", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ theme: {} }));
    await fetchSettings();
    expect(lastCall()[0]).toBe("/api/settings");
  });

  it("appends an encoded profile query", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({}));
    await fetchSettings("my profile");
    expect(lastCall()[0]).toBe("/api/settings?profile=my%20profile");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchSettings()).toBeNull();
  });
});

describe("getWebUiState", () => {
  it("GETs the web-ui-state blob", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ k: "v" }));
    expect(await getWebUiState()).toEqual({ k: "v" });
    expect(lastCall()[0]).toBe("/api/app-state/web-ui-state");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await getWebUiState()).toBeNull();
  });
});

describe("patchWebUiState", () => {
  it("PATCHes the partial update (string and null values)", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await patchWebUiState({ keep: "1", drop: null });
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/app-state/web-ui-state");
    expect(init?.method).toBe("PATCH");
    expect(bodyOf(init)).toEqual({ keep: "1", drop: null });
  });

  it("returns false on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 403 }));
    expect(await patchWebUiState({})).toBe(false);
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await patchWebUiState({})).toBe(false);
  });
});

// Sandbox volume ignores

describe("fetchVolumeIgnoresPreview", () => {
  it("GETs the preview with an encoded path, no profile", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ acknowledged: false, globs: [] }));
    const result = await fetchVolumeIgnoresPreview("/repo/a b");
    expect(result).toEqual({ acknowledged: false, globs: [] });
    const url = String(lastCall()[0]);
    expect(url).toContain("/api/sandbox/volume-ignores-preview?");
    expect(url).toContain("path=%2Frepo%2Fa+b");
    expect(url).not.toContain("profile=");
  });

  it("adds the profile param when provided", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ acknowledged: true, globs: [] }));
    await fetchVolumeIgnoresPreview("/repo", "work");
    expect(String(lastCall()[0])).toContain("profile=work");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchVolumeIgnoresPreview("/repo")).toBeNull();
  });

  it("returns null on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await fetchVolumeIgnoresPreview("/repo")).toBeNull();
  });
});

describe("markVolumeIgnoresGlobsAcknowledged", () => {
  it("POSTs the acknowledgement endpoint", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await markVolumeIgnoresGlobsAcknowledged();
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/app-state/volume-ignores-globs-acknowledged");
    expect(init?.method).toBe("POST");
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await markVolumeIgnoresGlobsAcknowledged()).toBe(false);
  });
});

// Profiles

describe("createProfile", () => {
  it("POSTs the name to /api/profiles", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await createProfile("work");
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/profiles");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ name: "work" });
  });

  it("returns false on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 409 }));
    expect(await createProfile("work")).toBe(false);
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await createProfile("work")).toBe(false);
  });
});

describe("deleteProfile", () => {
  it("DELETEs the encoded profile name", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await deleteProfile("my work");
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/profiles/my%20work");
    expect(init?.method).toBe("DELETE");
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await deleteProfile("work")).toBe(false);
  });
});

describe("renameProfile", () => {
  it("PATCHes the rename endpoint with new_name", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await renameProfile("old", "new");
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/profiles/old/rename");
    expect(init?.method).toBe("PATCH");
    expect(bodyOf(init)).toEqual({ new_name: "new" });
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await renameProfile("a", "b")).toBe(false);
  });
});

describe("setDefaultProfile", () => {
  it("PATCHes /api/default-profile with the name", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await setDefaultProfile("work");
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/default-profile");
    expect(init?.method).toBe("PATCH");
    expect(bodyOf(init)).toEqual({ name: "work" });
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await setDefaultProfile("work")).toBe(false);
  });
});

describe("getProfileSettings", () => {
  it("GETs the encoded profile settings endpoint", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ description: "x" }));
    const result = await getProfileSettings("my work");
    expect(result).toEqual({ description: "x" });
    expect(lastCall()[0]).toBe("/api/profiles/my%20work/settings");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 404 }));
    expect(await getProfileSettings("work")).toBeNull();
  });
});

// Themes & Sounds

describe("fetchThemes", () => {
  it("returns the array on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse(["empire", "dracula"]));
    expect(await fetchThemes()).toEqual(["empire", "dracula"]);
    expect(lastCall()[0]).toBe("/api/themes");
  });

  it("coalesces a null result to []", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchThemes()).toEqual([]);
  });
});

describe("fetchResolvedTheme", () => {
  it("GETs the encoded theme name", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ source: "named" }));
    await fetchResolvedTheme("My Theme");
    expect(lastCall()[0]).toBe("/api/themes/My%20Theme");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 404 }));
    expect(await fetchResolvedTheme("x")).toBeNull();
  });
});

describe("fetchCurrentTheme", () => {
  it("GETs /api/theme/current", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ source: "profile" }));
    await fetchCurrentTheme();
    expect(lastCall()[0]).toBe("/api/theme/current");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchCurrentTheme()).toBeNull();
  });
});

describe("fetchSounds", () => {
  it("returns the array on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse(["chime.wav"]));
    expect(await fetchSounds()).toEqual(["chime.wav"]);
    expect(lastCall()[0]).toBe("/api/sounds");
  });

  it("coalesces a null result to []", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchSounds()).toEqual([]);
  });
});

describe("fetchSoundBlob", () => {
  it("returns a Blob from the encoded sound file path", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("bytes", { status: 200 }));
    const blob = await fetchSoundBlob("my sound.wav");
    expect(blob).not.toBeNull();
    expect(await blob!.text()).toBe("bytes");
    expect(lastCall()[0]).toBe("/api/sounds/file/my%20sound.wav");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 404 }));
    expect(await fetchSoundBlob("x.wav")).toBeNull();
  });

  it("returns null on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await fetchSoundBlob("x.wav")).toBeNull();
  });
});

// Telemetry

describe("fetchTelemetryStatus", () => {
  it("GETs the telemetry status endpoint", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ enabled: true, responded: true, do_not_track: false }));
    const result = await fetchTelemetryStatus();
    expect(result?.enabled).toBe(true);
    expect(lastCall()[0]).toBe("/api/telemetry/status");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchTelemetryStatus()).toBeNull();
  });
});

describe("setTelemetryConsent", () => {
  it("POSTs the enabled flag and returns the updated status", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ enabled: true, responded: true, do_not_track: false }));
    const result = await setTelemetryConsent(true);
    expect(result?.enabled).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/telemetry/consent");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ enabled: true });
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await setTelemetryConsent(false)).toBeNull();
  });

  it("returns null on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await setTelemetryConsent(true)).toBeNull();
  });
});

describe("reportTelemetrySeen", () => {
  it("POSTs the surface and form_factor (fire-and-forget)", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    reportTelemetrySeen("web");
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const [url, init] = lastCall();
    expect(url).toBe("/api/telemetry/seen");
    expect(init?.method).toBe("POST");
    const body = bodyOf(init) as { surface: string; form_factor: unknown };
    expect(body.surface).toBe("web");
    expect(body).toHaveProperty("form_factor");
  });

  it("swallows a rejected fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(() => reportTelemetrySeen("diff_panel")).not.toThrow();
  });
});

describe("reportAcpInteraction", () => {
  it("POSTs the kind to the structured-interaction endpoint", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    reportAcpInteraction("prompt_queued");
    const [url, init] = lastCall();
    expect(url).toBe("/api/telemetry/structured-interaction");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ kind: "prompt_queued" });
  });

  it("swallows a rejected fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(() => reportAcpInteraction("prompt_queued")).not.toThrow();
  });
});

// Update status

describe("fetchUpdateStatus", () => {
  it("GETs the update status endpoint", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ update_available: true }));
    const result = await fetchUpdateStatus();
    expect(result?.update_available).toBe(true);
    expect(lastCall()[0]).toBe("/api/system/update-status");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchUpdateStatus()).toBeNull();
  });
});

describe("dismissUpdate", () => {
  it("POSTs the version to dismiss", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await dismissUpdate("1.2.3");
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/app-state/dismiss-update");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ version: "1.2.3" });
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await dismissUpdate("1.2.3")).toBe(false);
  });
});

// Branches

describe("fetchBranches", () => {
  it("GETs branches for a path, no include_remote by default", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse([{ name: "main", is_current: true }]));
    await fetchBranches("/repo");
    const url = String(lastCall()[0]);
    expect(url).toContain("/api/git/branches?");
    expect(url).toContain("path=%2Frepo");
    expect(url).not.toContain("include_remote");
  });

  it("adds include_remote=true when requested", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse([]));
    await fetchBranches("/repo", true);
    expect(String(lastCall()[0])).toContain("include_remote=true");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchBranches("/repo")).toBeNull();
  });
});

// Context primer

describe("fetchContextPrimer", () => {
  it("GETs the primer with an encoded id and before_seq", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ primer: "recap" }));
    await fetchContextPrimer("weird/id", 42);
    const url = String(lastCall()[0]);
    expect(url).toContain("/api/sessions/weird%2Fid/acp/context-primer?");
    expect(url).toContain("before_seq=42");
  });

  it("forwards an abort signal when given", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ primer: "" }));
    const controller = new AbortController();
    await fetchContextPrimer("s1", 1, controller.signal);
    expect(lastCall()[1]?.signal).toBe(controller.signal);
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchContextPrimer("s1", 1)).toBeNull();
  });
});

// Devices

describe("fetchDevices", () => {
  it("GETs /api/devices", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse([{ session_id: "d1", current: true }]));
    const result = await fetchDevices();
    expect(result).toHaveLength(1);
    expect(lastCall()[0]).toBe("/api/devices");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchDevices()).toBeNull();
  });
});

describe("revokeDevice", () => {
  it("DELETEs the encoded login session", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await revokeDevice("sess/1");
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/login/sessions/sess%2F1");
    expect(init?.method).toBe("DELETE");
  });

  it("returns false on an elevation-required 403", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 403 }));
    expect(await revokeDevice("d1")).toBe(false);
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await revokeDevice("d1")).toBe(false);
  });
});

describe("signOutAllDevices", () => {
  it("POSTs the logout-all endpoint", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await signOutAllDevices();
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/login/logout-all");
    expect(init?.method).toBe("POST");
  });

  it("returns false on network failure", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await signOutAllDevices()).toBe(false);
  });
});

// Wizard list helpers

describe("fetchAgents", () => {
  it("returns the array on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse([{ id: "claude" }]));
    expect(await fetchAgents()).toHaveLength(1);
    expect(lastCall()[0]).toBe("/api/agents");
  });

  it("coalesces a null result to []", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchAgents()).toEqual([]);
  });
});

describe("fetchProfiles", () => {
  it("returns the array on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse([{ name: "default" }]));
    expect(await fetchProfiles()).toHaveLength(1);
    expect(lastCall()[0]).toBe("/api/profiles");
  });

  it("coalesces a null result to []", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchProfiles()).toEqual([]);
  });
});

describe("getHomePath", () => {
  it("returns the path field on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ path: "/home/u" }));
    expect(await getHomePath()).toBe("/home/u");
    expect(lastCall()[0]).toBe("/api/filesystem/home");
  });

  it("returns null when the response has no path", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({}));
    expect(await getHomePath()).toBeNull();
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await getHomePath()).toBeNull();
  });
});

describe("browseFilesystem", () => {
  it("GETs with just the path and returns ok=true", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ entries: [{ name: "a" }], has_more: false }));
    const result = await browseFilesystem("/repo");
    expect(result.ok).toBe(true);
    expect(result.entries).toHaveLength(1);
    const url = String(lastCall()[0]);
    expect(url).toContain("/api/filesystem/browse?");
    expect(url).toContain("path=%2Frepo");
    expect(url).not.toContain("limit=");
    expect(url).not.toContain("filter=");
  });

  it("adds limit and filter params", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ entries: [], has_more: true }));
    await browseFilesystem("/repo", 50, "*.ts");
    const url = String(lastCall()[0]);
    expect(url).toContain("limit=50");
    expect(url).toContain("filter=");
  });

  it("returns an empty ok=false envelope on failure", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    const result = await browseFilesystem("/repo");
    expect(result).toEqual({ entries: [], has_more: false, ok: false });
  });
});

describe("fetchGroups", () => {
  it("returns the array on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse([{ path: "g" }]));
    expect(await fetchGroups()).toHaveLength(1);
    expect(lastCall()[0]).toBe("/api/groups");
  });

  it("coalesces a null result to []", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchGroups()).toEqual([]);
  });
});

describe("fetchProjects", () => {
  it("GETs /api/projects with no scope", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse([{ name: "p" }]));
    expect(await fetchProjects()).toHaveLength(1);
    expect(lastCall()[0]).toBe("/api/projects");
  });

  it("appends the scope query when given", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse([]));
    await fetchProjects("profile");
    expect(lastCall()[0]).toBe("/api/projects?scope=profile");
  });

  it("coalesces a null result to []", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchProjects()).toEqual([]);
  });
});

describe("createProject", () => {
  it("POSTs the body and returns the project on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ name: "p", path: "/p" }));
    const result = await createProject({ path: "/p", name: "p", scope: "global" });
    expect(result.ok).toBe(true);
    expect(result.project).toEqual({ name: "p", path: "/p" });
    const [url, init] = lastCall();
    expect(url).toBe("/api/projects");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ path: "/p", name: "p", scope: "global" });
  });

  it("returns a parsed error message on a JSON error body", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "dup" }), { status: 409 }));
    const result = await createProject({ path: "/p" });
    expect(result).toEqual({ ok: false, error: "dup" });
  });

  it("falls back to the raw text on a non-JSON error body", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("boom", { status: 500 }));
    const result = await createProject({ path: "/p" });
    expect(result.ok).toBe(false);
    expect(result.error).toBe("boom");
  });

  it("returns a network error message on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    const result = await createProject({ path: "/p" });
    expect(result).toEqual({ ok: false, error: "offline" });
  });
});

describe("deleteProject", () => {
  it("DELETEs the encoded name with the scope query", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const result = await deleteProject("my proj", "profile");
    expect(result).toEqual({ ok: true });
    const [url, init] = lastCall();
    expect(url).toBe("/api/projects/my%20proj?scope=profile");
    expect(init?.method).toBe("DELETE");
  });

  it("returns a parsed error on a JSON error body", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "nope" }), { status: 409 }));
    const result = await deleteProject("p", "global");
    expect(result).toEqual({ ok: false, error: "nope" });
  });

  it("returns a network error on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    const result = await deleteProject("p", "global");
    expect(result).toEqual({ ok: false, error: "offline" });
  });
});

describe("updateProject", () => {
  it("PATCHes the default_base_branch (string)", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ name: "p" }));
    const result = await updateProject("p", "global", "develop");
    expect(result.ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/projects/p?scope=global");
    expect(init?.method).toBe("PATCH");
    expect(bodyOf(init)).toEqual({ default_base_branch: "develop" });
  });

  it("PATCHes null to clear the base branch", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ name: "p" }));
    await updateProject("p", "global", null);
    expect(bodyOf(lastCall()[1])).toEqual({ default_base_branch: null });
  });

  it("returns a parsed error on a JSON error body", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "bad" }), { status: 400 }));
    const result = await updateProject("p", "global", "x");
    expect(result).toEqual({ ok: false, error: "bad" });
  });

  it("returns a network error on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    const result = await updateProject("p", "global", "x");
    expect(result).toEqual({ ok: false, error: "offline" });
  });
});

describe("setProjectPinned", () => {
  it("PATCHes the pinned flag and returns the project on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ name: "p", pinned: false }));
    const result = await setProjectPinned("p", "global", false);
    expect(result.ok).toBe(true);
    expect(result.project).toEqual({ name: "p", pinned: false });
    const [url, init] = lastCall();
    expect(url).toBe("/api/projects/p?scope=global");
    expect(init?.method).toBe("PATCH");
    expect(bodyOf(init)).toEqual({ pinned: false });
  });

  it("encodes the name and forwards the profile scope", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ name: "a b", pinned: true }));
    await setProjectPinned("a b", "profile", true);
    expect(lastCall()[0]).toBe("/api/projects/a%20b?scope=profile");
    expect(bodyOf(lastCall()[1])).toEqual({ pinned: true });
  });

  it("returns a parsed error on a JSON error body", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "nope" }), { status: 404 }));
    const result = await setProjectPinned("p", "global", true);
    expect(result).toEqual({ ok: false, error: "nope" });
  });

  it("falls back to the raw text on a non-JSON error body", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("boom", { status: 500 }));
    const result = await setProjectPinned("p", "global", true);
    expect(result.ok).toBe(false);
    expect(result.error).toBe("boom");
  });

  it("returns a network error on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    const result = await setProjectPinned("p", "global", true);
    expect(result).toEqual({ ok: false, error: "offline" });
  });
});

describe("fetchDockerStatus", () => {
  it("returns the parsed status on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ available: true, runtime: "docker" }));
    const result = await fetchDockerStatus();
    expect(result).toEqual({ available: true, runtime: "docker" });
    expect(lastCall()[0]).toBe("/api/docker/status");
  });

  it("falls back to an unavailable status on failure", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchDockerStatus()).toEqual({ available: false, runtime: null });
  });
});

// Create session

describe("createSession", () => {
  const body = { path: "/repo", agent: "claude" } as unknown as CreateSessionRequest;

  it("POSTs the body and returns the session on 200", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "s1" }));
    const result = await createSession(body);
    expect(result).toEqual({ ok: true, session: { id: "s1" } });
    const [url, init] = lastCall();
    expect(url).toBe("/api/sessions");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ path: "/repo", agent: "claude" });
  });

  it("surfaces a hooks_need_trust 403 with the command arrays", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          error: "hooks_need_trust",
          message: "trust me",
          on_create: ["a"],
          on_launch: ["b"],
          on_destroy: ["c"],
          needs_mcp_trust: true,
        }),
        { status: 403 },
      ),
    );
    const result = await createSession(body);
    expect(result.ok).toBe(false);
    expect(result.error).toBe("trust me");
    expect(result.hooksNeedTrust).toEqual({
      onCreate: ["a"],
      onLaunch: ["b"],
      onDestroy: ["c"],
      needsMcpTrust: true,
    });
  });

  it("returns the message on a generic JSON error", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "nope" }), { status: 400 }));
    const result = await createSession(body);
    expect(result).toEqual({ ok: false, error: "nope" });
  });

  it("returns a status+text error on a non-JSON error body", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("boom", { status: 500 }));
    const result = await createSession(body);
    expect(result.ok).toBe(false);
    expect(result.error).toContain("Server error (500)");
    expect(result.error).toContain("boom");
  });

  it("returns a network error on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    const result = await createSession(body);
    expect(result.ok).toBe(false);
    expect(result.error).toContain("offline");
  });
});

// Clone

describe("cloneRepo", () => {
  it("POSTs just the url by default", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ path: "/cloned" }));
    const result = await cloneRepo("https://x/y.git");
    expect(result).toEqual({ ok: true, path: "/cloned" });
    const [url, init] = lastCall();
    expect(url).toBe("/api/git/clone");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ url: "https://x/y.git" });
  });

  it("forwards destination/shallow/bare options", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ path: "/c" }));
    await cloneRepo("u", { destination: "/d", shallow: true, bare: true });
    expect(bodyOf(lastCall()[1])).toEqual({ url: "u", destination: "/d", shallow: true, bare: true });
  });

  it("returns the server message on a non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "no repo" }), { status: 404 }));
    const result = await cloneRepo("u");
    expect(result).toEqual({ ok: false, error: "no repo" });
  });

  it("returns a network error on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    const result = await cloneRepo("u");
    expect(result.ok).toBe(false);
    expect(result.error).toContain("offline");
  });
});

// Login

describe("loginStatus", () => {
  it("returns the parsed status on 200", async () => {
    fetchSpy.mockResolvedValueOnce(
      jsonResponse({ required: true, authenticated: false, elevated: false, elevated_until_secs: null }),
    );
    const result = await loginStatus();
    expect(result.required).toBe(true);
    expect(lastCall()[0]).toBe("/api/login/status");
  });

  it("falls back to a permissive default on failure", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await loginStatus()).toEqual({
      required: false,
      authenticated: true,
      elevated: true,
      elevated_until_secs: null,
    });
  });
});

describe("verifyToken", () => {
  it("returns true on an ok status response", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    expect(await verifyToken()).toBe(true);
    expect(lastCall()[0]).toBe("/api/login/status");
  });

  it("returns false on a non-ok response", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 401 }));
    expect(await verifyToken()).toBe(false);
  });

  it("returns false on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await verifyToken()).toBe(false);
  });
});

describe("login", () => {
  it("POSTs the passphrase plus a device-binding secret", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const result = await login("hunter2");
    expect(result).toEqual({ ok: true });
    const [url, init] = lastCall();
    expect(url).toBe("/api/login");
    expect(init?.method).toBe("POST");
    const body = bodyOf(init) as { passphrase: string; device_binding_secret: string };
    expect(body.passphrase).toBe("hunter2");
    expect(typeof body.device_binding_secret).toBe("string");
    expect(body.device_binding_secret.length).toBeGreaterThan(0);
  });

  it("returns the server message on a non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "wrong" }), { status: 401 }));
    const result = await login("bad");
    expect(result).toEqual({ ok: false, error: "wrong" });
  });

  it("returns a network error on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await login("x")).toEqual({ ok: false, error: "Network error" });
  });
});

describe("elevateLogin", () => {
  it("POSTs the passphrase with the device-binding header and returns the window", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ elevated_until_secs: 900 }));
    const result = await elevateLogin("hunter2");
    expect(result.ok).toBe(true);
    expect(result.elevated_until_secs).toBe(900);
    const [url, init] = lastCall();
    expect(url).toBe("/api/login/elevate");
    expect(init?.method).toBe("POST");
    const headers = init?.headers as Record<string, string>;
    expect(headers["X-Aoe-Device-Binding"]).toBeTruthy();
    expect(bodyOf(init)).toEqual({ passphrase: "hunter2" });
  });

  it("returns the server message on a non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "denied" }), { status: 401 }));
    const result = await elevateLogin("bad");
    expect(result).toEqual({ ok: false, error: "denied" });
  });

  it("returns a network error on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await elevateLogin("x")).toEqual({ ok: false, error: "Network error" });
  });
});

describe("logout", () => {
  it("POSTs /api/logout", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    await logout();
    const [url, init] = lastCall();
    expect(url).toBe("/api/logout");
    expect(init?.method).toBe("POST");
  });

  it("resolves even when the logout fetch rejects (best effort)", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    await expect(logout()).resolves.toBeUndefined();
  });
});

// Session mutations

describe("renameSession", () => {
  it("PATCHes the title and returns ok on 200", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const result = await renameSession("s1", "New Title");
    expect(result).toEqual({ ok: true });
    const [url, init] = lastCall();
    expect(url).toBe("/api/sessions/s1");
    expect(init?.method).toBe("PATCH");
    expect(bodyOf(init)).toEqual({ title: "New Title" });
  });

  it("surfaces the server message on a 409", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "running" }), { status: 409 }));
    const result = await renameSession("s1", "x");
    expect(result).toEqual({ ok: false, message: "running" });
  });

  it("returns ok=false with no message on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await renameSession("s1", "x")).toEqual({ ok: false });
  });
});

describe("setWorktreeName", () => {
  it("PATCHes the name and rename_branch flag", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const result = await setWorktreeName("s1", "feature", true);
    expect(result).toEqual({ ok: true });
    const [url, init] = lastCall();
    expect(url).toBe("/api/sessions/s1/worktree-name");
    expect(init?.method).toBe("PATCH");
    expect(bodyOf(init)).toEqual({ name: "feature", rename_branch: true });
  });

  it("surfaces the server message on a non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "invalid" }), { status: 400 }));
    const result = await setWorktreeName("s1", "x", false);
    expect(result).toEqual({ ok: false, message: "invalid" });
  });

  it("returns ok=false on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await setWorktreeName("s1", "x", false)).toEqual({ ok: false });
  });
});

describe("setSessionNotifications", () => {
  it("sends all-false for the off preset", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await setSessionNotifications("s1", "off");
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/sessions/s1/notifications");
    expect(init?.method).toBe("PATCH");
    expect(bodyOf(init)).toEqual({
      notify_on_waiting: false,
      notify_on_idle: false,
      notify_on_error: false,
    });
  });

  it("sends all-true for the all preset", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    await setSessionNotifications("s1", "all");
    expect(bodyOf(lastCall()[1])).toEqual({
      notify_on_waiting: true,
      notify_on_idle: true,
      notify_on_error: true,
    });
  });

  it("sends all-null for the default preset", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    await setSessionNotifications("s1", "default");
    expect(bodyOf(lastCall()[1])).toEqual({
      notify_on_waiting: null,
      notify_on_idle: null,
      notify_on_error: null,
    });
  });

  it("returns false on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await setSessionNotifications("s1", "off")).toBe(false);
  });
});

describe("setSessionDiffBase", () => {
  it("PATCHes a base branch and returns the session", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "s1" }));
    const result = await setSessionDiffBase("s1", "develop");
    expect(result).toEqual({ id: "s1" });
    const [url, init] = lastCall();
    expect(url).toBe("/api/sessions/s1/diff-base");
    expect(init?.method).toBe("PATCH");
    expect(bodyOf(init)).toEqual({ base_branch: "develop" });
  });

  it("PATCHes null to clear the override", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "s1" }));
    await setSessionDiffBase("s1", null);
    expect(bodyOf(lastCall()[1])).toEqual({ base_branch: null });
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await setSessionDiffBase("s1", "x")).toBeNull();
  });

  it("returns null on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await setSessionDiffBase("s1", "x")).toBeNull();
  });
});

describe("stopSession", () => {
  it("POSTs the stop endpoint and returns the session", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "s1", status: "Stopped" }));
    const result = await stopSession("s1");
    expect(result).toEqual({ id: "s1", status: "Stopped" });
    const [url, init] = lastCall();
    expect(url).toBe("/api/sessions/s1/stop");
    expect(init?.method).toBe("POST");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await stopSession("s1")).toBeNull();
  });

  it("returns null on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await stopSession("s1")).toBeNull();
  });
});

describe("startSession", () => {
  it("POSTs the start endpoint and returns the session", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "s1", status: "Running" }));
    const result = await startSession("s1");
    expect(result).toEqual({ id: "s1", status: "Running" });
    const [url, init] = lastCall();
    expect(url).toBe("/api/sessions/s1/start");
    expect(init?.method).toBe("POST");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await startSession("s1")).toBeNull();
  });

  it("returns null on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await startSession("s1")).toBeNull();
  });
});

describe("deleteSession", () => {
  it("DELETEs with the default empty options and returns messages", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ messages: ["removed worktree"] }));
    const result = await deleteSession("s1");
    expect(result).toEqual({ ok: true, messages: ["removed worktree"] });
    const [url, init] = lastCall();
    expect(url).toBe("/api/sessions/s1");
    expect(init?.method).toBe("DELETE");
    expect(bodyOf(init)).toEqual({});
  });

  it("forwards the delete options on the body", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({}));
    await deleteSession("s1", { delete_worktree: true, delete_branch: true });
    expect(bodyOf(lastCall()[1])).toEqual({ delete_worktree: true, delete_branch: true });
  });

  it("returns the error message on a non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify({ message: "in use" }), { status: 409 }));
    const result = await deleteSession("s1");
    expect(result).toEqual({ ok: false, error: "in use" });
  });

  it("returns a network error on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    const result = await deleteSession("s1");
    expect(result.ok).toBe(false);
    expect(result.error).toContain("offline");
  });
});

// MCP servers

describe("fetchMcpServers", () => {
  it("GETs without an agent query by default", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ agent: "claude", effective: [] }));
    await fetchMcpServers();
    expect(lastCall()[0]).toBe("/api/mcp/servers");
  });

  it("appends the encoded agent query", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ agent: "my agent", effective: [] }));
    await fetchMcpServers("my agent");
    expect(lastCall()[0]).toBe("/api/mcp/servers?agent=my%20agent");
  });

  it("returns null on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchMcpServers()).toBeNull();
  });
});

describe("resolveMcpConflict", () => {
  it("POSTs the winner and fingerprint, returns applied on 200", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const result = await resolveMcpConflict("ctx7", "claude", "aoe", "fp1");
    expect(result).toBe("applied");
    const [url, init] = lastCall();
    expect(url).toBe("/api/mcp/servers/ctx7/resolve");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ agent: "claude", winner: "aoe", fingerprint: "fp1" });
  });

  it("returns stale on a 409", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 409 }));
    expect(await resolveMcpConflict("n", "a", "native", "fp")).toBe("stale");
  });

  it("returns error on another non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await resolveMcpConflict("n", "a", "aoe", "fp")).toBe("error");
  });

  it("returns error on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await resolveMcpConflict("n", "a", "aoe", "fp")).toBe("error");
  });
});

describe("keepMcpServer", () => {
  it("POSTs the keep endpoint and returns true on 200", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await keepMcpServer("ctx7", "claude");
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/mcp/servers/ctx7/keep");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ agent: "claude" });
  });

  it("returns false on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await keepMcpServer("n", "a")).toBe(false);
  });

  it("returns false on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await keepMcpServer("n", "a")).toBe(false);
  });
});

describe("dropMcpServer", () => {
  it("POSTs the drop endpoint and returns true on 200", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 200 }));
    const ok = await dropMcpServer("ctx7", "claude");
    expect(ok).toBe(true);
    const [url, init] = lastCall();
    expect(url).toBe("/api/mcp/servers/ctx7/drop");
    expect(init?.method).toBe("POST");
    expect(bodyOf(init)).toEqual({ agent: "claude" });
  });

  it("returns false on non-2xx", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await dropMcpServer("n", "a")).toBe(false);
  });

  it("returns false on a thrown fetch", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await dropMcpServer("n", "a")).toBe(false);
  });
});

// Vitest coverage for the sandbox volume_ignores glob API client (#2045):
// the dry-run preview fetch and the acknowledge POST. Both swallow network
// and non-OK responses so the wizard treats a failure as "nothing to confirm"
// rather than blocking session creation.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { fetchVolumeIgnoresPreview, markVolumeIgnoresGlobsAcknowledged } from "../api";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("fetchVolumeIgnoresPreview (#2045)", () => {
  it("returns the parsed preview and encodes path + profile in the query", async () => {
    const payload = {
      acknowledged: false,
      globs: [{ pattern: "**/bin", matched_paths: ["/workspace/x/src/bin"] }],
    };
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(payload), { status: 200 }));

    const result = await fetchVolumeIgnoresPreview("/repo/with space", "prof");

    expect(result).toEqual(payload);
    const url = new URL(fetchSpy.mock.calls[0][0] as string, "http://localhost");
    expect(url.pathname).toBe("/api/sandbox/volume-ignores-preview");
    expect(url.searchParams.get("path")).toBe("/repo/with space");
    expect(url.searchParams.get("profile")).toBe("prof");
  });

  it("omits the profile param when none is given", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ acknowledged: true, globs: [] }), { status: 200 }));

    await fetchVolumeIgnoresPreview("/repo");

    const url = new URL(fetchSpy.mock.calls[0][0] as string, "http://localhost");
    expect(url.searchParams.has("profile")).toBe(false);
  });

  it("returns null on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response("nope", { status: 500 }));
    expect(await fetchVolumeIgnoresPreview("/repo")).toBeNull();
  });

  it("returns null when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("network down"));
    expect(await fetchVolumeIgnoresPreview("/repo")).toBeNull();
  });
});

describe("markVolumeIgnoresGlobsAcknowledged (#2045)", () => {
  it("POSTs to the acknowledge endpoint and returns true on success", async () => {
    fetchSpy.mockResolvedValue(new Response("{}", { status: 200 }));

    expect(await markVolumeIgnoresGlobsAcknowledged()).toBe(true);
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/app-state/volume-ignores-globs-acknowledged");
    expect(init?.method).toBe("POST");
  });

  it("returns false on a non-OK response (e.g. read-only 403)", async () => {
    fetchSpy.mockResolvedValue(new Response("forbidden", { status: 403 }));
    expect(await markVolumeIgnoresGlobsAcknowledged()).toBe(false);
  });

  it("returns false when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("network down"));
    expect(await markVolumeIgnoresGlobsAcknowledged()).toBe(false);
  });
});

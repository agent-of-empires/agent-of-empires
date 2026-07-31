// Vitest coverage for the CityHall bundle export client. Unlike most of api.ts
// this one throws rather than returning null, because the Settings panel has to
// show the admin *why* an export failed: the endpoint is refused outright in
// CityHall client mode, and "nothing happened" would be indistinguishable from
// a network blip.

import { beforeEach, describe, expect, it, vi } from "vitest";

import { fetchCityHallBundle } from "../api";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

describe("fetchCityHallBundle", () => {
  it("returns the TOML body on success", async () => {
    const body = 'schema_version = 1\n\n[settings.acp]\ndefault_agent = "claude-code"\n';
    fetchSpy.mockResolvedValue(new Response(body, { status: 200 }));

    await expect(fetchCityHallBundle()).resolves.toBe(body);
    expect(fetchSpy).toHaveBeenCalledWith("/api/cityhall/bundle");
  });

  it("throws the server's message when the endpoint refuses", async () => {
    fetchSpy.mockResolvedValue(
      new Response(
        JSON.stringify({ error: "cityhall_mode", message: "This action is disabled in CityHall client mode" }),
        {
          status: 403,
        },
      ),
    );

    await expect(fetchCityHallBundle()).rejects.toThrow(/disabled in CityHall client mode/);
  });

  // A proxy or a crash can return a non-JSON error body; falling back to the
  // status keeps the panel from rendering "undefined".
  it("falls back to the status code when the error body is not JSON", async () => {
    fetchSpy.mockResolvedValue(new Response("<html>502</html>", { status: 502 }));

    await expect(fetchCityHallBundle()).rejects.toThrow("Export failed (HTTP 502)");
  });
});

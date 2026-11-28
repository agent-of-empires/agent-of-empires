import { afterEach, expect, it, vi } from "vitest";

vi.unmock("./featureUse");
const { reportFeatureUse } = await vi.importActual<typeof import("./featureUse")>("./featureUse");

afterEach(() => {
  vi.unstubAllGlobals();
});

it("POSTs the feature fire-and-forget and swallows failures", async () => {
  const fetchSpy = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
  vi.stubGlobal("fetch", fetchSpy);
  reportFeatureUse("sort_az");
  expect(fetchSpy).toHaveBeenCalledWith(
    "/api/telemetry/feature-use",
    expect.objectContaining({
      method: "POST",
      keepalive: true,
      body: JSON.stringify({ feature: "sort_az" }),
    }),
  );

  vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("offline")));
  expect(() => reportFeatureUse("cheat_code")).not.toThrow();
});

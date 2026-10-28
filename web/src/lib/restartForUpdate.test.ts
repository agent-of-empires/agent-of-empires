import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { restartForUpdate } from "./restartForUpdate";
import { toastBus } from "./toastBus";

const restartSessionForUpdate = vi.hoisted(() => vi.fn());
vi.mock("./api", () => ({ restartSessionForUpdate }));

const handler = { error: vi.fn(), info: vi.fn() };

beforeEach(() => {
  vi.clearAllMocks();
  toastBus.handler = handler as unknown as typeof toastBus.handler;
});
afterEach(() => {
  toastBus.handler = null;
});

it("tells the user the conversation is kept on success, and the server message on refusal", async () => {
  restartSessionForUpdate.mockResolvedValueOnce({ ok: true });
  await restartForUpdate("s1");
  expect(restartSessionForUpdate).toHaveBeenCalledWith("s1");
  expect(handler.info).toHaveBeenCalledWith(expect.stringContaining("conversation is kept"));
  expect(handler.error).not.toHaveBeenCalled();

  restartSessionForUpdate.mockResolvedValueOnce({ ok: false, message: "Session is busy" });
  await restartForUpdate("s1");
  expect(handler.error).toHaveBeenCalledWith("Session is busy");

  restartSessionForUpdate.mockResolvedValueOnce({ ok: false });
  await restartForUpdate("s1");
  expect(handler.error).toHaveBeenLastCalledWith("Could not restart the session");
});

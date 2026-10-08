import { test, expect, bootDashboard } from "../helpers/liveTest";
import { seedSessionViaAoeAdd } from "../helpers/aoeServe";

function cursor(headers: Record<string, string>) {
  const epoch = headers["aoe-runtime-epoch"];
  const raw = headers["aoe-runtime-revision"];
  expect(epoch).toBeTruthy();
  expect(raw).toMatch(/^\d+$/);
  return { epoch: epoch!, revision: BigInt(raw!) };
}
async function readList(baseUrl: string) {
  const response = await fetch(baseUrl + "/api/sessions");
  expect(response.ok).toBe(true);
  const current = cursor(Object.fromEntries(response.headers.entries()));
  const body = (await response.json()) as {
    sessions: Array<{
      id: string;
      color: string | null;
      notify_on_waiting: boolean | null;
      notify_on_idle: boolean | null;
      notify_on_error: boolean | null;
    }>;
  };
  return { ...current, sessions: body.sessions };
}

test("isolated serve publishes advancing native epoch revision receipts for color and notifications", async ({
  page,
  spawnServe,
}) => {
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "native-receipts" }) });
  const before = await readList(serve.baseUrl);
  expect(before.sessions).toHaveLength(1);
  const id = before.sessions[0]!.id;
  await bootDashboard(page, serve);
  const row = page.getByTestId("sidebar-session-row").filter({ hasText: "native-receipts" });
  await expect(row).toBeVisible({ timeout: 10_000 });
  await row.click({ button: "right" });
  const colorResponse = page.waitForResponse(
    (r) => new URL(r.url()).pathname === "/api/sessions/" + id + "/color" && r.request().method() === "PATCH",
  );
  await page.getByTestId("sidebar-context-menu-color-red").click();
  const color = await colorResponse;
  expect(color.ok()).toBe(true);
  expect(color.request().postDataJSON()).toEqual({ color: "red" });
  const colorCursor = cursor(await color.allHeaders());
  expect(colorCursor.epoch).toBe(before.epoch);
  expect(colorCursor.revision > before.revision).toBe(true);
  expect(await color.json()).toMatchObject({ id, color: "red" });
  await expect(row.getByTestId("sidebar-session-color-dot")).toHaveAttribute("data-color", "red");
  const notifyResponse = page.waitForResponse(
    (r) => new URL(r.url()).pathname === "/api/sessions/" + id + "/notifications" && r.request().method() === "PATCH",
  );
  await page.getByTestId("sidebar-context-menu-notify-all").click();
  const notify = await notifyResponse;
  expect(notify.ok()).toBe(true);
  expect(notify.request().postDataJSON()).toEqual({
    notify_on_waiting: true,
    notify_on_idle: true,
    notify_on_error: true,
  });
  const notifyCursor = cursor(await notify.allHeaders());
  expect(notifyCursor.epoch).toBe(colorCursor.epoch);
  expect(notifyCursor.revision > colorCursor.revision).toBe(true);
  expect(await notify.json()).toMatchObject({
    id,
    color: "red",
    notify_on_waiting: true,
    notify_on_idle: true,
    notify_on_error: true,
  });
  await expect(page.getByTestId("sidebar-context-menu-notify-all")).toHaveAttribute("aria-pressed", "true");
  await expect
    .poll(async () => {
      const list = await readList(serve.baseUrl);
      const saved = list.sessions.find((session) => session.id === id);
      return (
        list.epoch === notifyCursor.epoch &&
        list.revision >= notifyCursor.revision &&
        saved?.color === "red" &&
        saved.notify_on_waiting === true &&
        saved.notify_on_idle === true &&
        saved.notify_on_error === true
      );
    })
    .toBe(true);
  // A fresh browser mount must derive the choices from the backend, not the pending overlay.
  await page.reload();
  await expect(row).toBeVisible({ timeout: 10_000 });
  await row.click({ button: "right" });
  await expect(page.getByTestId("sidebar-context-menu-color-red")).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByTestId("sidebar-context-menu-notify-all")).toHaveAttribute("aria-pressed", "true");
});

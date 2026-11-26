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
      notify_on_waiting: boolean | null;
      notify_on_idle: boolean | null;
      notify_on_error: boolean | null;
    }>;
  };
  return { ...current, sessions: body.sessions };
}

test("isolated serve publishes advancing native epoch revision receipts for notifications", async ({
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
  const pick = async (preset: "all" | "off") => {
    const response = page.waitForResponse(
      (r) => new URL(r.url()).pathname === "/api/sessions/" + id + "/notifications" && r.request().method() === "PATCH",
    );
    await page.getByTestId("sidebar-context-menu-notify-" + preset).click();
    const settled = await response;
    expect(settled.ok()).toBe(true);
    const on = preset === "all";
    const values = { notify_on_waiting: on, notify_on_idle: on, notify_on_error: on };
    expect(settled.request().postDataJSON()).toEqual(values);
    expect(await settled.json()).toMatchObject({ id, ...values });
    await expect(page.getByTestId("sidebar-context-menu-notify-" + preset)).toHaveAttribute("aria-pressed", "true");
    return cursor(await settled.allHeaders());
  };
  const all = await pick("all");
  expect(all.epoch).toBe(before.epoch);
  expect(all.revision > before.revision).toBe(true);
  const off = await pick("off");
  expect(off.epoch).toBe(all.epoch);
  expect(off.revision > all.revision).toBe(true);
  await expect
    .poll(async () => {
      const list = await readList(serve.baseUrl);
      const saved = list.sessions.find((session) => session.id === id);
      return (
        list.epoch === off.epoch &&
        list.revision >= off.revision &&
        saved?.notify_on_waiting === false &&
        saved.notify_on_idle === false &&
        saved.notify_on_error === false
      );
    })
    .toBe(true);
  // A fresh browser mount must derive the choice from the backend, not the pending overlay.
  await page.reload();
  await expect(row).toBeVisible({ timeout: 10_000 });
  await row.click({ button: "right" });
  await expect(page.getByTestId("sidebar-context-menu-notify-off")).toHaveAttribute("aria-pressed", "true");
});

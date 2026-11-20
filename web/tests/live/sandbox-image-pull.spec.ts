// A sandbox image pull against a real server (#5751): its progress in the banner and chip, and Cancel.

import { appendFileSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../helpers/liveTest";
import { appDirFor, resolveAoeBinary } from "../helpers/aoeServe";

// Docker 29.5.2's stdout for a pull, captured with its layers; the registry check can't be faked, so the
// test starts the pull through the API instead of waiting for an update to show.
const IMAGE = "ghcr.io/example/aoe-sandbox:live";
const FIXTURE = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../tests/fixtures/pull-progress/docker-29.5.2-cancel.txt",
);

/** A `docker` whose pull of `image` prints the captured lines, then holds until it is killed. Other pulls,
 *  such as a migration's, fail at once. */
function fakeDocker(shimBin: string, home: string, image: string) {
  const lines = join(home, "pull-lines.txt");
  const output = readFileSync(FIXTURE, "utf8")
    .split("\n")
    .filter((line) => line && !line.startsWith("#"));
  writeFileSync(lines, `${output.join("\n")}\n`);
  writeFileSync(
    join(shimBin, "docker"),
    `#!/bin/sh
case "$1" in
  info) exit 0 ;;
  pull) [ "$2" = "${image}" ] || exit 1; cat "${lines}"; exec sleep 600 ;;
  *) echo "fake docker: $*" >&2; exit 1 ;;
esac
`,
    { mode: 0o755 },
  );
}

test("a running pull shows its layers, hides to the chip and cancels for everyone", async ({ page, spawnServe }) => {
  const serve = await spawnServe({
    seedFn: ({ home, shimBin, env }) => {
      fakeDocker(shimBin, home, IMAGE);
      const appDir = appDirFor(home, env.XDG_CONFIG_HOME!, resolveAoeBinary());
      appendFileSync(
        join(appDir, "config.toml"),
        `\n[sandbox]\nenabled_by_default = true\nallow_image_pull = true\ncontainer_runtime = "docker"\ndefault_image = "${IMAGE}"\n`,
      );
    },
  });
  const status = async () =>
    (await (await fetch(`${serve.baseUrl}/api/sandbox/image-status`)).json()) as {
      pull: { state: string; layers_done?: number; layers_known?: number };
    };

  const started = await fetch(`${serve.baseUrl}/api/sandbox/image-pull`, { method: "POST" });
  expect(started.status).toBe(202);
  // The fake prints every line at once, so the counts settle on the fixture's.
  await expect.poll(async () => (await status()).pull, { timeout: 15_000 }).toMatchObject({ state: "running" });
  await expect.poll(async () => (await status()).pull.layers_known ?? 0, { timeout: 15_000 }).toBeGreaterThan(0);
  const { layers_done: done, layers_known: known } = (await status()).pull;

  // The server can reach GitHub, and AoE's own update would outrank this banner.
  await page.route("**/api/system/update-status", (r) =>
    r.fulfill({
      json: {
        update_check_mode: "notify",
        current_version: "1.0.0",
        latest_version: "1.0.0",
        update_available: false,
        release_url: null,
        error: null,
        dismissed_version: null,
      },
    }),
  );
  await page.goto(`${serve.baseUrl}/`);
  const banner = page.getByTestId("sandbox-image-banner");
  await expect(banner.getByRole("status")).toHaveText("Downloading sandbox image…");
  await expect(banner).toContainText(`${done} of ${known} layers`);

  await banner.getByRole("button", { name: "Hide sandbox image notice" }).click();
  const chip = page.getByRole("button", {
    name: `Downloading sandbox image, ${done} of ${known} layers. Show details`,
  });
  await expect(chip).toHaveText(`Image ${done}/${known}`);
  await chip.click();

  await banner.getByRole("button", { name: "Cancel" }).click();
  await expect(banner.getByRole("status")).toHaveText("Update cancelled. Sessions keep the current image.");
  expect((await status()).pull.state).toBe("cancelled");
});

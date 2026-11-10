import { test, expect } from "./helpers/mockedTest";
import { mockSettingsApis, mockStaticApis } from "./helpers/apiMocks";

const agent = {
  tool: "helper",
  command: "helper --serve",
  replies: false,
  launch: { container_env: { AGENT_HOME: "/tmp\u001b[2J" }, install_hint: "install \u202enow" },
  detection: { mode: "native_only" },
  reply_fallback: { allow: [{ kind: "literal", value: "y" }], deny: [{ kind: "named", value: "Escape" }] },
};
const common = { ui: [], agent_integrations: [agent], build_steps: [], fingerprint: "tree" };

test("install and update show safe agent declarations before approval", async ({ page }) => {
  await mockStaticApis(page);
  await mockSettingsApis(page);
  await page.route("**/api/plugins", (route) =>
    route.fulfill({
      json: {
        plugins: [
          {
            id: "example.plugin",
            name: "Example",
            version: "1.0.0",
            description: "",
            enabled: true,
            builtin: false,
            validation: "community",
            source: "gh:example/plugin",
            capabilities: [],
            ui_contributions: [],
            granted: true,
            needs_reapproval: false,
          },
        ],
        load_errors: [],
      },
    }),
  );
  await page.route("**/api/plugins/updates", (route) =>
    route.fulfill({
      json: {
        updates: [
          {
            id: "example.plugin",
            source: "gh:example/plugin",
            current: "old",
            available: "new",
            needs_update: true,
            error: null,
          },
        ],
      },
    }),
  );
  await page.route("**/api/plugins/example.plugin/update/preview", (route) =>
    route.fulfill({
      json: {
        kind: "consent_required",
        dismissed: false,
        consent: {
          ...common,
          id: "example.plugin",
          from_version: "1.0.0",
          to_version: "2.0.0",
          prior_capabilities: [],
          new_capabilities: [],
          added_capabilities: [],
          removed_capabilities: [],
          runtime_change: null,
          trust_downgrade: false,
          stays_active_if_declined: true,
          changelog: { entries: [], truncated: false, unavailable_reason: null, more_url: null },
        },
      },
    }),
  );
  await page.route("**/api/plugins/discover*", (route) =>
    route.fulfill({
      json: {
        results: [
          {
            slug: "gh:acme/widget",
            html_url: "https://github.com/acme/widget",
            description: "Widget",
            stars: 1,
            badge: "unvetted",
            install_command: "aoe plugin install gh:acme/widget",
          },
        ],
      },
    }),
  );
  await page.route("**/api/plugins/install/preview", (route) =>
    route.fulfill({
      json: {
        ...common,
        id: "acme.widget",
        version: "1.0.0",
        source: "gh:acme/widget",
        notice: "Install",
        unverified: false,
        validation: "community",
        capabilities: [],
      },
    }),
  );
  await page.goto("/settings/plugins");
  await page.getByTestId("plugins-check-updates").click();
  await page.getByTestId("plugin-update-example.plugin").click();
  const update = page.getByTestId("plugin-update-agent-integrations");
  await expect(update).toContainText("sandbox environment: AGENT_HOME=/tmp\\u{1b}[2J");
  await expect(update).toContainText("install: install \\u{202e}now");
  await expect(update).toContainText('reply fallback: allow ["y"], deny [Escape]');
  await page.getByTestId("plugin-update-consent-close").click();
  await page.getByTestId("plugins-tab-marketplace").click();
  await page.getByTestId("plugins-discover").click();
  await page.getByTestId("plugins-install-gh:acme/widget").click();
  const install = page.getByTestId("plugin-install-agent-integrations");
  await expect(install).toContainText("sandbox environment: AGENT_HOME=/tmp\\u{1b}[2J");
  await expect(install).toContainText("install: install \\u{202e}now");
  await expect(install).toContainText("detection: native_only");
  await expect(install).not.toContainText("\u001b");
  await expect(install).not.toContainText("\u202e");
});

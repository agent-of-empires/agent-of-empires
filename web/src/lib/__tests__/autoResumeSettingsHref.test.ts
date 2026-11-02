import { expect, it } from "vitest";
import { autoResumeSettingsHref } from "../autoResumeSettingsHref";
import type { ServerAbout } from "../api";

it("links the auto-resume row only where Settings can edit it", () => {
  const row = "/settings/structured-view?profile=my%20work&field=acp.rate_limit_auto_resume";
  const cases: [string, Partial<ServerAbout> | null, string | null][] = [
    ["editable", { read_only: false, cityhall_mode: false }, row],
    ["not loaded yet", null, row],
    ["read-only", { read_only: true, cityhall_mode: false }, null],
    ["CityHall", { read_only: false, cityhall_mode: true }, null],
  ];
  for (const [label, about, href] of cases) {
    expect(autoResumeSettingsHref("my work", about as ServerAbout | null), label).toBe(href);
  }
});

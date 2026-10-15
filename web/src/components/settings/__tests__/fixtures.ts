import type { SettingsFieldDescriptor } from "../../../lib/types";

/** A schema field descriptor with the boring defaults filled in. */
export function descriptor(
  over: Partial<SettingsFieldDescriptor> & Pick<SettingsFieldDescriptor, "section" | "field" | "label">,
): SettingsFieldDescriptor {
  return {
    category: "Sandbox",
    description: "",
    widget: { kind: "toggle" },
    web_write: { policy: "allow" },
    profile_overridable: true,
    validation: { rule: "none" },
    advanced: false,
    ...over,
  };
}

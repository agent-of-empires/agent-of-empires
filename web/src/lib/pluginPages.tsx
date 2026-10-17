import { PluginIdentityIcon } from "../components/settings/PluginIdentityIcon";
import type { CommandAction } from "../components/command-palette/types";
import type { PluginPage } from "./api";
import { requestNavigate } from "./sessionRoute";

export function pluginPagePath(page: Pick<PluginPage, "plugin_id" | "id">): string {
  return `/plugins/${encodeURIComponent(page.plugin_id)}/${encodeURIComponent(page.id)}`;
}

export function buildPluginPageActions(pages: PluginPage[]): CommandAction[] {
  return pages.map((page) => ({
    id: `plugin-page:${page.plugin_id}:${page.id}`,
    title: page.title,
    subtitle: page.plugin_id,
    group: "Pages",
    keywords: ["page", "plugin", page.plugin_id, page.id],
    icon: <PluginIdentityIcon icon={page.icon} />,
    perform: () => requestNavigate(pluginPagePath(page)),
  }));
}

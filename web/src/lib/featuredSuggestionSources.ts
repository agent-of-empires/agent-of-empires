import { discoverPlugins, type PluginDiscoveryResult } from "./api";

export type FeaturedSources = { status: "ok"; sources: PluginDiscoveryResult[] } | { status: "error"; message: string };

// Discovery searches GitHub, which rate-limits anonymous callers, so one lookup serves the whole page load.
let cached: Promise<FeaturedSources> | null = null;

/** Featured providers that declare `[suggestions]`; failures are not cached, so Try again re-asks. */
export function loadFeaturedSuggestionSources(): Promise<FeaturedSources> {
  cached ??= discoverPlugins("").then((res) => {
    if (res.kind === "ok") {
      return { status: "ok", sources: res.results.filter((r) => r.badge === "featured" && r.suggestions) };
    }
    cached = null;
    return { status: "error", message: res.message };
  });
  return cached;
}

/** A sentence with a next step for a failed lookup; `raw` is the server's reason, for an unknown failure's tooltip. */
export function discoveryFailureText(message: string): { text: string; raw?: string } {
  if (/rate limit/i.test(message)) return { text: "GitHub is rate limiting requests. Try again in a few minutes." };
  if (/unreachable|network error/i.test(message)) {
    return { text: "Couldn't reach GitHub. Check your connection and try again." };
  }
  return { text: "Couldn't look up suggestion sources.", raw: message };
}

export function clearFeaturedSuggestionSources(): void {
  cached = null;
}

// Until manifest labels are readable before install, derive a name from the `owner/plugin-name` slug.
const BRAND_CASING: Record<string, string> = { github: "GitHub" };

export function sourceLabel(slug: string): string {
  const name = (slug.split("/").pop() ?? slug).replace(/^plugin-/, "");
  return BRAND_CASING[name] ?? name.charAt(0).toUpperCase() + name.slice(1);
}

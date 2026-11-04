import type { ServerAbout } from "./api";

/** Waits between the dashboard's first /api/about tries: a blip at load shouldn't decide its shell. */
export const ABOUT_RETRY_DELAYS_MS = [500, 1500] as const;

/**
 * Ask for /api/about until it answers, waiting `delaysMs[i]` before try `i + 2`. Null when every try
 * failed, or when `stopped()` turns true while waiting.
 */
export async function fetchAboutRetrying(
  fetchOnce: () => Promise<ServerAbout | null>,
  delaysMs: readonly number[] = ABOUT_RETRY_DELAYS_MS,
  stopped: () => boolean = () => false,
): Promise<ServerAbout | null> {
  for (let attempt = 0; ; attempt++) {
    const about = await fetchOnce();
    if (about || attempt >= delaysMs.length || stopped()) return about;
    await new Promise((resolve) => setTimeout(resolve, delaysMs[attempt]));
    if (stopped()) return null;
  }
}

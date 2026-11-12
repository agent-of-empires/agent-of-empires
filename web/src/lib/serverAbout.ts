import type { ServerAbout } from "./api";

/** Waits between the dashboard's first /api/about tries: a blip at load shouldn't decide its shell. */
export const ABOUT_RETRY_DELAYS_MS = [500, 1500] as const;

/** How long one /api/about try may take before it counts as failed, so a hung request can't hold the shell. */
export const ABOUT_TRY_TIMEOUT_MS = 5000;

/** One /api/about try; null once it outlasts `timeoutMs`, when its request is aborted. */
export async function fetchAboutOnce(
  fetchOnce: (signal: AbortSignal) => Promise<ServerAbout | null>,
  timeoutMs: number = ABOUT_TRY_TIMEOUT_MS,
): Promise<ServerAbout | null> {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<null>((resolve) => {
    timer = setTimeout(() => {
      controller.abort();
      resolve(null);
    }, timeoutMs);
  });
  try {
    return await Promise.race([fetchOnce(controller.signal), deadline]);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Ask for /api/about until it answers, waiting `delaysMs[i]` before try `i + 2`; each try fails after
 * `timeoutMs`. Null when every try failed, or when `stopped()` turns true while waiting.
 */
export async function fetchAboutRetrying(
  fetchOnce: (signal: AbortSignal) => Promise<ServerAbout | null>,
  delaysMs: readonly number[] = ABOUT_RETRY_DELAYS_MS,
  stopped: () => boolean = () => false,
  timeoutMs: number = ABOUT_TRY_TIMEOUT_MS,
): Promise<ServerAbout | null> {
  for (let attempt = 0; ; attempt++) {
    const about = await fetchAboutOnce(fetchOnce, timeoutMs);
    if (about || attempt >= delaysMs.length || stopped()) return about;
    await new Promise((resolve) => setTimeout(resolve, delaysMs[attempt]));
    if (stopped()) return null;
  }
}

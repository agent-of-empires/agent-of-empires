# Adding a new page to the website

The website (agent-of-empires.com) is an Astro site in `website/`. `docs/` is the canonical source; never edit the generated pages under `website/src/pages/`.

1. Create the page in `docs/` with a `# Title` first line.
2. Add a `PAGES` entry (`source`, `dest`, `title`, `description`) in `website/scripts/sync-docs.mjs`. Relative links to any synced page are rewritten to its website URL; other `.md` links point at GitHub.
3. Add a nav entry in `website/src/data/docsNav.ts`. The build fails if a synced page is missing from the nav.

When you remove or rename a page, add a redirect in `website/astro.config.mjs`. Hand-written `*.astro` pages are edited directly.

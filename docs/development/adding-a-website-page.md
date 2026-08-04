# Adding a new page to the website

The public website (agent-of-empires.com) is an Astro static site in `website/`.
`docs/` is the canonical source for all documentation and guide content: edit
docs there, never on the website side.

1. Create the page in `docs/` (with a `# Title` as the first line).
2. Add an entry to the `PAGES` array in `website/scripts/sync-docs.mjs` with `source`, `dest`, `title`, and `description`.
3. Add the page's source path → website URL mapping to `URL_MAP` in the same script.
4. Add a nav entry in `website/src/data/docsNav.ts`.

The CI workflow (`.github/workflows/docs.yml`) triggers on changes to `docs/**`,
`website/**`, and other relevant paths.

Astro component pages (`*.astro`) like `website/src/pages/guides/index.astro` are
not generated; edit them directly.

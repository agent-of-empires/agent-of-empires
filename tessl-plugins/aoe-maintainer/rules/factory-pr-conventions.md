# Factory PR conventions

Apply these when an agent opens or updates a pull request in this repository.
`AGENTS.md` and `CONTRIBUTING.md` at the repository root are the canonical
rules; this file lists what CI rejects.

- Title: `<type>(<scope>): <lowercase subject> (#<n>)`. Allowed types are the
  `types:` list in `.github/workflows/pr-title-check.yml`. Do not start the
  title with `#<n>:`.
- Body: keep every heading of `.github/pull_request_template.md`.
  `.github/workflows/pr-template-check.yml` fails the PR, then closes it after
  24 hours, if `## Checklist`, `## Test Coverage Analysis`, or `## AI Usage` is
  missing.
- In `## AI Usage`, tick "AI was used" and "I am an AI Agent filling out this
  form", and name the model.
- Keep a `Closes #<n>` line for each issue the PR fixes.
- No em dashes, and no `--` as a prose separator.
- Run the pre-review checks in the "Pull requests" section of `AGENTS.md`.
- Never create or change `.github/**`, `.tessl/**`, `tessl.json`,
  `.tessl-code-review.yml`, `tessl-plugins/**` or `CODEOWNERS`. If the task
  needs one of them, stop and ask a maintainer in a comment.

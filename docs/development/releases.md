# Releases

Maintainer reference for how `agent-of-empires` ships: stable releases cut from a release PR, plus a daily nightly channel.

## Stable releases

`.github/workflows/release-plz.yml` runs [release-plz](https://release-plz.dev) on every push to `main`. It keeps one release PR open, branch `release-plz-*` and label `release`, that bumps `Cargo.toml` and `Cargo.lock` and prepends the new section to `CHANGELOG.md`. Merge it whenever you want to ship. On that merge release-plz pushes the `vX.Y.Z` tag with `RELEASE_TOKEN`, and the tag push starts `release.yml`. That workflow waits for `release` environment approval, builds the platform binaries through `build-binaries.yml`, publishes the GitHub release with the version's `CHANGELOG.md` section as its body, and publishes the ClawHub skill. `docker.yml` pushes the sandbox images for the same tag.

Configuration lives in `release-plz.toml`: only `agent-of-empires` is released, versions come from `v*` tags, and nothing is published to crates.io. The changelog groups commits by conventional-commit type; `pr-title-check.yml` keeps PR titles, which become squash-commit subjects, within those types.

release-plz force-pushes the release PR on every update, so edits made on its branch are lost. To ship a different version, merge a normal PR that sets the version in `Cargo.toml` and `Cargo.lock`; release-plz keeps a version that is already ahead of the last tag.

## Rebuilding an existing tag

Dispatch the release workflow with the existing tag, subject to the same release-environment approval:

```bash
gh workflow run release.yml -f tag=v1.18.0
```

The build checks out the requested tag and reads `[toolchain].channel` from its `rust-toolchain.toml`. Numeric pins are preserved. Historical tags using `stable` install the current stable compiler through the external setup action, without requiring helper files in the old tree. Such rebuilds do not reproduce the original compiler version.

## Versioning

Semver, from conventional commits since the last `v*` tag: `feat` is minor, anything else is patch, and `feat!:` or a `BREAKING CHANGE:` footer is major. Bump minor for user-visible features without a `feat` prefix, new CLI subcommands, or new config sections. Bump major for breaking config changes, removed CLI subcommands, or on-disk breakage needing maintainer attention beyond a migration. Override the computed bump as described in [Stable releases](#stable-releases). When uncertain, take the bigger bump.

## Nightly channel

`.github/workflows/nightly.yml` runs daily, and on `workflow_dispatch`. It skips when `main` has not moved since the last nightly. Otherwise it builds `main` through `build-binaries.yml` with the version stamped as `X.Y.(Z+1)-nightly.YYYYMMDD`, where `X.Y.Z` is the version in `Cargo.toml`, publishes a GitHub prerelease tagged `nightly-YYYYMMDD`, pushes the sandbox images as `:nightly`, and deletes all but the newest `NIGHTLY_KEEP` nightlies with their tags. It does not need `release` environment approval and never publishes to ClawHub.

The tag stays outside `v*` so `release.yml`, `docker.yml`'s tag trigger and release-plz ignore it, and the version sorts above the stable release it builds on. The update check follows the running binary's channel: stable builds only see stable releases and nightly builds only see nightlies. A nightly release's version is read from its name, `Agent of Empires vX.Y.Z-nightly.YYYYMMDD`, because its tag only carries the date.

Once a migration is merged to `main`, its number and behavior are frozen: nightly users have already run it.

## Skill hubs

`aoe` ships a coding-agent management skill to two hubs from `contrib/`, and `cargo xtask check-skill` (run in CI) validates that both reference real CLI commands and follow each hub's frontmatter rules.

- **ClawHub** (`contrib/openclaw-skill/`) is published automatically by `release.yml` on every release. Its frontmatter must carry no top-level `version:`; ClawHub's `_meta.json` and the `--version` flag are the source of truth.
- **Hermes** (`contrib/hermes-skill/`) is PR-based and published by hand, so the in-repo copy is the source of truth and the hub copy drifts until you re-publish. Its frontmatter must carry a `version:`. Bump it, then run `hermes skills publish contrib/hermes-skill --to github --repo NousResearch/hermes-agent` (needs the `hermes` CLI and a GitHub token with fork permissions). Cadence is on change, not every release.

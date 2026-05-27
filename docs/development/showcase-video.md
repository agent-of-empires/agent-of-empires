# Showcase video generation

A two-step pipeline that turns the existing Playwright suites into a single highlight clip:

1. **Record** every spec under a special Playwright config that always captures video.
2. **Combine** all the recorded `video.webm` files into one mp4 (concat or paginated grid, with optional title overlays).

Both steps reuse the existing mocked and live test suites; no showcase-specific spec is required. New tests added to the suite show up in the next showcase render automatically.

## When to use this

- Cutting a demo clip for a release post, a PR, or a Slack share.
- Spot-checking a refactor visually across the whole UI surface in one pass.
- Stress-running both mocked and live suites under deterministic single-worker conditions (failures during showcase are real test failures; the videos are emitted regardless).

It is **not** a replacement for normal CI. The showcase config disables parallelism and adds per-action slow-motion, so a full run is multiple times slower than the regular suites.

## Step 1: record the videos

### Config

`web/playwright.showcase.config.ts` defines a single Playwright project pair (mocked + live) under `tests/` and `tests/live/`. It extends nothing implicitly: the file is self-contained so it does not drift if `playwright.live.config.ts` changes shape later.

Key knobs (all in the file):

| Setting | Value | Why |
|---|---|---|
| `fullyParallel` | `false` | Recordings must be deterministic; parallel workers would also fight the encoder for CPU. |
| `workers` | `1` | Same reason. |
| `retries` | `0` | A retried test would produce a second video and pollute the combined output. |
| `timeout` | `180_000` | `slowMo` + video encoding stretches per-test wall clock well past the live default of 60s. |
| `use.video` | `{ mode: "on", size: { width: 1440, height: 900 } }` | Every test gets a full-viewport recording, pass or fail. |
| `use.trace` | `"on"` | Trace Viewer companion alongside the video; useful when a showcase run also surfaces a failure. |
| `use.launchOptions.slowMo` | `250` | Visible pacing between Playwright actions. The combine step speeds the result up. |
| `webServer` | `vite preview` on 4173 | Required for the `mocked` project. The `live` project ignores it. |
| `globalSetup` | live's `liveGlobalSetup.ts` | No-op when the `aoe` binary is already on disk; otherwise builds release. |

### Run

```bash
# Use the debug binary so the live specs don't touch the release namespace.
cargo build --features serve
cd web
npm run build

export AOE_E2E_BINARY="$(pwd)/../target/debug/aoe"
npm run test:showcase
```

`test:showcase` is wired in `web/package.json` and just runs `playwright test --config=playwright.showcase.config.ts`.

Filter to a subset:

```bash
# One project.
npx playwright test --config=playwright.showcase.config.ts --project=mocked
npx playwright test --config=playwright.showcase.config.ts --project=live

# One spec.
npx playwright test --config=playwright.showcase.config.ts tests/live/golden-path.spec.ts
```

Recordings land under `web/test-results/<spec-name>/video.webm`. The HTML report at `web/playwright-showcase-report/` embeds each video. A failed test still produces a usable `video.webm`, which is by design: showcase runs prioritize footage over passing CI.

## Step 2: combine into one clip

`web/scripts/combine-showcase-videos.sh` walks `web/test-results/`, finds every `video.webm`, and produces a single mp4 with a configurable layout, speed, and quality.

### Quick reference

```bash
cd web

# Simplest case: concat all videos, cap at 2 minutes.
./scripts/combine-showcase-videos.sh

# Custom output path.
./scripts/combine-showcase-videos.sh ~/Desktop/aoe-showcase.mp4

# 2x2 streaming-lane grid with per-cell titles.
GRID=2x2 SHOW_TITLES=1 ./scripts/combine-showcase-videos.sh

# Higher quality (lower CRF), slower encode.
CRF=12 PRESET=veryslow GRID=2x2 SHOW_TITLES=1 ./scripts/combine-showcase-videos.sh

# Different cap, different resolution.
MAX_SECONDS=60 TARGET_W=1440 TARGET_H=900 ./scripts/combine-showcase-videos.sh
```

### How the layouts work

**Concat (default; no `GRID` set).** Videos play back-to-back in filesystem-sorted order. The total duration of the run is the sum of input durations; the script speeds playback by `factor = max(1, total / MAX_SECONDS)` so the output is always at most `MAX_SECONDS` long.

**Grid (`GRID=COLSxROWS`, e.g. `2x2`).** Cells are arranged in a `COLS by ROWS` grid. Within the grid, each cell is an **independent streaming lane**: when a lane's current video finishes, the next one starts immediately. Cells do not wait on each other.

Distribution across lanes is round-robin: with 9 videos in a 2x2 grid, lane 0 plays inputs 0/4/8, lane 1 plays 1/5, lane 2 plays 2/6, lane 3 plays 3/7. Round-robin keeps lane lengths balanced so no lane runs out long before the others, which avoids a static dead panel at the end of the clip.

A lane that finishes before the longest lane freezes on its last frame until xstack runs out of input. Speed factor in grid mode is `max(1, longest_lane / MAX_SECONDS)`.

### Titles (`SHOW_TITLES=1`)

When set, a translucent bottom-left text box is overlaid on each cell (grid mode) or each segment (concat mode). Title text is derived from the spec's `test-results` directory name:

```
test-results/tests-live-golden-path-create-view-delete.../video.webm
                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                  -> "golden path: create view delete..."
```

The `tests-` and `live-` prefixes are stripped, dashes are turned into spaces, and the result is capped at 80 characters.

Drawtext requires an ffmpeg build that links libfreetype. The default Homebrew ffmpeg 8.x does not include it; install the `homebrew-ffmpeg/ffmpeg` tap version instead:

```bash
brew uninstall ffmpeg
brew tap homebrew-ffmpeg/ffmpeg
brew install homebrew-ffmpeg/ffmpeg/ffmpeg
ffmpeg -filters 2>/dev/null | grep drawtext  # must list it
```

Without that build, run without `SHOW_TITLES=1`.

### All environment variables

| Var | Default | Effect |
|---|---|---|
| `MAX_SECONDS` | `120` | Hard cap on output duration. Speed factor scales to fit. |
| `CRF` | `14` | libx264 constant rate factor. 0 = lossless, 14 ~= near-lossless, 18 ~= visually lossless, 23 = libx264 default, 28+ = visibly lossy. Lower is better but file size grows fast. |
| `PRESET` | `slow` | libx264 preset. `veryslow` = best compression at same quality, `medium`/`fast` cheaper. |
| `GRID` | unset | `COLSxROWS` enables grid mode (e.g. `2x2`, `3x3`, `4x2`). Must total >= 2 cells. |
| `SHOW_TITLES` | `0` | `1` overlays the spec name on each cell/segment. Requires drawtext. |
| `TARGET_W` | `1440` | Output canvas width. Matches Playwright viewport. |
| `TARGET_H` | `900` | Output canvas height. Matches Playwright viewport. |
| `FONT` | auto-detect | Path to a TTF/TTC. Tries common macOS and Linux fonts if unset. |

The script auto-rounds cell width/height to even numbers (libx264 requirement).

### Quality vs file size

The defaults (`CRF=14`, `PRESET=slow`) are tuned for screen-recording content: lots of crisp text, sharp UI edges, low motion. Typical output for a 2-minute clip at 1440x900 grid: ~50-150 MB.

| CRF | Look | Size (rough, 2-min 1440x900 4-panel) |
|---|---|---|
| 0 | Mathematically lossless | 500 MB - 2 GB |
| 12 | Near-lossless | ~150-300 MB |
| 14 (default) | Visually lossless on text/UI | ~50-150 MB |
| 18 | Visually lossless on natural video; can soften text | ~25-60 MB |
| 23 | libx264 default; text edges visibly softer | ~10-25 MB |

If a future showcase clip needs uploading to a tool with a size cap (Slack 1 GB, GitHub upload 25 MB), use `CRF=20-23` with `PRESET=veryslow`. The slower preset reclaims a lot of quality at higher CRF.

### Performance notes

- Concat mode without titles uses the cheap path (concat demuxer + a single `setpts` re-encode pass). One ffmpeg pass, modest CPU.
- Concat mode with titles, and any grid mode, build a `filter_complex` graph with one input per video. With 200+ recordings this graph is large; ffmpeg parses it fine but encode time scales with input count.
- On macOS, `ARG_MAX` is ~262 kB. With several hundred inputs and absolute paths, the command line can approach that. If you hit it, render in batches and concat the batch outputs as a second pass.
- `slowMo: 250` in the Playwright config is the main multiplier on step-1 wall clock. Dropping to `slowMo: 100` (or removing it) speeds the recording stage substantially, at the cost of some footage looking jumpy on fast UI transitions.

## Common workflows

### Just give me the demo clip

```bash
cd web
AOE_E2E_BINARY="$(pwd)/../target/debug/aoe" npm run test:showcase
SHOW_TITLES=1 ./scripts/combine-showcase-videos.sh
open test-results/showcase-combined.mp4
```

### Re-render combine without re-running tests

The recordings persist in `test-results/` until Playwright cleans them up (it doesn't clean by default). Iterate on the combine command without re-recording:

```bash
GRID=2x2 SHOW_TITLES=1 ./scripts/combine-showcase-videos.sh
GRID=3x3 SHOW_TITLES=1 ./scripts/combine-showcase-videos.sh
CRF=12 PRESET=veryslow GRID=2x2 ./scripts/combine-showcase-videos.sh ~/Desktop/aoe-highlight.mp4
```

### Reset between runs

```bash
rm -rf web/test-results web/playwright-showcase-report
```

Playwright will recreate both on the next showcase run.

## Files

- `web/playwright.showcase.config.ts` -- config used by step 1.
- `web/scripts/combine-showcase-videos.sh` -- combiner used by step 2.
- `web/package.json` -- `test:showcase` script.
- `web/test-results/` -- per-test recordings (gitignored).
- `web/playwright-showcase-report/` -- HTML report (gitignored).

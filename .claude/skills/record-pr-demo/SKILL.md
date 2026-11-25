---
name: record-pr-demo
description: Record a demo video of a PR's change by driving real Chromium against a real `aoe serve` with real tmux sessions, then publish MP4 + inline GIF to the PR. Use when asked for a demo, video, GIF or recording of a PR or branch, or to show a dashboard change working.
---

# Record a PR demo

Each clip is evidence a reviewer can watch: a real daemon, real sessions, real browser input, no mocked API. If a behavior cannot be shown for real, say so rather than staging it; where one input truly cannot be produced (a dropped touch end), simulate only that input and say so in the PR comment.

Files beside this one:

- `demo-lib.mjs`: Playwright helpers (`launch`, `finish`, `caption`, `ring`, `glide`, `prep`) with a visible pointer overlay.
- `convert.sh`: webm to MP4 + GIF + a contact sheet of frames.
- `publish.sh`: push clips to the `e2e-recordings` branch and print their URLs.
- [`TECHNIQUES.md`](TECHNIQUES.md): read when a clip needs touch input, a native `<select>`, link hover/click proof, a WebSocket counter, or a simulated lost event.

## 1. Build

Build the branch that contains every change you will show (the top of a stack covers all PRs in it):

```sh
cargo build --features web
```

Debug builds use their own app dir (`agent-of-empires-dev`), tmux socket prefix and port 8081, so they never touch the user's real aoe. Run the build alone; Rust and Playwright together can exhaust a memory-capped sandbox.

## 2. Stand up an isolated daemon

```sh
export DEMO=/tmp/aoedemo
export XDG_CONFIG_HOME=$DEMO/xdg AOE_TMUX_SOCKET=$DEMO/tmux.sock
export CLAUDE_CONFIG_DIR=$DEMO/claude-config TERM=xterm-256color LANG=C.UTF-8
export PATH=$PWD/target/debug:$PATH
mkdir -p $XDG_CONFIG_HOME/agent-of-empires-dev $CLAUDE_CONFIG_DIR
printf '[session.custom_agents]\nshell = "bash"\n' > $XDG_CONFIG_HOME/agent-of-empires-dev/config.toml
ln -sf ~/.claude/.credentials.json $CLAUDE_CONFIG_DIR/.credentials.json
echo '{"hasCompletedOnboarding":true,"theme":"dark"}' > $CLAUDE_CONFIG_DIR/.claude.json
nohup aoe serve > $DEMO/serve.log 2>&1 &
aoe hooks approve   # isolated install only; the real ~/.claude is untouched
```

`XDG_CONFIG_HOME` isolates the app dir on Linux; on macOS check `aoe serve` output for the app dir it used. The browser authenticates with `?token=` from `serve.token` in that app dir.

Done when `aoe list` answers and `curl -s localhost:8081` returns the dashboard.

## 3. Build the scene

Make small git repos under `$DEMO/projects/` so sessions fall into real groups, and add a fixture script per behavior (for example `links.sh` printing a long URL and a long OSC 8 label). Create sessions through aoe:

```sh
aoe add -t shell-urls -g web-shop --tool shell -l .
aoe add -t claude-agent -g web-shop --tool claude -l --prompt '<one tiny prompt>' .
```

Use `--tool shell` for anything scripted; use one real agent session where the PR touches agent views, and keep its prompt tiny (it may ignore `--model`). Done when every session the clips need shows in `aoe list` and its tmux pane holds the state you will film.

## 4. Record

Write one script per clip in a scratch dir, importing `demo-lib.mjs`. Each clip is 10 to 40 seconds and shows one PR's behavior end to end:

- Pace for a viewer: `glide` the pointer, pause about 1 s between steps, and `caption` what is happening.
- Use real input (`page.mouse`, `keyboard`, CDP touch). Use `prep()` for setup writes through the dashboard's own API before recording starts.
- Show only the final behavior unless the user asks for before/after.

Run with `AOE_DEMO_TOKEN_FILE` (or `AOE_DEMO_TOKEN`) and `AOE_DEMO_RAW=<scratch>/raw` set. On arm64 Linux add `PLAYWRIGHT_HOST_PLATFORM_OVERRIDE=ubuntu24.04-arm64 PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS=1`.

## 5. Convert and check

```sh
.claude/skills/record-pr-demo/convert.sh <raw>/<clip>.webm <out-dir>
```

Read every contact sheet it prints. Done when each clip visibly shows its behavior at the moments the captions claim, the GIF is at most 8 MB, and no overlay hides the thing being shown. Re-record any clip that fails.

## 6. Publish

Posting is outward-facing: confirm with the user before this step.

```sh
.claude/skills/record-pr-demo/publish.sh <out-dir> <subdir-name>
```

It commits the files onto the `e2e-recordings` branch (the same branch `e2e-recording.yml` uses) without touching any worktree, and prints raw GIF and MP4 page URLs. GitHub has no API for PR video attachments; a raw GIF URL renders inline in a comment. Post one comment per PR:

```md
**Demo** (real Chromium, debug `aoe serve` at <sha>, real tmux sessions)

<one or two sentences on what the clip shows, plus any simulated input or overlay a reviewer should know about>

![<clip>](<raw gif url>)

[MP4](<mp4 blob url>)
```

## 7. Clean up

`aoe serve --stop`, `tmux -S $AOE_TMUX_SOCKET kill-server`, and remove `$DEMO` once the user is happy with the clips.

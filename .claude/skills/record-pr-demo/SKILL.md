---
name: record-pr-demo
description: Record a demo video of a PR's change by driving real Chromium against a real `aoe serve` with real tmux sessions, then attach the MP4 to a PR comment, where it plays inline. Use when asked for a demo, video, GIF or recording of a PR or branch, or to show a dashboard change working.
---

# Record a PR demo

Each clip is evidence a reviewer can watch: a real daemon, real sessions, real browser input, no mocked API. If a behavior cannot be shown for real, say so rather than staging it; where one input truly cannot be produced (a dropped touch end), simulate only that input and say so in the PR comment.

Files beside this one:

- `demo-lib.mjs`: Playwright helpers (`launch`, `finish`, `caption`, `ring`, `glide`, `prep`) with a visible pointer overlay.
- `convert.sh`: webm to MP4 plus a contact sheet of frames.
- [`TECHNIQUES.md`](TECHNIQUES.md): read when a clip needs touch input, a native `<select>`, link hover/click proof, a WebSocket counter, a simulated lost event, or the native TUI.

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
aoe add -t claude-agent -g web-shop --tool claude -l --structured-view --prompt '<one tiny prompt>' .
```

Use `--tool shell` for anything scripted; use one real agent session where the PR touches agent views, and keep its prompt tiny (it may ignore `--model`). Done when every session the clips need shows in `aoe list`. Terminal sessions must have a tmux pane holding the state you will film. Structured sessions (`--structured-view`) have no agent pane, so check their `aoe list` status (Idle or Active) or the transcript.

## 4. Record

Write one script per clip in a scratch dir, importing `demo-lib.mjs`. Each clip is 10 to 40 seconds and shows one PR's behavior end to end:

- Pace for a viewer: `glide` the pointer, pause about 1 s between steps, and `caption` what is happening.
- Use real input (`page.mouse`, `keyboard`, CDP touch). Use `prep()` for setup writes through the dashboard's own API before recording starts.
- Show only the final behavior unless the user asks for before/after.

Run with `AOE_DEMO_TOKEN_FILE` (or `AOE_DEMO_TOKEN`) and `AOE_DEMO_RAW=<scratch>/raw` set. On arm64 Linux add `PLAYWRIGHT_HOST_PLATFORM_OVERRIDE=ubuntu24.04-arm64 PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS=1`.

### TUI clips

Run the dev `aoe` in ttyd and drive it with Playwright, as `web/scripts/record-tui-demo.mjs` does (read it for the ttyd and `aoe` setup). Here the script uses `demo-lib.mjs` (`launch`, `caption`) and `convert.sh`, so the clip matches the web ones. Use the step 2 environment and start ttyd with `--cwd` set to a scene repo (no `cd`, so later steps keep their repo-root paths). ttyd runs a writable shell with your env, so it takes a password:

```sh
export TTYD_PW=$(openssl rand -hex 12)
nohup ttyd --port 7683 --interface 127.0.0.1 --cwd "$DEMO/projects/<scene>" -c demo:$TTYD_PW -t rendererType=dom -t fontSize=15 --writable aoe > "$DEMO/ttyd.log" 2>&1 &
echo $! > "$DEMO/ttyd.pid"
```

In the clip script: `const h = await launch("<clip>")` (the name also goes to `finish(h, "<clip>")` at the end), `await h.context.setHTTPCredentials({ username: "demo", password: process.env.TTYD_PW })`, `h.page.goto("http://127.0.0.1:7683")`, click `.xterm`, then `h.page.keyboard.press`. Set `AOE_DEMO_TOKEN=-` (no dashboard token). Assert on `document.body.innerText` before each key. Traps are in [`TECHNIQUES.md`](TECHNIQUES.md#tui-clips-ttyd). For scripted e2e tests, `RECORD_E2E=1` records asciinema casts that `agg` converts; both must be on `PATH` (`tests/e2e/harness.rs`).

Done when the contact sheet shows each key's effect at the moment its caption claims.

## 5. Convert and check

```sh
.claude/skills/record-pr-demo/convert.sh <raw>/<clip>.webm <out-dir>
```

Read every contact sheet it prints. Done when each clip visibly shows its behavior at the moments the captions claim, the MP4 is at most 10 MB (the attachment limit), and no overlay hides the thing being shown. Re-record any clip that fails.

## 6. Publish

Posting is outward-facing: confirm with the user before this step.

Write the comment to `<out-dir>/body.md`, referencing each clip by relative path, and post it with one `--attach` per clip (gh 2.103 or later):

```md
**Demo** (real Chromium, debug `aoe serve` at <sha>, real tmux sessions)

<one or two sentences on what the clip shows, plus any simulated input or overlay a reviewer should know about>

![<clip>](./<clip>.mp4)
```

```sh
gh pr comment <N> --body-file <out-dir>/body.md --attach <out-dir>/<clip>.mp4
```

`gh` uploads each file to GitHub's attachment storage and rewrites its reference to the uploaded URL, which renders as a video player. A video reference must be alone in its paragraph. `gh pr create` and `gh pr edit` take `--attach` the same way. Never commit clips to a branch of the repo.

## 7. Clean up

`aoe serve --stop`, `kill $(cat $DEMO/ttyd.pid)` for a TUI clip, `tmux -S $AOE_TMUX_SOCKET kill-server`, and remove `$DEMO` once the user is happy with the clips.

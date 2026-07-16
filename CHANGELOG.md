# Changelog

All notable changes to Agent of Empires will be documented in this file.

The format follows [Conventional Commits](https://www.conventionalcommits.org/).

## [1.13.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.13.0) - 2026-07-16



### Bug Fixes

- **containers:** Promote does_container_exist to caller-driven tri-state (closes #2654) in [#2719](https://github.com/agent-of-empires/agent-of-empires/pull/2719) by [@jerome-benoit](https://github.com/jerome-benoit) ([`21ad248`](https://github.com/agent-of-empires/agent-of-empires/commit/21ad2481ee470c36a4c622c66894bf1d8d6735a9))
- **session:** Eliminate spurious restamp seed and IdleField ambiguity (#2697 review) in [#2729](https://github.com/agent-of-empires/agent-of-empires/pull/2729) by [@jerome-benoit](https://github.com/jerome-benoit) ([`334f50c`](https://github.com/agent-of-empires/agent-of-empires/commit/334f50c20592403fad2bde68aed60d5cd0608328))
- **acp:** Persist model pick so respawn keeps it in [#2771](https://github.com/agent-of-empires/agent-of-empires/pull/2771) by [@Seluj78](https://github.com/Seluj78) ([`441b283`](https://github.com/agent-of-empires/agent-of-empires/commit/441b2832c29517636ab37db9af9420001f1cc191))
- **acp:** Latch context-window size so the composer stops flickering 200k to 1M in [#2774](https://github.com/agent-of-empires/agent-of-empires/pull/2774) by [@Seluj78](https://github.com/Seluj78) ([`627ee92`](https://github.com/agent-of-empires/agent-of-empires/commit/627ee9290432f7ebf0185ec8647685933fbe40db))
- **acp:** Send client_info in initialize request to satisfy strict agent backends in [#2770](https://github.com/agent-of-empires/agent-of-empires/pull/2770) by [@Seluj78](https://github.com/Seluj78) ([`c3328cd`](https://github.com/agent-of-empires/agent-of-empires/commit/c3328cd95a7af47e2dacec6cf05f85c728350d37))
- **plugin:** Recover and observe stopped plugin workers without a daemon restart in [#2773](https://github.com/agent-of-empires/agent-of-empires/pull/2773) by [@Seluj78](https://github.com/Seluj78) ([`119570f`](https://github.com/agent-of-empires/agent-of-empires/commit/119570f7c6694a513eff3a135f2ea4036356d390))
- **serve:** Stop the live-session pane re-assert loop that flickers the cursor (#2766) in [#2772](https://github.com/agent-of-empires/agent-of-empires/pull/2772) by [@Seluj78](https://github.com/Seluj78) ([`188b09d`](https://github.com/agent-of-empires/agent-of-empires/commit/188b09d97e9cf93d502d583c1c93672d9a0cba7f))
- **tui:** Make w skip snoozed sessions when jumping to next attention in [#2750](https://github.com/agent-of-empires/agent-of-empires/pull/2750) by [@athal7](https://github.com/athal7) ([`6267148`](https://github.com/agent-of-empires/agent-of-empires/commit/6267148cfe43f713561239e84e727fb4b344faef))
- **web:** Align live terminal enter chords in [#2765](https://github.com/agent-of-empires/agent-of-empires/pull/2765) by [@SYU8384](https://github.com/SYU8384) ([`24129a5`](https://github.com/agent-of-empires/agent-of-empires/commit/24129a5ae4c76f5182601d2c472c74b9b6484c7f))
- **config:** Follow symlinks when saving the global config in [#2785](https://github.com/agent-of-empires/agent-of-empires/pull/2785) by [@Seluj78](https://github.com/Seluj78) ([`66a0de9`](https://github.com/agent-of-empires/agent-of-empires/commit/66a0de9402966e337274770f3a26393a9b83b976))
- **tui:** Render structured tool paths repo-relative in [#2797](https://github.com/agent-of-empires/agent-of-empires/pull/2797) by [@Seluj78](https://github.com/Seluj78) ([`f7640fc`](https://github.com/agent-of-empires/agent-of-empires/commit/f7640fc12c07344c2839a8e7f01f87180918fd7e))
- **acp:** Warn about stale adapters on serve startup in [#2799](https://github.com/agent-of-empires/agent-of-empires/pull/2799) by [@Seluj78](https://github.com/Seluj78) ([`06276b7`](https://github.com/agent-of-empires/agent-of-empires/commit/06276b7cea9288b24fae2e1d551b06cb73f190fa))
- **acp:** Anchor tool-group card id to first child so it stops re-folding in [#2807](https://github.com/agent-of-empires/agent-of-empires/pull/2807) by [@Seluj78](https://github.com/Seluj78) ([`f5320a9`](https://github.com/agent-of-empires/agent-of-empires/commit/f5320a9cad1107983ca5a8d8829e7c87ea73c949))
- **acp:** Honor structured-view acp_defaults (model + effort) on aoe add and reconciler respawns in [#2809](https://github.com/agent-of-empires/agent-of-empires/pull/2809) by [@Seluj78](https://github.com/Seluj78) ([`dfa1f4a`](https://github.com/agent-of-empires/agent-of-empires/commit/dfa1f4a87ec2a90d502d5ac13195cf3431a7518d))
- **web:** Gray out current agent in switch-agent modal in [#2810](https://github.com/agent-of-empires/agent-of-empires/pull/2810) by [@Seluj78](https://github.com/Seluj78) ([`9e204c7`](https://github.com/agent-of-empires/agent-of-empires/commit/9e204c7fa440ef3e2c826f5a3d126dd243fbdf4b))
- **session:** Launch empty Claude threads fresh-pinned instead of a doomed --resume in [#2700](https://github.com/agent-of-empires/agent-of-empires/pull/2700) by [@Eric162](https://github.com/Eric162) ([`b882260`](https://github.com/agent-of-empires/agent-of-empires/commit/b882260284cd363d1e9694540b5925cb80eb33cd))
- **acp:** Treat CapacityFull as a first-class transient (refund budget, retry, no orphan) (#1027) in [#2782](https://github.com/agent-of-empires/agent-of-empires/pull/2782) by [@jerome-benoit](https://github.com/jerome-benoit) ([`7580fe3`](https://github.com/agent-of-empires/agent-of-empires/commit/7580fe399936657a3f798dd4c755adde8a070415))
- **test:** Stop home-isolation helpers leaking XDG config in [#2786](https://github.com/agent-of-empires/agent-of-empires/pull/2786) by [@athal7](https://github.com/athal7) ([`2a7865d`](https://github.com/agent-of-empires/agent-of-empires/commit/2a7865d5bb0e662729d83b92d19be06b49ab9a38))
- **tui:** Stop Copilot session spinner spinning after the turn ends in [#2816](https://github.com/agent-of-empires/agent-of-empires/pull/2816) by [@Seluj78](https://github.com/Seluj78) ([`855a009`](https://github.com/agent-of-empires/agent-of-empires/commit/855a009b2a75a1b903e6703c2d0e60d9c5fa59f9))
- **web:** End the first-run tour on dismiss instead of stranding the scrim in [#2820](https://github.com/agent-of-empires/agent-of-empires/pull/2820) by [@Seluj78](https://github.com/Seluj78) ([`e8f55ef`](https://github.com/agent-of-empires/agent-of-empires/commit/e8f55ef974bdcaa276cdc9411bd011d6c6ef6a7c))
- Retain bounded live terminal scrollback in [#2813](https://github.com/agent-of-empires/agent-of-empires/pull/2813) by [@SYU8384](https://github.com/SYU8384) ([`51b3566`](https://github.com/agent-of-empires/agent-of-empires/commit/51b35669c13ede3daec84a2269b85fc925135e55))
- **session:** Durable cross-process purge claim to close the purge/restore race (#2541) in [#2780](https://github.com/agent-of-empires/agent-of-empires/pull/2780) by [@jerome-benoit](https://github.com/jerome-benoit) ([`1c8d94f`](https://github.com/agent-of-empires/agent-of-empires/commit/1c8d94f2b773506a60cdf4881d8bf2241a76bf29))
- **web:** Prevent live terminal bottom-scroll blackout in [#2776](https://github.com/agent-of-empires/agent-of-empires/pull/2776) by [@SYU8384](https://github.com/SYU8384) ([`03d1560`](https://github.com/agent-of-empires/agent-of-empires/commit/03d1560932767390db55e0ef62bbea83be53781f))
- **web:** Insert soft newline on Shift+Enter in live terminal in [#2823](https://github.com/agent-of-empires/agent-of-empires/pull/2823) by [@njbrake](https://github.com/njbrake) ([`996f52c`](https://github.com/agent-of-empires/agent-of-empires/commit/996f52cde4fd0aa8aa0e053d62671c1e6af2f1a7))
- **tui:** Skip pre-draw cursor hide during live-send to stop flicker in [#2825](https://github.com/agent-of-empires/agent-of-empires/pull/2825) by [@athal7](https://github.com/athal7) ([`38547b0`](https://github.com/agent-of-empires/agent-of-empires/commit/38547b076bf653e4de1e99ba6792b4187d5713e9))
- **tui:** Keep the live preview from desyncing by a row in [#2830](https://github.com/agent-of-empires/agent-of-empires/pull/2830) by [@njbrake](https://github.com/njbrake) ([`5826e0f`](https://github.com/agent-of-empires/agent-of-empires/commit/5826e0fd33fbecdf16c54e1f0ee0c67bdbe83397))
- **web:** Present multi-session deletes as workspace actions in [#2827](https://github.com/agent-of-empires/agent-of-empires/pull/2827) by [@Seluj78](https://github.com/Seluj78) ([`9796de0`](https://github.com/agent-of-empires/agent-of-empires/commit/9796de059d125f9c323b7591444956a000d524d0))
- **web:** Preserve right dock tabs on collapse in [#2829](https://github.com/agent-of-empires/agent-of-empires/pull/2829) by [@Seluj78](https://github.com/Seluj78) ([`3f0ee18`](https://github.com/agent-of-empires/agent-of-empires/commit/3f0ee180371796cc47e96ecdd4195fc687f73662))
- **git:** Make cleanup running-state probe warn runtime-neutral in [#2838](https://github.com/agent-of-empires/agent-of-empires/pull/2838) by [@njbrake](https://github.com/njbrake) ([`f937959`](https://github.com/agent-of-empires/agent-of-empires/commit/f937959d45a0fcc069217726fbaa64052429afad))
- **server:** Defer passive-status unread mark until persist Ok in [#2850](https://github.com/agent-of-empires/agent-of-empires/pull/2850) by [@njbrake](https://github.com/njbrake) ([`7f6914d`](https://github.com/agent-of-empires/agent-of-empires/commit/7f6914dee02b6757e265841def8fa985895fe1d0))
- **containers:** Harden RuntimeBase classifier plumbing on edge cases in [#2845](https://github.com/agent-of-empires/agent-of-empires/pull/2845) by [@njbrake](https://github.com/njbrake) ([`02e569b`](https://github.com/agent-of-empires/agent-of-empires/commit/02e569b37edb8917889fb5780f832fcb60d46ea0))
- Disable trust-folder prompt for YOLO-mode sandboxed sessions in [#2840](https://github.com/agent-of-empires/agent-of-empires/pull/2840) by [@njbrake](https://github.com/njbrake) ([`b009fe4`](https://github.com/agent-of-empires/agent-of-empires/commit/b009fe4e4d948b5682ab02bbfbc036b1d4e5628b))
- **tui:** Honor repo config sandbox.default_image in new-session dialog in [#2833](https://github.com/agent-of-empires/agent-of-empires/pull/2833) by [@njbrake](https://github.com/njbrake) ([`76b87ff`](https://github.com/agent-of-empires/agent-of-empires/commit/76b87ffc8f841c2c0b881e11165d381de780b6e3))
- **config:** Read-modify-write config.toml and split app_state into state.toml in [#2821](https://github.com/agent-of-empires/agent-of-empires/pull/2821) by [@athal7](https://github.com/athal7) ([`4a3bdb2`](https://github.com/agent-of-empires/agent-of-empires/commit/4a3bdb2872d77406b4bdace6c0b468fbd7124fc0))
- **config:** Repair rustdoc private intra-doc link on state.toml helper in [#2868](https://github.com/agent-of-empires/agent-of-empires/pull/2868) by [@njbrake](https://github.com/njbrake) ([`e65c03c`](https://github.com/agent-of-empires/agent-of-empires/commit/e65c03cdb44a14be01ae05fc5cd3b7efdbf69384))
- Stop a dead tool-session pane's Ctrl+C from killing aoe itself in [#2839](https://github.com/agent-of-empires/agent-of-empires/pull/2839) by [@athal7](https://github.com/athal7) ([`16a26ae`](https://github.com/agent-of-empires/agent-of-empires/commit/16a26ae8f5c0108b7fe7a31c98899db826680375))
- **tui:** Show Tab as Enter's attach/live-send complement in the footer in [#2862](https://github.com/agent-of-empires/agent-of-empires/pull/2862) by [@hamid-mouti](https://github.com/hamid-mouti) ([`49c9785`](https://github.com/agent-of-empires/agent-of-empires/commit/49c9785551be01f119527c84b6336bda20159890))
- **server:** GC stale ACP reconciler maps on session delete in [#2836](https://github.com/agent-of-empires/agent-of-empires/pull/2836) by [@njbrake](https://github.com/njbrake) ([`a38b51b`](https://github.com/agent-of-empires/agent-of-empires/commit/a38b51b25eacffd02db157437408884f9e073ed1))
- **sandbox:** Send container workdir on session/new for sandboxed agents in [#2873](https://github.com/agent-of-empires/agent-of-empires/pull/2873) by [@Seluj78](https://github.com/Seluj78) ([`1291042`](https://github.com/agent-of-empires/agent-of-empires/commit/1291042dc0f88466b099b1393397c9610e748de1))
- **web:** Cap context menus with dvh so mobile Delete stays reachable in [#2872](https://github.com/agent-of-empires/agent-of-empires/pull/2872) by [@Seluj78](https://github.com/Seluj78) ([`7bee7b7`](https://github.com/agent-of-empires/agent-of-empires/commit/7bee7b7fe313537649e1e839458c1f97c64a7481))
- **profile:** Stop -p/--profile from lazily creating unknown profiles in [#2874](https://github.com/agent-of-empires/agent-of-empires/pull/2874) by [@athal7](https://github.com/athal7) ([`12acd8f`](https://github.com/agent-of-empires/agent-of-empires/commit/12acd8ff6d229a923a5d5632123bd7ba461b7018))
- **test:** Skip chmod write-failure injection when running as root in [#2876](https://github.com/agent-of-empires/agent-of-empires/pull/2876) by [@njbrake](https://github.com/njbrake) ([`9f6fd9d`](https://github.com/agent-of-empires/agent-of-empires/commit/9f6fd9d26059e4ef758be9ee39ed7aaeabe0e984))
- **test:** Scope git file-transport opt-in per-command, drop global env race in [#2877](https://github.com/agent-of-empires/agent-of-empires/pull/2877) by [@njbrake](https://github.com/njbrake) ([`1615964`](https://github.com/agent-of-empires/agent-of-empires/commit/16159646ee580e0e4a5ca3a02eb402689d33b8c0))
- **test:** Isolate HOME in resume_intent_default_uses_observed in [#2879](https://github.com/agent-of-empires/agent-of-empires/pull/2879) by [@njbrake](https://github.com/njbrake) ([`d951f90`](https://github.com/agent-of-empires/agent-of-empires/commit/d951f902f84071ee8bccaccc110627495074d90e))
- **sandbox:** Trust codex and gemini yolo workdirs in [#2855](https://github.com/agent-of-empires/agent-of-empires/pull/2855) by [@Seluj78](https://github.com/Seluj78) ([`de1955b`](https://github.com/agent-of-empires/agent-of-empires/commit/de1955bf5e9e9a20052c6edf146a323ee489910e))
- **web:** Preserve terminal key combos in [#2854](https://github.com/agent-of-empires/agent-of-empires/pull/2854) by [@Seluj78](https://github.com/Seluj78) ([`38f38bd`](https://github.com/agent-of-empires/agent-of-empires/commit/38f38bd777233c8371473141c5b4c38fb97913b8))
- **test:** Give each test process a unique tmux socket and tear e2e servers down in [#2880](https://github.com/agent-of-empires/agent-of-empires/pull/2880) by [@njbrake](https://github.com/njbrake) ([`c64c1b8`](https://github.com/agent-of-empires/agent-of-empires/commit/c64c1b87ec3103a80013f1dbc023700c006d65da))
- **tmux:** Treat "no server running" as empty, not a warn in [#2884](https://github.com/agent-of-empires/agent-of-empires/pull/2884) by [@Seluj78](https://github.com/Seluj78) ([`7ddeebc`](https://github.com/agent-of-empires/agent-of-empires/commit/7ddeebc7c3e052e779f8961ef754caa6af5aac56))
- **tui:** Fall back to send-keys when VT socket write fails in [#2882](https://github.com/agent-of-empires/agent-of-empires/pull/2882) by [@athal7](https://github.com/athal7) ([`8ec0d35`](https://github.com/agent-of-empires/agent-of-empires/commit/8ec0d35cbdab3c05e9959f9db17fb5a885eb373a))
- **tui:** Drop the pin/unpin confirmation popups in [#2890](https://github.com/agent-of-empires/agent-of-empires/pull/2890) by [@njbrake](https://github.com/njbrake) ([`2dcfd94`](https://github.com/agent-of-empires/agent-of-empires/commit/2dcfd943c176d0a59299c7c4488ae02293ca9869))
- **acp:** Skip lifecycle envelopes when no prompt is in flight in [#2891](https://github.com/agent-of-empires/agent-of-empires/pull/2891) by [@Seluj78](https://github.com/Seluj78) ([`af73ab2`](https://github.com/agent-of-empires/agent-of-empires/commit/af73ab2e0851b0cda57cafe328affb7cf864ff63))


### Features

- Add composer action extension point in [#2585](https://github.com/agent-of-empires/agent-of-empires/pull/2585) by [@amanzainal](https://github.com/amanzainal) ([`8f21510`](https://github.com/agent-of-empires/agent-of-empires/commit/8f215108ffe1e57989e53cce671f4c7f7f6512b3))
- **web:** Explain composer usage/cost indicator with a hover tooltip in [#2806](https://github.com/agent-of-empires/agent-of-empires/pull/2806) by [@Seluj78](https://github.com/Seluj78) ([`02350c2`](https://github.com/agent-of-empires/agent-of-empires/commit/02350c242a94d7207486b0f2ab48ea6679833fa9))
- **serve:** Use full first-turn context for smart rename and add opt-in prompt-start timing in [#2811](https://github.com/agent-of-empires/agent-of-empires/pull/2811) by [@Seluj78](https://github.com/Seluj78) ([`49fdc1f`](https://github.com/agent-of-empires/agent-of-empires/commit/49fdc1f7d59657322fa36338ba30cbe651781ab0))
- **tui:** Auto-enter live-send on view switch, support Tool view in [#2777](https://github.com/agent-of-empires/agent-of-empires/pull/2777) by [@athal7](https://github.com/athal7) ([`8fcfd2c`](https://github.com/agent-of-empires/agent-of-empires/commit/8fcfd2c00b5fc8b98d447c6578fbddab257c3f25))
- Respond to agent permission prompts from the sidebar in [#2764](https://github.com/agent-of-empires/agent-of-empires/pull/2764) by [@athal7](https://github.com/athal7) ([`77dc53f`](https://github.com/agent-of-empires/agent-of-empires/commit/77dc53f430fa209de7985a0ec46dc27a352b538b))
- **acp:** Agent-agnostic conversation summary for structured sessions in [#2814](https://github.com/agent-of-empires/agent-of-empires/pull/2814) by [@Seluj78](https://github.com/Seluj78) ([`b243243`](https://github.com/agent-of-empires/agent-of-empires/commit/b243243ad8490fc9671ce4e3e3f46381637168bd))
- Add sandbox.network config for container network isolation in [#2824](https://github.com/agent-of-empires/agent-of-empires/pull/2824) by [@njbrake](https://github.com/njbrake) ([`d9fa418`](https://github.com/agent-of-empires/agent-of-empires/commit/d9fa41886a0ffbc65c48e12591dfe38c960a4035))
- Per-session color labels settable via CLI and web (#2383) in [#2851](https://github.com/agent-of-empires/agent-of-empires/pull/2851) by [@njbrake](https://github.com/njbrake) ([`8d7b312`](https://github.com/agent-of-empires/agent-of-empires/commit/8d7b31222d0d4c5587b5138a0d606d6d3b139ef4))
- Add tmux socket name setting for session isolation in [#2846](https://github.com/agent-of-empires/agent-of-empires/pull/2846) by [@njbrake](https://github.com/njbrake) ([`d7bb5e8`](https://github.com/agent-of-empires/agent-of-empires/commit/d7bb5e8259e54790be66cb6df8ed3aa9a0501965))
- **web:** Add mobile sidebar-side setting (left/right) in [#2844](https://github.com/agent-of-empires/agent-of-empires/pull/2844) by [@njbrake](https://github.com/njbrake) ([`a2a02c1`](https://github.com/agent-of-empires/agent-of-empires/commit/a2a02c1c3377be54711c7932794f3d3d8d48189e))
- **tui:** Run background tool commands in [#2856](https://github.com/agent-of-empires/agent-of-empires/pull/2856) by [@Seluj78](https://github.com/Seluj78) ([`a8aa025`](https://github.com/agent-of-empires/agent-of-empires/commit/a8aa025a59ca58a3b1b82941201068949722233c))
- **serve:** Add Host/Origin DNS-rebinding gate; --behind-proxy and hostname access to 0.0.0.0 now need --allowed-host in [#2835](https://github.com/agent-of-empires/agent-of-empires/pull/2835) by [@jerome-benoit](https://github.com/jerome-benoit) ([`e9a0f23`](https://github.com/agent-of-empires/agent-of-empires/commit/e9a0f23b2beff265cbb36ca28f3fa0605e078aa5))


### Other

- Remove support for x86_64-darwin in [#2778](https://github.com/agent-of-empires/agent-of-empires/pull/2778) by [@neunenak](https://github.com/neunenak) ([`fed711b`](https://github.com/agent-of-empires/agent-of-empires/commit/fed711b4da6a900dab3df71e9c3b385a9f432f3f))


### Performance

- **tui:** Cut live-mode input and echo latency to near-attach in [#2822](https://github.com/agent-of-empires/agent-of-empires/pull/2822) by [@njbrake](https://github.com/njbrake) ([`7f46fcf`](https://github.com/agent-of-empires/agent-of-empires/commit/7f46fcf884206057816b1ad84a3fc5f9a3d8fdcd))



### New Contributors

- [@athal7](https://github.com/athal7) made their first contribution in [#2882](https://github.com/agent-of-empires/agent-of-empires/pull/2882)
- [@hamid-mouti](https://github.com/hamid-mouti) made their first contribution in [#2862](https://github.com/agent-of-empires/agent-of-empires/pull/2862)
- [@chen-jiying](https://github.com/chen-jiying) made their first contribution in [#2779](https://github.com/agent-of-empires/agent-of-empires/pull/2779)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.12.1...v1.13.0
## [1.12.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.12.1) - 2026-07-08



### Bug Fixes

- Stop orphaning sandbox containers on session delete and create in [#2576](https://github.com/agent-of-empires/agent-of-empires/pull/2576) by [@peteski22](https://github.com/peteski22) ([`785547f`](https://github.com/agent-of-empires/agent-of-empires/commit/785547f813226a4d4905f316b6b6bad9c4d1a805))
- **session:** Honor repo-local overrides in smart-rename indicator + gate (closes #2351) in [#2602](https://github.com/agent-of-empires/agent-of-empires/pull/2602) by [@jerome-benoit](https://github.com/jerome-benoit) ([`4654abf`](https://github.com/agent-of-empires/agent-of-empires/commit/4654abf85a599c670f852968f825c58f3d8d371e))
- **tui:** Sort storages.keys() before feeding rewire helpers (closes #2584) in [#2604](https://github.com/agent-of-empires/agent-of-empires/pull/2604) by [@jerome-benoit](https://github.com/jerome-benoit) ([`eeda766`](https://github.com/agent-of-empires/agent-of-empires/commit/eeda7665c638bb50040db472aa4bdab19a8c532e))
- **serve:** Honor loopback trust in handler-side elevation gates in [#2611](https://github.com/agent-of-empires/agent-of-empires/pull/2611) by [@Seluj78](https://github.com/Seluj78) ([`7812fd1`](https://github.com/agent-of-empires/agent-of-empires/commit/7812fd170c6cbd8f91f6d0e7e83d16ceef83a8b5))
- **tmux:** Fix poisoned paired-terminal shell and isolate the dev tmux socket in [#2617](https://github.com/agent-of-empires/agent-of-empires/pull/2617) by [@Seluj78](https://github.com/Seluj78) ([`c6143db`](https://github.com/agent-of-empires/agent-of-empires/commit/c6143dbf20b58ff108c3fdd02ceeb1a298df11c8))
- **acp:** Recover runner PID via SO_PEERCRED when registry unreadable (closes #2102) in [#2618](https://github.com/agent-of-empires/agent-of-empires/pull/2618) by [@jerome-benoit](https://github.com/jerome-benoit) ([`de18775`](https://github.com/agent-of-empires/agent-of-empires/commit/de1877536cb87ad31bb7d5df79d44f74a2666f39))
- **server:** Stop rejecting session title/group on shell metacharacters in [#2626](https://github.com/agent-of-empires/agent-of-empires/pull/2626) by [@Seluj78](https://github.com/Seluj78) ([`1adbfe7`](https://github.com/agent-of-empires/agent-of-empires/commit/1adbfe7552708b606790f4dffab1987e8681dede))
- **acp:** Seed sidebar status from post-Stopped agent activity in [#2627](https://github.com/agent-of-empires/agent-of-empires/pull/2627) by [@Seluj78](https://github.com/Seluj78) ([`1d01b51`](https://github.com/agent-of-empires/agent-of-empires/commit/1d01b518de654060f1532d4616be80a2fa34faee))
- **tui:** Remove doubled border seam in stacked mode (closes #2301) in [#2622](https://github.com/agent-of-empires/agent-of-empires/pull/2622) by [@jerome-benoit](https://github.com/jerome-benoit) ([`483bc2e`](https://github.com/agent-of-empires/agent-of-empires/commit/483bc2e891e806c50086272d5a449dd7d2c76796))
- **cockpit:** Sessions stuck 'running' around async background agents and mid-stream stalls in [#2629](https://github.com/agent-of-empires/agent-of-empires/pull/2629) by [@Seluj78](https://github.com/Seluj78) ([`af42727`](https://github.com/agent-of-empires/agent-of-empires/commit/af427278914a4e9e2ffd4bda8a8dd9548a105356))
- Remove Blacksmith CI runner fallback vars in [#2638](https://github.com/agent-of-empires/agent-of-empires/pull/2638) by [@Seluj78](https://github.com/Seluj78) ([`000c174`](https://github.com/agent-of-empires/agent-of-empires/commit/000c1740fa7230d8e1aa226b057581b5d3acde49))
- **acp:** Peer_pid_from_socket bounds connect() with non-blocking + poll(POLLOUT, 100ms) (closes #2621) in [#2630](https://github.com/agent-of-empires/agent-of-empires/pull/2630) by [@jerome-benoit](https://github.com/jerome-benoit) ([`da03c5f`](https://github.com/agent-of-empires/agent-of-empires/commit/da03c5ff2c8fafe7fb5d23907829d73e16d041bc))
- **serve:** Skip .git probe for protected $HOME folders to avoid macOS TCC prompts in [#2644](https://github.com/agent-of-empires/agent-of-empires/pull/2644) by [@Seluj78](https://github.com/Seluj78) ([`1636974`](https://github.com/agent-of-empires/agent-of-empires/commit/16369747a0a3e89b655c074a1953546467131d6a))
- **plugin:** Peel annotated tags in ls_remote to stop phantom update loop in [#2648](https://github.com/agent-of-empires/agent-of-empires/pull/2648) by [@Seluj78](https://github.com/Seluj78) ([`b474a5b`](https://github.com/agent-of-empires/agent-of-empires/commit/b474a5b56ca7107f12ebb817d3338501904e1291))
- **acp:** Recover a bg-Bash turn stalled mid-stream in ~2 min, not 30 in [#2649](https://github.com/agent-of-empires/agent-of-empires/pull/2649) by [@Seluj78](https://github.com/Seluj78) ([`2c5a9fe`](https://github.com/agent-of-empires/agent-of-empires/commit/2c5a9fef3c206bed188467483c9378c0e870f19a))
- **session:** Preserve sandboxed opencode.db across launches; only skip_entries needed for host-drift protection (closes #2605) in [#2650](https://github.com/agent-of-empires/agent-of-empires/pull/2650) by [@jerome-benoit](https://github.com/jerome-benoit) ([`c3b19f9`](https://github.com/agent-of-empires/agent-of-empires/commit/c3b19f993c095ef197f3ac8c6456355ca7897f00))
- **session:** Defer smart-rename to turn-complete + bound global concurrency (closes #2348) in [#2651](https://github.com/agent-of-empires/agent-of-empires/pull/2651) by [@jerome-benoit](https://github.com/jerome-benoit) ([`165f793`](https://github.com/agent-of-empires/agent-of-empires/commit/165f79326a46b1fa0742eb5094f008084bda1fbf))
- **containers:** Close the swallowing-existence-probe bug class in [#2652](https://github.com/agent-of-empires/agent-of-empires/pull/2652) by [@jerome-benoit](https://github.com/jerome-benoit) ([`83d110c`](https://github.com/agent-of-empires/agent-of-empires/commit/83d110c5b7efabcb5d76c034f232941bc95b6bb1))
- **containers:** Per-runtime permission_denied_markers in [#2664](https://github.com/agent-of-empires/agent-of-empires/pull/2664) by [@jerome-benoit](https://github.com/jerome-benoit) ([`cc7947e`](https://github.com/agent-of-empires/agent-of-empires/commit/cc7947ecaade0c6081158e5e210ce0de5ec23587))
- **web:** Compute live-terminal cursor column by cell width, not code units in [#2675](https://github.com/agent-of-empires/agent-of-empires/pull/2675) by [@Seluj78](https://github.com/Seluj78) ([`59651b5`](https://github.com/agent-of-empires/agent-of-empires/commit/59651b58a5d1637cd9f89e10a42706100309eb0d))
- **web:** Restore clipboard image paste in the live terminal in [#2682](https://github.com/agent-of-empires/agent-of-empires/pull/2682) by [@Seluj78](https://github.com/Seluj78) ([`d917362`](https://github.com/agent-of-empires/agent-of-empires/commit/d91736226e0fd80e61d71be44d86b51b6cc3dcd8))
- **web:** Disable worktree toggle for non-git folders, surface a typed create error in [#2683](https://github.com/agent-of-empires/agent-of-empires/pull/2683) by [@Seluj78](https://github.com/Seluj78) ([`b6168bc`](https://github.com/agent-of-empires/agent-of-empires/commit/b6168bca5c1c1d990ea9d5eb15ee8a57ed06b20f))
- **web:** Fill and blink the live terminal cursor while focused in [#2687](https://github.com/agent-of-empires/agent-of-empires/pull/2687) by [@Seluj78](https://github.com/Seluj78) ([`cdf29ac`](https://github.com/agent-of-empires/agent-of-empires/commit/cdf29ac9e44170d6adaf6b8fd7f2bbf01a2c34a6))
- **acp:** Forward SSH_AUTH_SOCK to spawned agents for git-over-SSH in [#2692](https://github.com/agent-of-empires/agent-of-empires/pull/2692) by [@Seluj78](https://github.com/Seluj78) ([`daf3cf8`](https://github.com/agent-of-empires/agent-of-empires/commit/daf3cf895907172a709b595087bbfb0acfbede94))
- **session:** Activity age resets to <1m on TUI/daemon reload in [#2697](https://github.com/agent-of-empires/agent-of-empires/pull/2697) by [@Seluj78](https://github.com/Seluj78) ([`6b66437`](https://github.com/agent-of-empires/agent-of-empires/commit/6b664379dae2298a5f8c4e73286438670d132f56))
- **git:** Promote branch_exists to fail-closed tri-state (closes #2653) in [#2689](https://github.com/agent-of-empires/agent-of-empires/pull/2689) by [@jerome-benoit](https://github.com/jerome-benoit) ([`069e751`](https://github.com/agent-of-empires/agent-of-empires/commit/069e751506546818c4f5abac50638387639f3c51))
- **web:** Image paste, Alt-key forwarding, and live-ws proxy in [#2637](https://github.com/agent-of-empires/agent-of-empires/pull/2637) by [@SYU8384](https://github.com/SYU8384) ([`c3da21e`](https://github.com/agent-of-empires/agent-of-empires/commit/c3da21ea1a83346e07315bfa95d2f86bce43fb58))
- **session:** Sanitize invalid branch ref edges in [#2702](https://github.com/agent-of-empires/agent-of-empires/pull/2702) by [@fengjikui](https://github.com/fengjikui) ([`c737244`](https://github.com/agent-of-empires/agent-of-empires/commit/c73724431505017351a0f24d4067d210803c9dc0))
- Validate plugin uninstall targets in [#2701](https://github.com/agent-of-empires/agent-of-empires/pull/2701) by [@fengjikui](https://github.com/fengjikui) ([`7c27dad`](https://github.com/agent-of-empires/agent-of-empires/commit/7c27dad9d9edb66225f12f8ff65d47ccd043fe9f))
- Preserve structured sessions across state rewrites in [#2639](https://github.com/agent-of-empires/agent-of-empires/pull/2639) by [@SYU8384](https://github.com/SYU8384) ([`d561a96`](https://github.com/agent-of-empires/agent-of-empires/commit/d561a96e13af5e9a0258ccd466aeb2dc4ce8c286))
- **tui:** Keep search matches on Enter so n/N can cycle (closes #2676) in [#2688](https://github.com/agent-of-empires/agent-of-empires/pull/2688) by [@jerome-benoit](https://github.com/jerome-benoit) ([`bfd1c06`](https://github.com/agent-of-empires/agent-of-empires/commit/bfd1c06b9e41f311928a4c1b8464cdaf7f68278c))
- **cli:** Sanitize explicit worktree branches in [#2698](https://github.com/agent-of-empires/agent-of-empires/pull/2698) by [@fengjikui](https://github.com/fengjikui) ([`5933773`](https://github.com/agent-of-empires/agent-of-empires/commit/5933773439e1514df48063682d36bed1f926a12d))
- **session:** Guard the drain against same-cwd session-id clobbering in [#2709](https://github.com/agent-of-empires/agent-of-empires/pull/2709) by [@Seluj78](https://github.com/Seluj78) ([`4e4190c`](https://github.com/agent-of-empires/agent-of-empires/commit/4e4190c956f7606d0fa7cfad8931895a9533e451))
- **web:** Dedupe tool_start across the history page seam on prepend in [#2712](https://github.com/agent-of-empires/agent-of-empires/pull/2712) by [@Seluj78](https://github.com/Seluj78) ([`f54f543`](https://github.com/agent-of-empires/agent-of-empires/commit/f54f543c769226a93858e4c974c832825c283121))
- **tui:** Keep Trash collapsed on delete and skip trash in w-navigation in [#2710](https://github.com/agent-of-empires/agent-of-empires/pull/2710) by [@Eric162](https://github.com/Eric162) ([`434e18b`](https://github.com/agent-of-empires/agent-of-empires/commit/434e18b00f225e46a3f02380c2592e7bcf23cee2))
- **tui:** Exclude trashed sessions from sidebar group count in [#2732](https://github.com/agent-of-empires/agent-of-empires/pull/2732) by [@MatthewWolff](https://github.com/MatthewWolff) ([`d4fb61f`](https://github.com/agent-of-empires/agent-of-empires/commit/d4fb61f7d34293e28eb1fb9bb7c0c0cad7421b1c))
- **tui:** Open project add form from empty picker in [#2738](https://github.com/agent-of-empires/agent-of-empires/pull/2738) by [@Seluj78](https://github.com/Seluj78) ([`98507b7`](https://github.com/agent-of-empires/agent-of-empires/commit/98507b713696026ca1845c25940784c9c62bb52f))
- **tui:** Reveal hidden idle sessions on w in [#2740](https://github.com/agent-of-empires/agent-of-empires/pull/2740) by [@Seluj78](https://github.com/Seluj78) ([`fe5b72f`](https://github.com/agent-of-empires/agent-of-empires/commit/fe5b72fca268680d1dc5f7d1fc41b5596f8d8393))


### Features

- Fork a session to branch convo into new session in [#2569](https://github.com/agent-of-empires/agent-of-empires/pull/2569) by [@MatthewWolff](https://github.com/MatthewWolff) ([`e948735`](https://github.com/agent-of-empires/agent-of-empires/commit/e9487358b0a0700743796aa14874cd058a8317ee))
- **session:** Add auto-resume toggle and fix resume-probe retry loop in [#2612](https://github.com/agent-of-empires/agent-of-empires/pull/2612) by [@Seluj78](https://github.com/Seluj78) ([`fefe603`](https://github.com/agent-of-empires/agent-of-empires/commit/fefe6039941ad15b9e0dfd58aa52fd26c6c82460))
- **session:** Add GitHub Copilot CLI session resume in [#2521](https://github.com/agent-of-empires/agent-of-empires/pull/2521) by [@anxkhn](https://github.com/anxkhn) ([`2c40a97`](https://github.com/agent-of-empires/agent-of-empires/commit/2c40a97d562e32125abd06e5cb7abf8c31083936))
- **web:** Remember last agent instruction in session wizard in [#2620](https://github.com/agent-of-empires/agent-of-empires/pull/2620) by [@Seluj78](https://github.com/Seluj78) ([`6b45f1c`](https://github.com/agent-of-empires/agent-of-empires/commit/6b45f1cb1f37399a8f41e0daedd05a3d53b8197d))
- **web:** Let users pick the terminal font family in [#2619](https://github.com/agent-of-empires/agent-of-empires/pull/2619) by [@Seluj78](https://github.com/Seluj78) ([`809e4c1`](https://github.com/agent-of-empires/agent-of-empires/commit/809e4c1928254e3b940df9e93ce94b90ede63daf))
- Extend first-run tour to open Settings for worktree and plugins in [#2634](https://github.com/agent-of-empires/agent-of-empires/pull/2634) by [@Seluj78](https://github.com/Seluj78) ([`05269e9`](https://github.com/agent-of-empires/agent-of-empires/commit/05269e91aa13fbf5b1af8505b8f54d87f9fdd981))
- **plugin:** Add manifest identity icons (icon, icon_asset) and a discovery source avatar in [#2636](https://github.com/agent-of-empires/agent-of-empires/pull/2636) by [@Seluj78](https://github.com/Seluj78) ([`9747785`](https://github.com/agent-of-empires/agent-of-empires/commit/9747785a9094aad70cba6d582a04aa8f001191f2))
- **acp:** Per-agent structured-view defaults editor (model, mode, per-model thinking) in [#2635](https://github.com/agent-of-empires/agent-of-empires/pull/2635) by [@Seluj78](https://github.com/Seluj78) ([`1b0c32a`](https://github.com/agent-of-empires/agent-of-empires/commit/1b0c32a3aa797c6b7fe636c4e5895da2b95432ea))
- **web:** Render a plugin's icon_asset in the activity bar and dock tab in [#2647](https://github.com/agent-of-empires/agent-of-empires/pull/2647) by [@Seluj78](https://github.com/Seluj78) ([`3283a0a`](https://github.com/agent-of-empires/agent-of-empires/commit/3283a0aba1e0b281f5fe2e41ea939a6bbb172794))
- **cli:** Add `aoe session import` to bulk-import Claude Code sessions in [#2661](https://github.com/agent-of-empires/agent-of-empires/pull/2661) by [@Seluj78](https://github.com/Seluj78) ([`64d88d0`](https://github.com/agent-of-empires/agent-of-empires/commit/64d88d0556ffa01503ac16b331782185b1c2be52))
- **web:** Add "Use this folder" to the directory browser in [#2681](https://github.com/agent-of-empires/agent-of-empires/pull/2681) by [@Seluj78](https://github.com/Seluj78) ([`7dc33f6`](https://github.com/agent-of-empires/agent-of-empires/commit/7dc33f6a9b29978d7897ddc1e838b9c30810b47c))
- **web:** Make agent-output URLs clickable in the terminal view in [#2686](https://github.com/agent-of-empires/agent-of-empires/pull/2686) by [@Seluj78](https://github.com/Seluj78) ([`02fd590`](https://github.com/agent-of-empires/agent-of-empires/commit/02fd5901b13d2e659fb8f6b007563b9e8e86dd39))
- **plugin:** Filter sessions.list and bound UI-state quota per session in [#2695](https://github.com/agent-of-empires/agent-of-empires/pull/2695) by [@Seluj78](https://github.com/Seluj78) ([`1bf90c1`](https://github.com/agent-of-empires/agent-of-empires/commit/1bf90c19de178d400a6d0cf874c35b429c550554))
- **acp:** Raise default structured-view worker cap to 100 in [#2703](https://github.com/agent-of-empires/agent-of-empires/pull/2703) by [@Seluj78](https://github.com/Seluj78) ([`bad349e`](https://github.com/agent-of-empires/agent-of-empires/commit/bad349eaed003cbcf35fc0eca2f11c4e4fa8629b))
- **web:** Support title-derived worktree sessions in [#2737](https://github.com/agent-of-empires/agent-of-empires/pull/2737) by [@Seluj78](https://github.com/Seluj78) ([`d2120b9`](https://github.com/agent-of-empires/agent-of-empires/commit/d2120b93ee911c85dd967974ac923bc5c90f8420))
- **agents:** Configure hook status maps in [#2736](https://github.com/agent-of-empires/agent-of-empires/pull/2736) by [@Seluj78](https://github.com/Seluj78) ([`e6f2465`](https://github.com/agent-of-empires/agent-of-empires/commit/e6f24656aa820bd664c3e517c6d99c7bb9f0c8d1))
- **ui:** Make session row suffix configurable in [#2713](https://github.com/agent-of-empires/agent-of-empires/pull/2713) by [@Seluj78](https://github.com/Seluj78) ([`61a6fd0`](https://github.com/agent-of-empires/agent-of-empires/commit/61a6fd09dee88fe6169d84f91fc0e4714d03c02c))


### Performance

- **acp:** Index event-store lookups by discriminant to fix /api/sessions scans in [#2623](https://github.com/agent-of-empires/agent-of-empires/pull/2623) by [@gilbertl](https://github.com/gilbertl) ([`a20f636`](https://github.com/agent-of-empires/agent-of-empires/commit/a20f636208d5ac37422d86b5e91d6180105566a6))
- **session:** Fast-path smart-rename listener gate on Event variant in [#2662](https://github.com/agent-of-empires/agent-of-empires/pull/2662) by [@jerome-benoit](https://github.com/jerome-benoit) ([`f633feb`](https://github.com/agent-of-empires/agent-of-empires/commit/f633feba758c618b50bc884c7e2f2c476cdab3ee))



### New Contributors

- [@fengjikui](https://github.com/fengjikui) made their first contribution in [#2698](https://github.com/agent-of-empires/agent-of-empires/pull/2698)
- [@SYU8384](https://github.com/SYU8384) made their first contribution in [#2639](https://github.com/agent-of-empires/agent-of-empires/pull/2639)
- [@anxkhn](https://github.com/anxkhn) made their first contribution in [#2521](https://github.com/agent-of-empires/agent-of-empires/pull/2521)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.12.0...v1.12.1
## [1.12.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.12.0) - 2026-07-01



### Bug Fixes

- **worktree:** Dedupe generated civ branches in [#2441](https://github.com/agent-of-empires/agent-of-empires/pull/2441) by [@Seluj78](https://github.com/Seluj78) ([`75cee7b`](https://github.com/agent-of-empires/agent-of-empires/commit/75cee7bac9f5580907c990b3c8f335d17a30ab84))
- **acp:** Enrich opencode approval context in [#2450](https://github.com/agent-of-empires/agent-of-empires/pull/2450) by [@Seluj78](https://github.com/Seluj78) ([`b5dc10f`](https://github.com/agent-of-empires/agent-of-empires/commit/b5dc10f658d53807944c8d3edca0610915af2ea0))
- **serve:** Drop passphrase elevation for plugin pane actions in [#2456](https://github.com/agent-of-empires/agent-of-empires/pull/2456) by [@Seluj78](https://github.com/Seluj78) ([`1780a6c`](https://github.com/agent-of-empires/agent-of-empires/commit/1780a6c7ee04d4b5a9e0a621d622a1d27ad457bc))
- **acp:** Ignore claude acp title pushes in [#2457](https://github.com/agent-of-empires/agent-of-empires/pull/2457) by [@Seluj78](https://github.com/Seluj78) ([`e86947a`](https://github.com/agent-of-empires/agent-of-empires/commit/e86947a8c1b2224fa167f68f4bdc68cd34f6964e))
- **acp:** Stdin keepalive to unstick a stalled agent mid-turn in [#2459](https://github.com/agent-of-empires/agent-of-empires/pull/2459) by [@Seluj78](https://github.com/Seluj78) ([`b89dd5d`](https://github.com/agent-of-empires/agent-of-empires/commit/b89dd5d559f28182ebf80a9b109d2491de5c18f5))
- **hooks:** Gate macos-only TempDir import in [#2478](https://github.com/agent-of-empires/agent-of-empires/pull/2478) by [@Seluj78](https://github.com/Seluj78) ([`ffe7ac3`](https://github.com/agent-of-empires/agent-of-empires/commit/ffe7ac363f28394fc3c1befeaf2341bffb8da02b))
- **session:** Skip corrupt group rows on load in [#2453](https://github.com/agent-of-empires/agent-of-empires/pull/2453) by [@jerome-benoit](https://github.com/jerome-benoit) ([`0f25331`](https://github.com/agent-of-empires/agent-of-empires/commit/0f25331a25759eda1869803ca55c05685f86f3ee))
- **plugins:** Feature plugin-github 1.2.0, unpin broken 1.1.0 in [#2482](https://github.com/agent-of-empires/agent-of-empires/pull/2482) by [@Seluj78](https://github.com/Seluj78) ([`82a70ed`](https://github.com/agent-of-empires/agent-of-empires/commit/82a70ed7d1f4cee1ea8281dcd31357086d8fb00b))
- **notifications:** Retract handled approval/question pushes on other devices in [#2498](https://github.com/agent-of-empires/agent-of-empires/pull/2498) by [@Seluj78](https://github.com/Seluj78) ([`52bd94b`](https://github.com/agent-of-empires/agent-of-empires/commit/52bd94bfe8fe4a911a9e96d6b72d23852a57d923))
- **web:** Persist staged composer attachments in per-session drafts in [#2500](https://github.com/agent-of-empires/agent-of-empires/pull/2500) by [@Seluj78](https://github.com/Seluj78) ([`95aa315`](https://github.com/agent-of-empires/agent-of-empires/commit/95aa3154ae49e78e164d4cc5b1266135a05bbcb4))
- **tui:** Persist trash before tmux teardown in [#2520](https://github.com/agent-of-empires/agent-of-empires/pull/2520) by [@jerome-benoit](https://github.com/jerome-benoit) ([`aa34e44`](https://github.com/agent-of-empires/agent-of-empires/commit/aa34e44574b06136ac3a3525e8e00f617ac65f7a))
- **session:** Exclude stopped peer sids from Claude resume mtime fallback in [#2481](https://github.com/agent-of-empires/agent-of-empires/pull/2481) by [@jerome-benoit](https://github.com/jerome-benoit) ([`283af66`](https://github.com/agent-of-empires/agent-of-empires/commit/283af665eb3f99c09ae80d452e74549d65d259c0))
- **plugins:** Hold the GitHub refresh spinner until the worker's state lands in [#2519](https://github.com/agent-of-empires/agent-of-empires/pull/2519) by [@Seluj78](https://github.com/Seluj78) ([`5c578ba`](https://github.com/agent-of-empires/agent-of-empires/commit/5c578baacdbc9447fd28b649958e5ea49819751d))
- **cli:** Correct trash and purge path bugs (#2524, #2525, #2527, #2534) in [#2542](https://github.com/agent-of-empires/agent-of-empires/pull/2542) by [@Seluj78](https://github.com/Seluj78) ([`560be7a`](https://github.com/agent-of-empires/agent-of-empires/commit/560be7aeff3115c9c8b5467e622dbcdf32ebdeda))
- **serve:** Correct trash/purge server-side defaults (#2523, #2532) in [#2543](https://github.com/agent-of-empires/agent-of-empires/pull/2543) by [@Seluj78](https://github.com/Seluj78) ([`6ef533e`](https://github.com/agent-of-empires/agent-of-empires/commit/6ef533ee85c46aa1cf45773f589e369ee809c18b))
- **web:** Scope Trash to whole workspaces and gate the trashed composer in [#2539](https://github.com/agent-of-empires/agent-of-empires/pull/2539) by [@Seluj78](https://github.com/Seluj78) ([`bfbd508`](https://github.com/agent-of-empires/agent-of-empires/commit/bfbd5082df63b35e37bc1d823d9bdb2c6cad8953))
- Clear idle dormant marker on unarchive in [#2554](https://github.com/agent-of-empires/agent-of-empires/pull/2554) by [@gilbertl](https://github.com/gilbertl) ([`faa7d8c`](https://github.com/agent-of-empires/agent-of-empires/commit/faa7d8cb83e32beb1b9dac9a8a603da995486ad7))
- Update Codex ACP package scope in [#2555](https://github.com/agent-of-empires/agent-of-empires/pull/2555) by [@gilbertl](https://github.com/gilbertl) ([`ac975ac`](https://github.com/agent-of-empires/agent-of-empires/commit/ac975ac187bcfbb5be1b99228304bdebcdac9792))
- **web:** Send backtab (CSI Z) for Shift+Tab in the live terminal in [#2556](https://github.com/agent-of-empires/agent-of-empires/pull/2556) by [@CoreInfusion](https://github.com/CoreInfusion) ([`560af60`](https://github.com/agent-of-empires/agent-of-empires/commit/560af60f6e06b60fb53773005e5adb1c9bf28303))
- **web:** Contain long Trash session names in [#2562](https://github.com/agent-of-empires/agent-of-empires/pull/2562) by [@Seluj78](https://github.com/Seluj78) ([`937acab`](https://github.com/agent-of-empires/agent-of-empires/commit/937acabc574bf6b1d0eac8cb043a77e74fc11598))
- Update Codex ACP yolo mode id in [#2564](https://github.com/agent-of-empires/agent-of-empires/pull/2564) by [@gilbertl](https://github.com/gilbertl) ([`f4b5140`](https://github.com/agent-of-empires/agent-of-empires/commit/f4b514064cf90ebc5d7f3d3f8c6627750bcfcf57))
- **hooks:** Extract Claude session_id via jq in sandbox (closes #1760) in [#2545](https://github.com/agent-of-empires/agent-of-empires/pull/2545) by [@jerome-benoit](https://github.com/jerome-benoit) ([`c4810a5`](https://github.com/agent-of-empires/agent-of-empires/commit/c4810a5a9828e46aa43f0d53fee0f6073bd6eef8))
- **web:** Place sidebar Trash count badge next to the Trash icon in [#2577](https://github.com/agent-of-empires/agent-of-empires/pull/2577) by [@Seluj78](https://github.com/Seluj78) ([`5e4f6cd`](https://github.com/agent-of-empires/agent-of-empires/commit/5e4f6cd60514a87fa1c72b973364f54af91546c5))
- **sidebar:** Suppress unread indicator on archived and snoozed rows in [#2575](https://github.com/agent-of-empires/agent-of-empires/pull/2575) by [@Seluj78](https://github.com/Seluj78) ([`d7be8d9`](https://github.com/agent-of-empires/agent-of-empires/commit/d7be8d9de43271455349d888332944e763fb674a))
- **tui:** Enable kitty keyboard protocol so Shift+Enter inserts a newline in live mode (closes #2362) in [#2582](https://github.com/agent-of-empires/agent-of-empires/pull/2582) by [@jerome-benoit](https://github.com/jerome-benoit) ([`bf7cc36`](https://github.com/agent-of-empires/agent-of-empires/commit/bf7cc3681834ee13e83bb632c9eefe03feadec53))
- **tui:** Persist Reload Failed dialog ack across identical rewire failures (closes #2112) in [#2578](https://github.com/agent-of-empires/agent-of-empires/pull/2578) by [@jerome-benoit](https://github.com/jerome-benoit) ([`92ceeab`](https://github.com/agent-of-empires/agent-of-empires/commit/92ceeab22f7fb61398de69fcc4b24381a840b523))
- **tmux:** Pin pane-base-index 0 in raw new-session tests (#2231) in [#2568](https://github.com/agent-of-empires/agent-of-empires/pull/2568) by [@jerome-benoit](https://github.com/jerome-benoit) ([`51ffaed`](https://github.com/agent-of-empires/agent-of-empires/commit/51ffaed9b846c74c0a8185b2bb2aad10dd20d22f))
- Suppress PWA lifecycle network toasts in [#2588](https://github.com/agent-of-empires/agent-of-empires/pull/2588) by [@amanzainal](https://github.com/amanzainal) ([`583a05b`](https://github.com/agent-of-empires/agent-of-empires/commit/583a05bf7fcd7638e0d65a71918b739dace8abbe))
- **web:** Order Trash section newest-first in [#2599](https://github.com/agent-of-empires/agent-of-empires/pull/2599) by [@Seluj78](https://github.com/Seluj78) ([`dc22d76`](https://github.com/agent-of-empires/agent-of-empires/commit/dc22d764ad66ff19fed5978b5da0346956266bf5))


### Features

- **server:** Render the web live view from the shared VtChannel (event-driven, mouse forwarding) in [#2435](https://github.com/agent-of-empires/agent-of-empires/pull/2435) by [@njbrake](https://github.com/njbrake) ([`13d0752`](https://github.com/agent-of-empires/agent-of-empires/commit/13d0752e4092cca0736793aa9e438b6e2b8333d5))
- **panes:** Tabbed pane groups with multiple terminal tabs in [#2446](https://github.com/agent-of-empires/agent-of-empires/pull/2446) by [@Seluj78](https://github.com/Seluj78) ([`c46b5ed`](https://github.com/agent-of-empires/agent-of-empires/commit/c46b5edb1810df635c87ce660e9a4f81775287ed))
- **web:** Drag-and-drop pane reordering and cross-dock move in [#2458](https://github.com/agent-of-empires/agent-of-empires/pull/2458) by [@Seluj78](https://github.com/Seluj78) ([`093038d`](https://github.com/agent-of-empires/agent-of-empires/commit/093038d947ba1b746e14526e764d527b5829c32a))
- **acp:** Live background sub-agents panel for the async Task tool in [#2461](https://github.com/agent-of-empires/agent-of-empires/pull/2461) by [@Seluj78](https://github.com/Seluj78) ([`0e81d22`](https://github.com/agent-of-empires/agent-of-empires/commit/0e81d22dd02d5caa8161205e585e7e5a2e38abf8))
- **acp:** Resume rate-limited structured sessions in [#2465](https://github.com/agent-of-empires/agent-of-empires/pull/2465) by [@Seluj78](https://github.com/Seluj78) ([`26085ba`](https://github.com/agent-of-empires/agent-of-empires/commit/26085ba6dd04afbe4cd266bd9661b3b2f418df3a))
- **tui:** Render plugin UI slots in the structured view in [#2447](https://github.com/agent-of-empires/agent-of-empires/pull/2447) by [@Seluj78](https://github.com/Seluj78) ([`d8a2b21`](https://github.com/agent-of-empires/agent-of-empires/commit/d8a2b2164a76b891f9f7e1c5be43440c190bc2b3))
- **plugin-ui:** Render a PR comment block and validated hex color in plugin panes in [#2442](https://github.com/agent-of-empires/agent-of-empires/pull/2442) by [@Seluj78](https://github.com/Seluj78) ([`2ffd4c8`](https://github.com/agent-of-empires/agent-of-empires/commit/2ffd4c8765afcb1efb064dfc4fb8e1f3f5230a71))
- **plugins:** Add status manifest section and aoe_version host compatibility in [#2469](https://github.com/agent-of-empires/agent-of-empires/pull/2469) by [@Seluj78](https://github.com/Seluj78) ([`893b493`](https://github.com/agent-of-empires/agent-of-empires/commit/893b4939e1b1836b09a16ed506f3672d0f6d1ecb))
- **telemetry:** Add plugin-adoption census to usage_snapshot in [#2471](https://github.com/agent-of-empires/agent-of-empires/pull/2471) by [@Seluj78](https://github.com/Seluj78) ([`ed4ccc3`](https://github.com/agent-of-empires/agent-of-empires/commit/ed4ccc37a7fe49fd75e27877d9d321265c495213))
- **web:** Render plugin sort-key and filter-facet slots in the dashboard sidebar in [#2470](https://github.com/agent-of-empires/agent-of-empires/pull/2470) by [@Seluj78](https://github.com/Seluj78) ([`51e4ba2`](https://github.com/agent-of-empires/agent-of-empires/commit/51e4ba24a0113f46d8147939c6d2e0530c6c9f93))
- **plugin:** Pane 64KB payload budget and expandable comment bodies in [#2476](https://github.com/agent-of-empires/agent-of-empires/pull/2476) by [@Seluj78](https://github.com/Seluj78) ([`4bcfee7`](https://github.com/agent-of-empires/agent-of-empires/commit/4bcfee767e9b44b5e3a43d4e4553ac93bad0cdce))
- **plugins:** Add GitHub discovery and update checks (#2365) in [#2473](https://github.com/agent-of-empires/agent-of-empires/pull/2473) by [@Seluj78](https://github.com/Seluj78) ([`c392152`](https://github.com/agent-of-empires/agent-of-empires/commit/c392152a63ffbb91e9e4700fde1220e3eea58eb2))
- **plugin:** Per-version featured verification and load-path hash memo in [#2472](https://github.com/agent-of-empires/agent-of-empires/pull/2472) by [@Seluj78](https://github.com/Seluj78) ([`eed4307`](https://github.com/agent-of-empires/agent-of-empires/commit/eed4307c18952e62d04b03c829a1a1691b30fb87))
- **plugin:** Feature plugin-github 1.0.0 and 1.1.0 releases in [#2479](https://github.com/agent-of-empires/agent-of-empires/pull/2479) by [@Seluj78](https://github.com/Seluj78) ([`d38bf11`](https://github.com/agent-of-empires/agent-of-empires/commit/d38bf11f17b7f040d9e6d5fcfee244d88b7fe713))
- **plugin:** Default install to latest release and fix featured build-tree load in [#2480](https://github.com/agent-of-empires/agent-of-empires/pull/2480) by [@Seluj78](https://github.com/Seluj78) ([`b4e0b58`](https://github.com/agent-of-empires/agent-of-empires/commit/b4e0b58f3e73aecaaab500c0383001864853f384))
- **web:** Show spinner for plugin refresh actions (manual and auto) in [#2496](https://github.com/agent-of-empires/agent-of-empires/pull/2496) by [@Seluj78](https://github.com/Seluj78) ([`8193635`](https://github.com/agent-of-empires/agent-of-empires/commit/819363501a6dd2a6ed44abce04e9339efcaaf697))
- **cli:** Show trust/validation in plugin install output in [#2497](https://github.com/agent-of-empires/agent-of-empires/pull/2497) by [@Seluj78](https://github.com/Seluj78) ([`7f19ae4`](https://github.com/agent-of-empires/agent-of-empires/commit/7f19ae415faaa822278c4c318f492e27aca6373e))
- **plugin:** In-app capability-consent popup for plugin updates in [#2499](https://github.com/agent-of-empires/agent-of-empires/pull/2499) by [@Seluj78](https://github.com/Seluj78) ([`e785e52`](https://github.com/agent-of-empires/agent-of-empires/commit/e785e52056ed70942240db703da95f542a625a4c))
- **plugin:** Expose archived/snoozed flags on sessions.list in [#2505](https://github.com/agent-of-empires/agent-of-empires/pull/2505) by [@Seluj78](https://github.com/Seluj78) ([`dfe82aa`](https://github.com/agent-of-empires/agent-of-empires/commit/dfe82aa614bf96c5ff770acc269b34b76bcea9d7))
- **web:** Same-location pane splits in the right and bottom docks in [#2501](https://github.com/agent-of-empires/agent-of-empires/pull/2501) by [@Seluj78](https://github.com/Seluj78) ([`154503f`](https://github.com/agent-of-empires/agent-of-empires/commit/154503fd8d93b9fbd87d7b8d31be6673fbf7bd17))
- **session:** Configurable trash for deleted sessions with restore and purge in [#2502](https://github.com/agent-of-empires/agent-of-empires/pull/2502) by [@Seluj78](https://github.com/Seluj78) ([`ec54fdf`](https://github.com/agent-of-empires/agent-of-empires/commit/ec54fdf18a1b4226cdf30e2aade1e7d7d3e549fe))
- **plugin:** Screenshots and GIFs in the marketplace detail modal in [#2507](https://github.com/agent-of-empires/agent-of-empires/pull/2507) by [@Seluj78](https://github.com/Seluj78) ([`d368cc1`](https://github.com/agent-of-empires/agent-of-empires/commit/d368cc108c26248caaac4c2cfe854e106ea6268e))
- **plugins:** Install, uninstall, and update plugins from the dashboard in [#2508](https://github.com/agent-of-empires/agent-of-empires/pull/2508) by [@Seluj78](https://github.com/Seluj78) ([`b08ac43`](https://github.com/agent-of-empires/agent-of-empires/commit/b08ac43649528596490d67d73562b12f5ad90e91))
- 🗿🥚 (web + TUI) in [#2511](https://github.com/agent-of-empires/agent-of-empires/pull/2511) by [@Seluj78](https://github.com/Seluj78) ([`7ceaca7`](https://github.com/agent-of-empires/agent-of-empires/commit/7ceaca75c6e56fed1454ba82a20921a26b51c70b))
- **web:** Search session conversations from the command palette in [#2517](https://github.com/agent-of-empires/agent-of-empires/pull/2517) by [@Seluj78](https://github.com/Seluj78) ([`4b050b5`](https://github.com/agent-of-empires/agent-of-empires/commit/4b050b5917ae868cbfe5cd194788d160ce61f202))
- **web:** Move sidebar Trash to footer icon, order Projects below Snoozed & archived in [#2516](https://github.com/agent-of-empires/agent-of-empires/pull/2516) by [@Seluj78](https://github.com/Seluj78) ([`1cfe207`](https://github.com/agent-of-empires/agent-of-empires/commit/1cfe207d871a558042f5c4c78fa01f0539548fb5))
- **web:** Surface plugin row slots and panes on the mobile dashboard in [#2518](https://github.com/agent-of-empires/agent-of-empires/pull/2518) by [@Seluj78](https://github.com/Seluj78) ([`badd942`](https://github.com/agent-of-empires/agent-of-empires/commit/badd942622bd77285ae16fa16c74874d7379ab7b))
- **plugins:** Client-executed command actions, with open-ui-link in the cmd+k palette and keymap in [#2526](https://github.com/agent-of-empires/agent-of-empires/pull/2526) by [@Seluj78](https://github.com/Seluj78) ([`953932b`](https://github.com/agent-of-empires/agent-of-empires/commit/953932b700475a03f44122ed368d0b1cf41397e9))
- **worktree:** Relocate a session's worktree out of the active dir when trashed in [#2531](https://github.com/agent-of-empires/agent-of-empires/pull/2531) by [@Seluj78](https://github.com/Seluj78) ([`5672004`](https://github.com/agent-of-empires/agent-of-empires/commit/56720046616937efee4543630a4a78f50f3de61b))
- **plugins:** Show an update changelog before applying, in the web modal and TUI in [#2581](https://github.com/agent-of-empires/agent-of-empires/pull/2581) by [@Seluj78](https://github.com/Seluj78) ([`b149c9d`](https://github.com/agent-of-empires/agent-of-empires/commit/b149c9d21399d7db535c7cdc20cbc5463d603468))
- **web:** Scope Cmd+K palette results with category tabs in [#2589](https://github.com/agent-of-empires/agent-of-empires/pull/2589) by [@Seluj78](https://github.com/Seluj78) ([`300cf46`](https://github.com/agent-of-empires/agent-of-empires/commit/300cf46bbed5759a4cb9e3152a9004aadf311a37))
- **tui:** Add session.confirm_delete toggle to guard delete in [#2595](https://github.com/agent-of-empires/agent-of-empires/pull/2595) by [@Seluj78](https://github.com/Seluj78) ([`7dcda4b`](https://github.com/agent-of-empires/agent-of-empires/commit/7dcda4b5c8640a697c850206241f284684f6a2ed))
- **web:** Add Restore button to the trashed-session banner in [#2594](https://github.com/agent-of-empires/agent-of-empires/pull/2594) by [@Seluj78](https://github.com/Seluj78) ([`0c2aa9c`](https://github.com/agent-of-empires/agent-of-empires/commit/0c2aa9cc7161c5333dad7d1dc6fd03b72a2dae1c))
- **dashboard:** Serve session artifacts and stop dead /tmp transcript links in [#2598](https://github.com/agent-of-empires/agent-of-empires/pull/2598) by [@Seluj78](https://github.com/Seluj78) ([`1c768ba`](https://github.com/agent-of-empires/agent-of-empires/commit/1c768ba1ba8883228810eaac9410b2ed2e5c4577))



### New Contributors

- [@amanzainal](https://github.com/amanzainal) made their first contribution in [#2588](https://github.com/agent-of-empires/agent-of-empires/pull/2588)
- [@CoreInfusion](https://github.com/CoreInfusion) made their first contribution in [#2556](https://github.com/agent-of-empires/agent-of-empires/pull/2556)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.11.3...v1.12.0
## [1.11.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.11.3) - 2026-06-26



### Bug Fixes

- **session:** Delete workspace dir when removing a multi-repo session in [#2369](https://github.com/agent-of-empires/agent-of-empires/pull/2369) by [@Seluj78](https://github.com/Seluj78) ([`9485e69`](https://github.com/agent-of-empires/agent-of-empires/commit/9485e69e5e1a811e90ba0816dcba2acd3985776d))
- **web:** Sidebar project badge counts only live workspaces, not archived/snoozed in [#2373](https://github.com/agent-of-empires/agent-of-empires/pull/2373) by [@Seluj78](https://github.com/Seluj78) ([`afc4211`](https://github.com/agent-of-empires/agent-of-empires/commit/afc4211ffaa2e1edbe435b73f654882ef33dc3e3))
- **acp:** Self-heal a quiet between-prompt turn with an expired wake in [#2377](https://github.com/agent-of-empires/agent-of-empires/pull/2377) by [@Seluj78](https://github.com/Seluj78) ([`3499cfd`](https://github.com/agent-of-empires/agent-of-empires/commit/3499cfd1d21399f40ba1bc6d42639b887eea313e))
- **acp:** Demote premature orphan cancel to prompt_complete on late cost frame in [#2378](https://github.com/agent-of-empires/agent-of-empires/pull/2378) by [@Seluj78](https://github.com/Seluj78) ([`98278fb`](https://github.com/agent-of-empires/agent-of-empires/commit/98278fb1241357e2b4c8902fe42379dd66c02600))
- **session:** Preserve slashes in title-derived branch names in [#2379](https://github.com/agent-of-empires/agent-of-empires/pull/2379) by [@Seluj78](https://github.com/Seluj78) ([`296f210`](https://github.com/agent-of-empires/agent-of-empires/commit/296f21064b2f099568d989a853fa4b4c4d73bf67))
- **kiro:** Launch via `chat` subcommand and support user-selected agents in [#2382](https://github.com/agent-of-empires/agent-of-empires/pull/2382) by [@MatthewWolff](https://github.com/MatthewWolff) ([`3d162d9`](https://github.com/agent-of-empires/agent-of-empires/commit/3d162d908a72b726a6049fa8d6d436fe1adb3424))
- **tui:** Stabilize live capture worker empty capture test in [#2390](https://github.com/agent-of-empires/agent-of-empires/pull/2390) by [@snvtac](https://github.com/snvtac) ([`d2ad16b`](https://github.com/agent-of-empires/agent-of-empires/commit/d2ad16bf64a62cb31503fa9313f138567bfe28e0))
- **kiro:** Resolve --agent hooks target by name field, not filename in [#2395](https://github.com/agent-of-empires/agent-of-empires/pull/2395) by [@MatthewWolff](https://github.com/MatthewWolff) ([`83a34ac`](https://github.com/agent-of-empires/agent-of-empires/commit/83a34accd24b45ed700193569c509a176114cc81))
- **session:** Publish captured sid to live tmux pane in [#2361](https://github.com/agent-of-empires/agent-of-empires/pull/2361) by [@snvtac](https://github.com/snvtac) ([`d4610b9`](https://github.com/agent-of-empires/agent-of-empires/commit/d4610b9ead7395539b1cbc17d94e70032317a4df))
- **web:** Paste with Ctrl+V and copy with Ctrl+Shift+C in the live terminal in [#2409](https://github.com/agent-of-empires/agent-of-empires/pull/2409) by [@njbrake](https://github.com/njbrake) ([`2ee0021`](https://github.com/agent-of-empires/agent-of-empires/commit/2ee002127fda57d18095d930a927d60670bc1030))
- **tui:** Forward wheel as arrow keys for alt-screen apps without mouse tracking in [#2411](https://github.com/agent-of-empires/agent-of-empires/pull/2411) by [@njbrake](https://github.com/njbrake) ([`9208eab`](https://github.com/agent-of-empires/agent-of-empires/commit/9208eab3bb7a0351d026bd23072625d25a1521e6))
- **tui:** Scroll alt-screen agents in passive preview, send PgUp/PgDn in [#2417](https://github.com/agent-of-empires/agent-of-empires/pull/2417) by [@njbrake](https://github.com/njbrake) ([`9ba95ab`](https://github.com/agent-of-empires/agent-of-empires/commit/9ba95ab835a200f004aed259110274814601349e))
- **sandbox:** Pin container workdir so exec survives worktree git-linkage breaks in [#2418](https://github.com/agent-of-empires/agent-of-empires/pull/2418) by [@njbrake](https://github.com/njbrake) ([`ac48296`](https://github.com/agent-of-empires/agent-of-empires/commit/ac48296c69578407b07b2150d78299831d1bd029))
- **web:** Default the new-session worktree toggle to worktree.enabled in [#2424](https://github.com/agent-of-empires/agent-of-empires/pull/2424) by [@Seluj78](https://github.com/Seluj78) ([`4ee0875`](https://github.com/agent-of-empires/agent-of-empires/commit/4ee0875cefa59382a81813c4a5c75b886e064605))
- **hooks:** Leave Running via Claude idle_prompt + StopFailure hooks in [#2429](https://github.com/agent-of-empires/agent-of-empires/pull/2429) by [@njbrake](https://github.com/njbrake) ([`6e1272e`](https://github.com/agent-of-empires/agent-of-empires/commit/6e1272e36e02f13096a67b274de7882672a82b63))
- **acp:** Surface OpenCode prompt runtime errors in [#2427](https://github.com/agent-of-empires/agent-of-empires/pull/2427) by [@Seluj78](https://github.com/Seluj78) ([`57b6cf0`](https://github.com/agent-of-empires/agent-of-empires/commit/57b6cf0f07793bfd16b4433c5b7f6efc13132e09))
- **serve:** Reject native title prompt echoes in [#2430](https://github.com/agent-of-empires/agent-of-empires/pull/2430) by [@Seluj78](https://github.com/Seluj78) ([`a543e8e`](https://github.com/agent-of-empires/agent-of-empires/commit/a543e8e58eaf5822006fc45d166a125c9d816abd))
- **git:** Lock aoe-created worktrees so a cross-boundary prune can't reap them in [#2440](https://github.com/agent-of-empires/agent-of-empires/pull/2440) by [@njbrake](https://github.com/njbrake) ([`9cb1476`](https://github.com/agent-of-empires/agent-of-empires/commit/9cb147605b8e447aeaebc38662813da9bff9bb0f))
- **web:** Make structured-view code blocks horizontally scrollable in [#2444](https://github.com/agent-of-empires/agent-of-empires/pull/2444) by [@Seluj78](https://github.com/Seluj78) ([`d1adb0d`](https://github.com/agent-of-empires/agent-of-empires/commit/d1adb0d057d3f2edee31405105793406c8f88262))
- **hooks:** Recover Claude session to Idle after an Esc interrupt in [#2449](https://github.com/agent-of-empires/agent-of-empires/pull/2449) by [@njbrake](https://github.com/njbrake) ([`7351550`](https://github.com/agent-of-empires/agent-of-empires/commit/735155064c563bc5cefcdc48a521f5505a920bb3))
- **tui:** Keep interior spacing in vt100 live preview (regression from #2433) in [#2448](https://github.com/agent-of-empires/agent-of-empires/pull/2448) by [@njbrake](https://github.com/njbrake) ([`14877c5`](https://github.com/agent-of-empires/agent-of-empires/commit/14877c5893ab93b3298bd1e27f792dd432ef00f1))


### Features

- **plugins:** Minimal plugin core (registry + enable/disable) in [#2311](https://github.com/agent-of-empires/agent-of-empires/pull/2311) by [@njbrake](https://github.com/njbrake) ([`ea69911`](https://github.com/agent-of-empires/agent-of-empires/commit/ea699113e28bfe0ea0ac3d260ab72787194c7662))
- **plugins:** Per-session plugin_meta + persisted plugin settings (#2091) in [#2368](https://github.com/agent-of-empires/agent-of-empires/pull/2368) by [@Seluj78](https://github.com/Seluj78) ([`a082f1b`](https://github.com/agent-of-empires/agent-of-empires/commit/a082f1bfd2e03c21cb2e94751033c94f5c7c937b))
- **plugins:** Manifest contribution schema, capability grants, external install + lockfile in [#2375](https://github.com/agent-of-empires/agent-of-empires/pull/2375) by [@Seluj78](https://github.com/Seluj78) ([`f465980`](https://github.com/agent-of-empires/agent-of-empires/commit/f465980ca708e890db7e83b08e096480ee7cc9f1))
- **plugins:** Featured/curated index + integrity hashing in [#2388](https://github.com/agent-of-empires/agent-of-empires/pull/2388) by [@Seluj78](https://github.com/Seluj78) ([`b1c8ce0`](https://github.com/agent-of-empires/agent-of-empires/commit/b1c8ce05e91418eb87011c3d2de19fd067c2d515))
- **plugin:** Tier 1 worker host: runtime resolution, ndjson worker protocol, capability-gated API, NoSandbox in [#2387](https://github.com/agent-of-empires/agent-of-empires/pull/2387) by [@Seluj78](https://github.com/Seluj78) ([`560960d`](https://github.com/agent-of-empires/agent-of-empires/commit/560960d28e6a1d6198a449a779596c7ffdfc22f7))
- **plugins:** Tier 0 contribution registries (settings, themes, keybinds, CLI grafting) in [#2389](https://github.com/agent-of-empires/agent-of-empires/pull/2389) by [@Seluj78](https://github.com/Seluj78) ([`823b522`](https://github.com/agent-of-empires/agent-of-empires/commit/823b522113d876a4819667c33daee0b42d8812c5))
- **plugin:** Worker-facing config.get host RPC in [#2399](https://github.com/agent-of-empires/agent-of-empires/pull/2399) by [@Seluj78](https://github.com/Seluj78) ([`747abf4`](https://github.com/agent-of-empires/agent-of-empires/commit/747abf485608ba67575a0f9eceb95ba1c5bde58f))
- **acp:** Apply claude-agent-acp >=0.52 native session title push, skip redundant smart_rename one-shot in [#2400](https://github.com/agent-of-empires/agent-of-empires/pull/2400) by [@Seluj78](https://github.com/Seluj78) ([`5948bc1`](https://github.com/agent-of-empires/agent-of-empires/commit/5948bc142956e295f94c4015f9882bdcde99953a))
- **tui:** Reassure that updating won't tear down running sessions in [#2408](https://github.com/agent-of-empires/agent-of-empires/pull/2408) by [@njbrake](https://github.com/njbrake) ([`4af658c`](https://github.com/agent-of-empires/agent-of-empires/commit/4af658c9a440ffe7fa9e21c77a0bab73f72ba838))
- **web:** Live terminal polish (bottom padding + crisp Safari fonts), fix xtask dev serve build in [#2412](https://github.com/agent-of-empires/agent-of-empires/pull/2412) by [@njbrake](https://github.com/njbrake) ([`e82f787`](https://github.com/agent-of-empires/agent-of-empires/commit/e82f787e9e2c66e64c6261901bdbd074220ff8cd))
- **plugin:** Run command-worker build steps for interpreted/venv workers in [#2406](https://github.com/agent-of-empires/agent-of-empires/pull/2406) by [@Seluj78](https://github.com/Seluj78) ([`f0dc777`](https://github.com/agent-of-empires/agent-of-empires/commit/f0dc777634ed78fec68e5fdbaa4ba1b99ae5d5ea))
- **plugins:** Plugin UI extension points (host-rendered slots over capability-gated RPCs) in [#2405](https://github.com/agent-of-empires/agent-of-empires/pull/2405) by [@Seluj78](https://github.com/Seluj78) ([`5d1d399`](https://github.com/agent-of-empires/agent-of-empires/commit/5d1d399bb11cff1efcef3cdac1eb31c033041797))
- **plugins:** Dockable pane tool-windows, activity bar, and pane actions in [#2432](https://github.com/agent-of-empires/agent-of-empires/pull/2432) by [@Seluj78](https://github.com/Seluj78) ([`1d9fb8b`](https://github.com/agent-of-empires/agent-of-empires/commit/1d9fb8bdbe67c5046d6fadcb34445ff420e5aeae))
- **tui:** Default live preview to in-process vt100 over tmux pipe-pane in [#2433](https://github.com/agent-of-empires/agent-of-empires/pull/2433) by [@njbrake](https://github.com/njbrake) ([`24716de`](https://github.com/agent-of-empires/agent-of-empires/commit/24716dee70180fc22d4acd93292d06883e246047))
- **tui:** Forward mouse clicks and drags to the live agent in live-send in [#2422](https://github.com/agent-of-empires/agent-of-empires/pull/2422) by [@njbrake](https://github.com/njbrake) ([`821f00c`](https://github.com/agent-of-empires/agent-of-empires/commit/821f00c7fe6b21fd9092e71d6419dcfaab66cdf6))
- **tui:** Interactive preview pane (mouse forwarding, smooth scroll, edge autoscroll, double-click attach) in [#2425](https://github.com/agent-of-empires/agent-of-empires/pull/2425) by [@njbrake](https://github.com/njbrake) ([`cd23d50`](https://github.com/agent-of-empires/agent-of-empires/commit/cd23d505f0e8b12318c17a1f175377dfd6e248da))



### New Contributors

- [@snvtac](https://github.com/snvtac) made their first contribution in [#2361](https://github.com/agent-of-empires/agent-of-empires/pull/2361)
- [@MatthewWolff](https://github.com/MatthewWolff) made their first contribution in [#2395](https://github.com/agent-of-empires/agent-of-empires/pull/2395)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.11.2...v1.11.3
## [1.11.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.11.2) - 2026-06-24



### Bug Fixes

- **release:** Drop obsolete clawhub publish.js sed workaround in [#2269](https://github.com/agent-of-empires/agent-of-empires/pull/2269) by [@njbrake](https://github.com/njbrake) ([`f5b535f`](https://github.com/agent-of-empires/agent-of-empires/commit/f5b535f296e0f53072975a32d5a902ca2e8c107e))
- **hooks:** Harden /tmp/aoe-hooks against TOCTOU and symlink attacks on multi-tenant hosts in [#2221](https://github.com/agent-of-empires/agent-of-empires/pull/2221) by [@jerome-benoit](https://github.com/jerome-benoit) ([`85ba40f`](https://github.com/agent-of-empires/agent-of-empires/commit/85ba40f2f8c5a3842eb84e6fd9f7e6f6892cf201))
- **web:** Raise new-session wizard above the tooltip layer on mobile in [#2259](https://github.com/agent-of-empires/agent-of-empires/pull/2259) by [@Seluj78](https://github.com/Seluj78) ([`6112575`](https://github.com/agent-of-empires/agent-of-empires/commit/6112575c57ee9e5c0e0cba6c1992db7b23cbafaf))
- **web:** Auto-dismiss stuck "Waking…" banner after the wake fires in [#2261](https://github.com/agent-of-empires/agent-of-empires/pull/2261) by [@Seluj78](https://github.com/Seluj78) ([`cbff94e`](https://github.com/agent-of-empires/agent-of-empires/commit/cbff94e9116d71f0c6e5a1dcee75f18bbc3b1865))
- **serve:** Flip sidebar dot to Running on agent activity in [#2264](https://github.com/agent-of-empires/agent-of-empires/pull/2264) by [@Seluj78](https://github.com/Seluj78) ([`d154304`](https://github.com/agent-of-empires/agent-of-empires/commit/d154304bacd807a2caa51ce6d3df7c3acef505d6))
- **serve:** Tied-worktree rename no longer crash-loops the structured-view worker in [#2271](https://github.com/agent-of-empires/agent-of-empires/pull/2271) by [@Seluj78](https://github.com/Seluj78) ([`19474a1`](https://github.com/agent-of-empires/agent-of-empires/commit/19474a1dac9684a59394fd43fc50ed7c0db00872))
- **migrations:** Route all persistent writes through atomic_write in [#2272](https://github.com/agent-of-empires/agent-of-empires/pull/2272) by [@jerome-benoit](https://github.com/jerome-benoit) ([`4310710`](https://github.com/agent-of-empires/agent-of-empires/commit/43107109e588552f164a6300fb83a9b3779686e5))
- **tui:** Clear context-menu highlight when the cursor leaves every item in [#2275](https://github.com/agent-of-empires/agent-of-empires/pull/2275) by [@njbrake](https://github.com/njbrake) ([`1136baa`](https://github.com/agent-of-empires/agent-of-empires/commit/1136baa6e5baee245c653ca657d3178022d913f8))
- **hooks:** Atomic + symlink-following writes for user-symlinked configs in [#2265](https://github.com/agent-of-empires/agent-of-empires/pull/2265) by [@jerome-benoit](https://github.com/jerome-benoit) ([`5975387`](https://github.com/agent-of-empires/agent-of-empires/commit/59753871b832d2753aa2e443f85a1053ff56ee05))
- **tui:** Hold a fresh manual unread for the current visit in [#2284](https://github.com/agent-of-empires/agent-of-empires/pull/2284) by [@njbrake](https://github.com/njbrake) ([`2486c27`](https://github.com/agent-of-empires/agent-of-empires/commit/2486c2754aaee5313a0771ff371e194ccb0d0c39))
- **hooks:** Tighten parse-error signaling and marker matching in [#2278](https://github.com/agent-of-empires/agent-of-empires/pull/2278) by [@jerome-benoit](https://github.com/jerome-benoit) ([`bdc792e`](https://github.com/agent-of-empires/agent-of-empires/commit/bdc792ea07ded54000c2be9f8c0181c020328005))
- **session:** Keep unread rank from reviving sunk rows in [#2280](https://github.com/agent-of-empires/agent-of-empires/pull/2280) by [@jerome-benoit](https://github.com/jerome-benoit) ([`73db5c2`](https://github.com/agent-of-empires/agent-of-empires/commit/73db5c2391f330b46ff1c529a03eb92b09b9d16a))
- Honor yolo/auto-approve for codex sessions (full-access mode) in [#2289](https://github.com/agent-of-empires/agent-of-empires/pull/2289) by [@gilbertl](https://github.com/gilbertl) ([`7fe63e5`](https://github.com/agent-of-empires/agent-of-empires/commit/7fe63e50b2620c410a8f6b462520fbcb5404384c))
- **tui:** Exit live-send before tmux attach on double-click in [#2293](https://github.com/agent-of-empires/agent-of-empires/pull/2293) by [@njbrake](https://github.com/njbrake) ([`a561584`](https://github.com/agent-of-empires/agent-of-empires/commit/a5615849648fbf992350bc4725aca0b4eacad67f))
- **theme:** Tokyo-night-storm unread brighter than waiting on dark bg in [#2297](https://github.com/agent-of-empires/agent-of-empires/pull/2297) by [@jerome-benoit](https://github.com/jerome-benoit) ([`7fb21c6`](https://github.com/agent-of-empires/agent-of-empires/commit/7fb21c6c9cedad2028a1d72ef9a2c247918a3d1f))
- **tui:** Address PR #2294 review follow-ups (hover staleness, persistence logging, docs) in [#2299](https://github.com/agent-of-empires/agent-of-empires/pull/2299) by [@njbrake](https://github.com/njbrake) ([`ff0a2f1`](https://github.com/agent-of-empires/agent-of-empires/commit/ff0a2f1c5323af6441539a4f4c50f45471ee1533))
- **acp:** Stop agent messages rendering twice from leaked consolidated chunk in [#2308](https://github.com/agent-of-empires/agent-of-empires/pull/2308) by [@Seluj78](https://github.com/Seluj78) ([`f173e78`](https://github.com/agent-of-empires/agent-of-empires/commit/f173e783b79a78c4e97293452f8bbc4f572beacb))
- **telemetry:** Recover plan-mode signal, retire orphaned view_toggles in [#2309](https://github.com/agent-of-empires/agent-of-empires/pull/2309) by [@Seluj78](https://github.com/Seluj78) ([`5f70511`](https://github.com/agent-of-empires/agent-of-empires/commit/5f705119e77f37a29ce788fb62b2241444422930))
- **web:** Keep sub-agent Task with its children at the history window cut in [#2314](https://github.com/agent-of-empires/agent-of-empires/pull/2314) by [@Seluj78](https://github.com/Seluj78) ([`b96c7ba`](https://github.com/agent-of-empires/agent-of-empires/commit/b96c7ba87876f90223b3eddd5ffac8eba3a67ca7))
- **hooks:** Preserve foreign keys and skip rewrite when state is clean in [#2295](https://github.com/agent-of-empires/agent-of-empires/pull/2295) by [@jerome-benoit](https://github.com/jerome-benoit) ([`0bd14d8`](https://github.com/agent-of-empires/agent-of-empires/commit/0bd14d85d311c99a641f1f16c6c640be49be5b06))
- **session:** Rescan live session id on resume across tracked agents in [#2298](https://github.com/agent-of-empires/agent-of-empires/pull/2298) by [@jerome-benoit](https://github.com/jerome-benoit) ([`1ca619f`](https://github.com/agent-of-empires/agent-of-empires/commit/1ca619f24f402bdc10cc248fbdaf1fc25b3383b9))
- **web:** Sidebar Shift+click range after plain click, and move bulk triage to the right-click menu in [#2315](https://github.com/agent-of-empires/agent-of-empires/pull/2315) by [@Seluj78](https://github.com/Seluj78) ([`eeb5db5`](https://github.com/agent-of-empires/agent-of-empires/commit/eeb5db5d059f4fb472e5b8da3070199b03bdc25a))
- **serve:** Make structured-view smart-rename observable and robust in [#2266](https://github.com/agent-of-empires/agent-of-empires/pull/2266) by [@Seluj78](https://github.com/Seluj78) ([`2302907`](https://github.com/agent-of-empires/agent-of-empires/commit/230290784b15982192e3b942bc76681ae2c3c62f))
- **serve:** Silence past-due wakeup trace spam and skip lookups for archived sessions in [#2327](https://github.com/agent-of-empires/agent-of-empires/pull/2327) by [@Seluj78](https://github.com/Seluj78) ([`3bfff9f`](https://github.com/agent-of-empires/agent-of-empires/commit/3bfff9f101983a167dbd7bf05925b1bd14aa9679))
- **web:** Self-heal zombie structured-view WebSocket behind a proxy in [#2324](https://github.com/agent-of-empires/agent-of-empires/pull/2324) by [@Seluj78](https://github.com/Seluj78) ([`3f89f85`](https://github.com/agent-of-empires/agent-of-empires/commit/3f89f85cc4399ba2da791eba9b78707ed92ebaa1))
- **acp:** End agent-initiated turns that run with no driving prompt in [#2326](https://github.com/agent-of-empires/agent-of-empires/pull/2326) by [@Seluj78](https://github.com/Seluj78) ([`7287d3e`](https://github.com/agent-of-empires/agent-of-empires/commit/7287d3e18e331007bfcbfb573088f5eb735b85c3))
- **web:** Smooth mobile live-view scrollback + virtualize rows in [#2331](https://github.com/agent-of-empires/agent-of-empires/pull/2331) by [@njbrake](https://github.com/njbrake) ([`2e2e7c3`](https://github.com/agent-of-empires/agent-of-empires/commit/2e2e7c37ae45cf50582019163f7acb754bdd4371))
- **acp:** Stop agent messages rendering twice from leaked consolidated chunk with absent id in [#2334](https://github.com/agent-of-empires/agent-of-empires/pull/2334) by [@Seluj78](https://github.com/Seluj78) ([`dcb9a5b`](https://github.com/agent-of-empires/agent-of-empires/commit/dcb9a5b804ded3e7a847d9795e2d8849364fde42))
- **web:** Render empty TodoWrite clears as a todos card, not a blank think card in [#2336](https://github.com/agent-of-empires/agent-of-empires/pull/2336) by [@Seluj78](https://github.com/Seluj78) ([`01107fb`](https://github.com/agent-of-empires/agent-of-empires/commit/01107fbe18ae87fad78ea54fc4d76977ed298ca9))
- **tui:** Treat Ctrl+J (bare LF) as a newline in agent message inputs in [#2342](https://github.com/agent-of-empires/agent-of-empires/pull/2342) by [@Eric162](https://github.com/Eric162) ([`76bf392`](https://github.com/agent-of-empires/agent-of-empires/commit/76bf3924fc019e4b5d46c289b2469a75bc9c7471))
- **tui:** Scope group identity by profile to stop same-named group key conflicts in [#2343](https://github.com/agent-of-empires/agent-of-empires/pull/2343) by [@Eric162](https://github.com/Eric162) ([`594887d`](https://github.com/agent-of-empires/agent-of-empires/commit/594887d5ee947eaff519846ad8e6808ce9bd800d))
- **session:** Prefer authoritative hook sidecar over mtime scan on Claude resume in [#2345](https://github.com/agent-of-empires/agent-of-empires/pull/2345) by [@lamdor](https://github.com/lamdor) ([`a455987`](https://github.com/agent-of-empires/agent-of-empires/commit/a45598734fd03cdf1d0a6368b1a5c2b11bb1828f))
- **tui:** Persist project-mode folder collapse state across restarts in [#2357](https://github.com/agent-of-empires/agent-of-empires/pull/2357) by [@njbrake](https://github.com/njbrake) ([`7b17d43`](https://github.com/agent-of-empires/agent-of-empires/commit/7b17d43a96bbe01fb892ce7a017ca96cd791927d))


### Features

- **acp:** One-click adapter update clears every blocked session in [#2263](https://github.com/agent-of-empires/agent-of-empires/pull/2263) by [@Seluj78](https://github.com/Seluj78) ([`502ff50`](https://github.com/agent-of-empires/agent-of-empires/commit/502ff503f5b513ee3ccba0ea4c3e6fcc83e5574d))
- **serve:** Show a "monitoring" indicator for sessions parked on a Monitor tool in [#2270](https://github.com/agent-of-empires/agent-of-empires/pull/2270) by [@Seluj78](https://github.com/Seluj78) ([`e0d394d`](https://github.com/agent-of-empires/agent-of-empires/commit/e0d394dcf264ad6d8f33b2c3b8808666d53b5f5d))
- Unread session state (TUI + web dashboard) in [#2088](https://github.com/agent-of-empires/agent-of-empires/pull/2088) by [@Eric162](https://github.com/Eric162) ([`1dddf7e`](https://github.com/agent-of-empires/agent-of-empires/commit/1dddf7ec13e78681befc0dab6d6b80c339a17cbc))
- **tui:** Collapsible sidebar + clickable footer toolbar (and fix config-save screen flicker) in [#2294](https://github.com/agent-of-empires/agent-of-empires/pull/2294) by [@njbrake](https://github.com/njbrake) ([`425227b`](https://github.com/agent-of-empires/agent-of-empires/commit/425227b7a5994bf228fefde3617f49e2394bdd8b))
- **tui:** Tips system with badge, overlay, and an earned tip (#2262) in [#2282](https://github.com/agent-of-empires/agent-of-empires/pull/2282) by [@njbrake](https://github.com/njbrake) ([`da78adf`](https://github.com/agent-of-empires/agent-of-empires/commit/da78adfcce79977b80bf86998573400828a9ab3f))
- **acp:** Import existing Claude Code sessions into the structured view in [#2330](https://github.com/agent-of-empires/agent-of-empires/pull/2330) by [@Seluj78](https://github.com/Seluj78) ([`3f53343`](https://github.com/agent-of-empires/agent-of-empires/commit/3f533437ec0e67794f53da1a0ad4e646e7414222))
- **web:** Copy session id from the About modal in [#2328](https://github.com/agent-of-empires/agent-of-empires/pull/2328) by [@Seluj78](https://github.com/Seluj78) ([`aaf2937`](https://github.com/agent-of-empires/agent-of-empires/commit/aaf293781237bf9c43e8f8005d1174fd4fc73189))
- **web:** Surface the tips system on the dashboard as a tip-of-the-day in [#2332](https://github.com/agent-of-empires/agent-of-empires/pull/2332) by [@Seluj78](https://github.com/Seluj78) ([`05bbe80`](https://github.com/agent-of-empires/agent-of-empires/commit/05bbe80967524d88b475aaff47509b0bc7ad5891))
- **web:** Scroll structured-view transcript file links to the cited line in [#2338](https://github.com/agent-of-empires/agent-of-empires/pull/2338) by [@Seluj78](https://github.com/Seluj78) ([`06b0b74`](https://github.com/agent-of-empires/agent-of-empires/commit/06b0b74e7904daa4d4d14373aa3c704af62e3612))
- **acp:** Require opencode 1.16.0 and humanize permission approval titles in [#2339](https://github.com/agent-of-empires/agent-of-empires/pull/2339) by [@Seluj78](https://github.com/Seluj78) ([`05340c9`](https://github.com/agent-of-empires/agent-of-empires/commit/05340c955477a47b1622d031606779dde93a0cbe))
- **serve,web:** Show full file contents for unchanged cited files in [#2340](https://github.com/agent-of-empires/agent-of-empires/pull/2340) by [@Seluj78](https://github.com/Seluj78) ([`437ca6f`](https://github.com/agent-of-empires/agent-of-empires/commit/437ca6fb1e6270f2b1eb0fa6f43b992612f720fd))
- **smart-rename:** Per-CLI rename agent, manual re-trigger, and reliability fixes in [#2350](https://github.com/agent-of-empires/agent-of-empires/pull/2350) by [@Seluj78](https://github.com/Seluj78) ([`8819173`](https://github.com/agent-of-empires/agent-of-empires/commit/8819173377ebd5ee9f64fe1d4e0aeffb53f013ed))
- **hooks:** Flip Codex hook storage from config.toml to hooks.json in [#2353](https://github.com/agent-of-empires/agent-of-empires/pull/2353) by [@jerome-benoit](https://github.com/jerome-benoit) ([`013640d`](https://github.com/agent-of-empires/agent-of-empires/commit/013640d8bf0358049a7dc21c7818095defad152e))
- **web:** Flatten nested group names instead of truncating to leaf in [#2335](https://github.com/agent-of-empires/agent-of-empires/pull/2335) by [@Seluj78](https://github.com/Seluj78) ([`92bae24`](https://github.com/agent-of-empires/agent-of-empires/commit/92bae24e0c62979fb82016ac3d017bfcf070f5bb))


### Performance

- **serve:** Set MissedTickBehavior::Delay on status_poll_loop interval (#2305) in [#2307](https://github.com/agent-of-empires/agent-of-empires/pull/2307) by [@jerome-benoit](https://github.com/jerome-benoit) ([`43dfbf6`](https://github.com/agent-of-empires/agent-of-empires/commit/43dfbf6781965ca585e1feeac54d388de1fe129f))



### New Contributors

- [@lamdor](https://github.com/lamdor) made their first contribution in [#2345](https://github.com/agent-of-empires/agent-of-empires/pull/2345)
- [@gilbertl](https://github.com/gilbertl) made their first contribution in [#2289](https://github.com/agent-of-empires/agent-of-empires/pull/2289)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.11.1...v1.11.2
## [1.11.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.11.1) - 2026-06-17



### Bug Fixes

- **web:** Keep structured-view composer above the iOS soft keyboard in [#2014](https://github.com/agent-of-empires/agent-of-empires/pull/2014) by [@njbrake](https://github.com/njbrake) ([`d5741f3`](https://github.com/agent-of-empires/agent-of-empires/commit/d5741f3b59089ccf4d2537636025afcdd571c974))
- **tui:** Advance the cursor to the next session on archive in [#2107](https://github.com/agent-of-empires/agent-of-empires/pull/2107) by [@njbrake](https://github.com/njbrake) ([`5d9c3c2`](https://github.com/agent-of-empires/agent-of-empires/commit/5d9c3c22b9b6773b6a4feb9069f61872c81d96ef))
- Block tied-worktree rename while a sandbox container holds the mount in [#2117](https://github.com/agent-of-empires/agent-of-empires/pull/2117) by [@njbrake](https://github.com/njbrake) ([`e6ffdab`](https://github.com/agent-of-empires/agent-of-empires/commit/e6ffdabfc00f20ba366abad7fb24b4324fd3dab2))
- **session:** Skip a corrupt row in Storage::load in [#2119](https://github.com/agent-of-empires/agent-of-empires/pull/2119) by [@markphilipp](https://github.com/markphilipp) ([`7e877a8`](https://github.com/agent-of-empires/agent-of-empires/commit/7e877a82da469a260eec2b43f35a7d1443f89774))
- **tui:** Forward the wheel to full-screen live-send agents instead of dead scrollback in [#2123](https://github.com/agent-of-empires/agent-of-empires/pull/2123) by [@njbrake](https://github.com/njbrake) ([`884186a`](https://github.com/agent-of-empires/agent-of-empires/commit/884186a8db0438d4a4fd028c81a5962d86a4df27))
- **serve:** Run session deletion bookkeeping in a detached task in [#2127](https://github.com/agent-of-empires/agent-of-empires/pull/2127) by [@Seluj78](https://github.com/Seluj78) ([`bac8cd7`](https://github.com/agent-of-empires/agent-of-empires/commit/bac8cd76fcf6840367bc44f91984183675335acd))
- **web:** Suppress working spinner while a question or approval card is pending in [#2151](https://github.com/agent-of-empires/agent-of-empires/pull/2151) by [@Seluj78](https://github.com/Seluj78) ([`823880a`](https://github.com/agent-of-empires/agent-of-empires/commit/823880a45688ad9cce20954afb174f3655926995))
- **recovery:** Propagate on_launch hook timeouts as Status::Error in [#2169](https://github.com/agent-of-empires/agent-of-empires/pull/2169) by [@jerome-benoit](https://github.com/jerome-benoit) ([`0a1f374`](https://github.com/agent-of-empires/agent-of-empires/commit/0a1f3747a2858315ad2192690e9d174632c6bfa4))
- **nix:** Pin rustc via rust-overlay so the Cachix build tracks stable in [#2181](https://github.com/agent-of-empires/agent-of-empires/pull/2181) by [@njbrake](https://github.com/njbrake) ([`c6d7394`](https://github.com/agent-of-empires/agent-of-empires/commit/c6d7394550a742f20f2e26e36e1f7882cdbd1f02))
- **hooks:** Rewrite installed hook strings on startup so existing agent configs pick up shell hardening in [#2168](https://github.com/agent-of-empires/agent-of-empires/pull/2168) by [@jerome-benoit](https://github.com/jerome-benoit) ([`bcc809f`](https://github.com/agent-of-empires/agent-of-empires/commit/bcc809f07367b76fcb6bbd2614ec96637506abc0))
- **web:** Clean and markdown-render synthesize-mode memory recall card in [#2165](https://github.com/agent-of-empires/agent-of-empires/pull/2165) by [@Seluj78](https://github.com/Seluj78) ([`86116a5`](https://github.com/agent-of-empires/agent-of-empires/commit/86116a5738362a96a13d5733a280b22cba729691))
- **web:** Name diff base in empty changes panel in [#2180](https://github.com/agent-of-empires/agent-of-empires/pull/2180) by [@Seluj78](https://github.com/Seluj78) ([`0f79b25`](https://github.com/agent-of-empires/agent-of-empires/commit/0f79b257b9fab477c6de5e21ca3c2968b0007b46))
- **web:** Render structured-view tool-card file paths repo-relative in [#2162](https://github.com/agent-of-empires/agent-of-empires/pull/2162) by [@Seluj78](https://github.com/Seluj78) ([`61c7fa2`](https://github.com/agent-of-empires/agent-of-empires/commit/61c7fa2ead1d0119ca40981f4a9c856b193486cf))
- **acp:** Stop silent-orphan watchdog killing monitor turns in [#2183](https://github.com/agent-of-empires/agent-of-empires/pull/2183) by [@Seluj78](https://github.com/Seluj78) ([`a20b130`](https://github.com/agent-of-empires/agent-of-empires/commit/a20b1307e8a1216f190b73adc957fa47a8770b3e))
- **serve:** Persist recent projects so a deleted-session project stays in the wizard in [#2157](https://github.com/agent-of-empires/agent-of-empires/pull/2157) by [@Seluj78](https://github.com/Seluj78) ([`1e81784`](https://github.com/agent-of-empires/agent-of-empires/commit/1e817845b477d1a6882c445072337857a21c114f))
- **session:** Tear down all tmux sessions on archive in [#2179](https://github.com/agent-of-empires/agent-of-empires/pull/2179) by [@jerome-benoit](https://github.com/jerome-benoit) ([`42eb3b1`](https://github.com/agent-of-empires/agent-of-empires/commit/42eb3b1a136254ee72476449eef8107113699f70))
- **git:** Diff against origin tip when local base is stale in [#2194](https://github.com/agent-of-empires/agent-of-empires/pull/2194) by [@Seluj78](https://github.com/Seluj78) ([`2a2ba24`](https://github.com/agent-of-empires/agent-of-empires/commit/2a2ba24cd0574b4d59e0cd8fd7753d76f178b512))
- **tui:** Clear screen without a cursor-position query in [#2204](https://github.com/agent-of-empires/agent-of-empires/pull/2204) by [@peteski22](https://github.com/peteski22) ([`9f040c6`](https://github.com/agent-of-empires/agent-of-empires/commit/9f040c6c30c5411a25ca2f644721256745dbb2f7))
- **tui:** Run session restart off the event loop in [#2202](https://github.com/agent-of-empires/agent-of-empires/pull/2202) by [@njbrake](https://github.com/njbrake) ([`716fcd3`](https://github.com/agent-of-empires/agent-of-empires/commit/716fcd37dec739f8ad343bc799ff56e0aeabcab7))
- **projects:** Unpin keeps the saved project; decouple pin from the registry in [#2225](https://github.com/agent-of-empires/agent-of-empires/pull/2225) by [@Seluj78](https://github.com/Seluj78) ([`443f4ba`](https://github.com/agent-of-empires/agent-of-empires/commit/443f4baa76d8a0911a8cce1c1a3d3a46eebff2f2))
- **session:** Keep archived sessions out of Error when their tmux is gone in [#2227](https://github.com/agent-of-empires/agent-of-empires/pull/2227) by [@Seluj78](https://github.com/Seluj78) ([`073a2a6`](https://github.com/agent-of-empires/agent-of-empires/commit/073a2a62bf8ef71e1a3981fecceb7699d8c0f043))
- **acp:** Reliable structured-view stop, turn-end, and session recovery in [#2239](https://github.com/agent-of-empires/agent-of-empires/pull/2239) by [@Seluj78](https://github.com/Seluj78) ([`17d5430`](https://github.com/agent-of-empires/agent-of-empires/commit/17d54303574d817c8d87fc4b331cc9734c4a8638))
- **serve:** Heal structured session status out of a stale Stopped in [#2253](https://github.com/agent-of-empires/agent-of-empires/pull/2253) by [@Seluj78](https://github.com/Seluj78) ([`6043a35`](https://github.com/agent-of-empires/agent-of-empires/commit/6043a35881a421499032e6e8917be11fb5cc7e86))
- **tui:** Keep diff file list selection visible in [#2254](https://github.com/agent-of-empires/agent-of-empires/pull/2254) by [@MTGVim](https://github.com/MTGVim) ([`679eebb`](https://github.com/agent-of-empires/agent-of-empires/commit/679eebbf11e5bb12f24aae217080de6151d4e1cc))
- **web:** Portal the sidebar tooltip so it is not clipped at the panel edge in [#2217](https://github.com/agent-of-empires/agent-of-empires/pull/2217) by [@Seluj78](https://github.com/Seluj78) ([`2306474`](https://github.com/agent-of-empires/agent-of-empires/commit/230647439bb86f713b9a8ddecbeeba564ba82a67))
- **web:** Tolerate finger jitter on sidebar long-press so the session menu opens on Android in [#2234](https://github.com/agent-of-empires/agent-of-empires/pull/2234) by [@Seluj78](https://github.com/Seluj78) ([`604797f`](https://github.com/agent-of-empires/agent-of-empires/commit/604797f67ff966161d3e166e22a5917fb0d34a32))
- **web:** Allow archive and snooze on pinned sessions in [#2235](https://github.com/agent-of-empires/agent-of-empires/pull/2235) by [@Seluj78](https://github.com/Seluj78) ([`b5cfa14`](https://github.com/agent-of-empires/agent-of-empires/commit/b5cfa14264816412811e0761b8cc83670cd14a00))
- **session:** Preserve sid on ambiguous resume failure in [#2224](https://github.com/agent-of-empires/agent-of-empires/pull/2224) by [@jerome-benoit](https://github.com/jerome-benoit) ([`33f4b2d`](https://github.com/agent-of-empires/agent-of-empires/commit/33f4b2dccb479b9091a061a0a6a9cf10017ca4e1))
- **web:** Differentiate the two new-session button tooltips in [#2215](https://github.com/agent-of-empires/agent-of-empires/pull/2215) by [@Seluj78](https://github.com/Seluj78) ([`70dd5c6`](https://github.com/agent-of-empires/agent-of-empires/commit/70dd5c6db8c208334862fed24fdc031ad42ccb28))


### Features

- **web:** Server-side UI state sync, session Stop action, and dashboard/dev polish in [#2106](https://github.com/agent-of-empires/agent-of-empires/pull/2106) by [@njbrake](https://github.com/njbrake) ([`5180953`](https://github.com/agent-of-empires/agent-of-empires/commit/5180953c1a58a2db7e7ceca20664fe93e98d723f))
- **acp:** AskUserQuestion in the web structured view via ACP elicitation in [#2100](https://github.com/agent-of-empires/agent-of-empires/pull/2100) by [@Seluj78](https://github.com/Seluj78) ([`6f036b3`](https://github.com/agent-of-empires/agent-of-empires/commit/6f036b389eae7f83fd6634e04edf3ffe3f4cbbdc))
- **file-watch:** Config live-reload migration; HomeView config kicks via watcher in [#1741](https://github.com/agent-of-empires/agent-of-empires/pull/1741) by [@jerome-benoit](https://github.com/jerome-benoit) ([`c366594`](https://github.com/agent-of-empires/agent-of-empires/commit/c366594238e8a30ac12de1dd68a8a5af456b2971))
- **file-watch:** Dispatch Action::SetTheme on watcher-driven config refresh in [#2116](https://github.com/agent-of-empires/agent-of-empires/pull/2116) by [@jerome-benoit](https://github.com/jerome-benoit) ([`20fb965`](https://github.com/agent-of-empires/agent-of-empires/commit/20fb965f4332aa8055cc6de1a4a385ccda348261))
- **tui:** Optional branch rename in the tied worktree rename dialog in [#2118](https://github.com/agent-of-empires/agent-of-empires/pull/2118) by [@njbrake](https://github.com/njbrake) ([`3508830`](https://github.com/agent-of-empires/agent-of-empires/commit/3508830312af862b2e2c0b62f897c51c81c0f60c))
- **sandbox:** Host-side before_start hooks that inject env into the container in [#2114](https://github.com/agent-of-empires/agent-of-empires/pull/2114) by [@njbrake](https://github.com/njbrake) ([`9624744`](https://github.com/agent-of-empires/agent-of-empires/commit/962474444e60245be48913d90e94559c6a8a7c5d))
- **sandbox:** Per-session input + repo slug for before_start hooks in [#2121](https://github.com/agent-of-empires/agent-of-empires/pull/2121) by [@njbrake](https://github.com/njbrake) ([`e0daa74`](https://github.com/agent-of-empires/agent-of-empires/commit/e0daa74efafded8371b9268bbd49607243986f92))
- **sandbox:** Per-session input + repo slug for before_start hooks (re-land #2121) in [#2149](https://github.com/agent-of-empires/agent-of-empires/pull/2149) by [@njbrake](https://github.com/njbrake) ([`4dfb0eb`](https://github.com/agent-of-empires/agent-of-empires/commit/4dfb0ebcb364ca0503549257c2bea7192b001294))
- **telemetry:** Wire the session-create counter into the TUI surface in [#2122](https://github.com/agent-of-empires/agent-of-empires/pull/2122) by [@markphilipp](https://github.com/markphilipp) ([`31eb255`](https://github.com/agent-of-empires/agent-of-empires/commit/31eb255e9bfbb669226d6422cd8b5cafd22e1da3))
- **structured-view:** Recall and edit queued prompts with ArrowUp/ArrowDown in [#2155](https://github.com/agent-of-empires/agent-of-empires/pull/2155) by [@Seluj78](https://github.com/Seluj78) ([`e7ce7be`](https://github.com/agent-of-empires/agent-of-empires/commit/e7ce7be64448b4700ace64c85dac494068965560))
- Add "New Session" to the session right-click menu (TUI and web) in [#2195](https://github.com/agent-of-empires/agent-of-empires/pull/2195) by [@Seluj78](https://github.com/Seluj78) ([`f24b469`](https://github.com/agent-of-empires/agent-of-empires/commit/f24b469521a52636483cbcc9960cd6c1d3fdde61))
- **serve:** Notify on AskUserQuestion with a dedicated push and chime in [#2158](https://github.com/agent-of-empires/agent-of-empires/pull/2158) by [@Seluj78](https://github.com/Seluj78) ([`af7b090`](https://github.com/agent-of-empires/agent-of-empires/commit/af7b0906a58aec049dd31e645db8c2a7b86711e7))
- **web:** Render structured-view history recent-first with Load earlier in [#2161](https://github.com/agent-of-empires/agent-of-empires/pull/2161) by [@Seluj78](https://github.com/Seluj78) ([`4ddd738`](https://github.com/agent-of-empires/agent-of-empires/commit/4ddd73845e116be19405d0531cb26c31368efcb2))
- **web:** Dedicated tool cards for ToolSearch, Monitor, TaskStop in [#2167](https://github.com/agent-of-empires/agent-of-empires/pull/2167) by [@Seluj78](https://github.com/Seluj78) ([`afc6eb9`](https://github.com/agent-of-empires/agent-of-empires/commit/afc6eb92d0f76ee7bb64b982b9671a5aad04238a))
- **web:** Pin a project so it persists in the sidebar without sessions in [#2193](https://github.com/agent-of-empires/agent-of-empires/pull/2193) by [@Seluj78](https://github.com/Seluj78) ([`5b696ad`](https://github.com/agent-of-empires/agent-of-empires/commit/5b696ad6b3d445246a370c05df743d314ce38f6c))
- **web:** Show saved projects in new-session wizard Recent tab in [#2166](https://github.com/agent-of-empires/agent-of-empires/pull/2166) by [@Seluj78](https://github.com/Seluj78) ([`92d9a30`](https://github.com/agent-of-empires/agent-of-empires/commit/92d9a30142d8f848d5609fd7729536dc70d418da))
- Allow registering non-git directories as projects in [#2182](https://github.com/agent-of-empires/agent-of-empires/pull/2182) by [@plainlystated](https://github.com/plainlystated) ([`039de77`](https://github.com/agent-of-empires/agent-of-empires/commit/039de7762d4b94a19cb279bf119a25533f91c1b3))
- **web:** Move Profiles into a Settings tab, drop the sidebar button in [#2218](https://github.com/agent-of-empires/agent-of-empires/pull/2218) by [@Seluj78](https://github.com/Seluj78) ([`432e6f4`](https://github.com/agent-of-empires/agent-of-empires/commit/432e6f4a60e37235d7e5c4c95ce2658dc32ba117))
- **serve:** Single live size-owner across TUI, web, and mobile in [#2115](https://github.com/agent-of-empires/agent-of-empires/pull/2115) by [@njbrake](https://github.com/njbrake) ([`d349b6f`](https://github.com/agent-of-empires/agent-of-empires/commit/d349b6f1e57ae3aaa6779559eeb72d2483fbc19f))
- **web:** Surface individual settings in the command palette in [#2197](https://github.com/agent-of-empires/agent-of-empires/pull/2197) by [@Seluj78](https://github.com/Seluj78) ([`2be8a05`](https://github.com/agent-of-empires/agent-of-empires/commit/2be8a059ca88df4779ac58a68fa0a59cfcc09e87))
- **serve:** Web UI to install or update a missing or out-of-date ACP agent in [#2198](https://github.com/agent-of-empires/agent-of-empires/pull/2198) by [@Seluj78](https://github.com/Seluj78) ([`a06be42`](https://github.com/agent-of-empires/agent-of-empires/commit/a06be42f1879bf871be308fd73fdec833d158512))
- **web:** Restore last session on PWA relaunch in [#2200](https://github.com/agent-of-empires/agent-of-empires/pull/2200) by [@Seluj78](https://github.com/Seluj78) ([`592d39b`](https://github.com/agent-of-empires/agent-of-empires/commit/592d39b5c2af2803d15f311206f3da397a5392db))
- **serve:** Smart-rename structured view sessions from the first message in [#2201](https://github.com/agent-of-empires/agent-of-empires/pull/2201) by [@Seluj78](https://github.com/Seluj78) ([`7cacf57`](https://github.com/agent-of-empires/agent-of-empires/commit/7cacf572bba8248b5cd96cd256702a48387c0a9a))
- **settings:** Full-text settings search on the web, fuzzy matching on both surfaces in [#2223](https://github.com/agent-of-empires/agent-of-empires/pull/2223) by [@Seluj78](https://github.com/Seluj78) ([`313c869`](https://github.com/agent-of-empires/agent-of-empires/commit/313c869d03f0f844fd31ad20c9fe16ab98a6a893))
- **cli:** Add aoe killall panic command to force-stop daemon, workers, and tmux sessions in [#2156](https://github.com/agent-of-empires/agent-of-empires/pull/2156) by [@Seluj78](https://github.com/Seluj78) ([`269d4ff`](https://github.com/agent-of-empires/agent-of-empires/commit/269d4ffc509fd54ec61081de4ae2eb8acbd86dc2))
- **web:** Rework sidebar project rows, drop grab bar, hover-swap icon/chevron, add session count in [#2216](https://github.com/agent-of-empires/agent-of-empires/pull/2216) by [@Seluj78](https://github.com/Seluj78) ([`20e8b17`](https://github.com/agent-of-empires/agent-of-empires/commit/20e8b176e36dc48ae6258f73e238880f13ac7338))
- **acp:** Show answered AskUserQuestion in the structured-view transcript in [#2228](https://github.com/agent-of-empires/agent-of-empires/pull/2228) by [@Seluj78](https://github.com/Seluj78) ([`b260a90`](https://github.com/agent-of-empires/agent-of-empires/commit/b260a90fbd4e8add408256060ec2e68ce6f5c2fa))
- **web:** Tap anywhere to open the keyboard on mobile (terminal and structured view) in [#2249](https://github.com/agent-of-empires/agent-of-empires/pull/2249) by [@Seluj78](https://github.com/Seluj78) ([`5b2dec3`](https://github.com/agent-of-empires/agent-of-empires/commit/5b2dec322cd65f12d9453c0ed34ec78f403e98de))
- **web:** Rework new-session wizard into a single smart screen with progressive disclosure in [#2247](https://github.com/agent-of-empires/agent-of-empires/pull/2247) by [@Seluj78](https://github.com/Seluj78) ([`fd5d0f1`](https://github.com/agent-of-empires/agent-of-empires/commit/fd5d0f1c1f1b0075f760f51777d05d1fabd892b5))
- **web:** Recent-first structured-view history with auto-load on scroll-up in [#2240](https://github.com/agent-of-empires/agent-of-empires/pull/2240) by [@Seluj78](https://github.com/Seluj78) ([`d40b122`](https://github.com/agent-of-empires/agent-of-empires/commit/d40b122d327917820bf7e9025c866a9549ea68f7))
- **web:** Move the projects page into a dedicated sidebar section in [#2226](https://github.com/agent-of-empires/agent-of-empires/pull/2226) by [@Seluj78](https://github.com/Seluj78) ([`e946d39`](https://github.com/agent-of-empires/agent-of-empires/commit/e946d39ae17478ec08fd44705fa4cda9f6ee7e2e))


### Other

- Revert "feat(sandbox): per-session input + repo slug for before_start hooks (#2121)" in [#2148](https://github.com/agent-of-empires/agent-of-empires/pull/2148) by [@njbrake](https://github.com/njbrake) ([`0ef30a4`](https://github.com/agent-of-empires/agent-of-empires/commit/0ef30a4d803403a35843629ed980e784e6299c90))
- Update Dependabot labels from 'area:acp' to 'area:web' in [#2136](https://github.com/agent-of-empires/agent-of-empires/pull/2136) by [@Seluj78](https://github.com/Seluj78) ([`91100ad`](https://github.com/agent-of-empires/agent-of-empires/commit/91100adec34bef9225f1093bfde7d658d7ec5233))


### Reverts

- **nix:** Drop rust-overlay pin, fix cfg_select via nixpkgs lock bump in [#2192](https://github.com/agent-of-empires/agent-of-empires/pull/2192) by [@njbrake](https://github.com/njbrake) ([`5e8d69f`](https://github.com/agent-of-empires/agent-of-empires/commit/5e8d69fd9127654cc1342c6313fbed07f0c6c0fd))



### New Contributors

- [@MTGVim](https://github.com/MTGVim) made their first contribution in [#2254](https://github.com/agent-of-empires/agent-of-empires/pull/2254)
- [@plainlystated](https://github.com/plainlystated) made their first contribution in [#2182](https://github.com/agent-of-empires/agent-of-empires/pull/2182)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.11.0...v1.11.1
## [1.11.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.11.0) - 2026-06-10



### Bug Fixes

- On create hooks in container in [#1990](https://github.com/agent-of-empires/agent-of-empires/pull/1990) by [@Davicittod](https://github.com/Davicittod) ([`10827e7`](https://github.com/agent-of-empires/agent-of-empires/commit/10827e79f0facbd1f3cf76d52894c0f42b7dc10f))
- **tui:** Show a cursor in the live-send preview in [#2028](https://github.com/agent-of-empires/agent-of-empires/pull/2028) by [@njbrake](https://github.com/njbrake) ([`d0867e5`](https://github.com/agent-of-empires/agent-of-empires/commit/d0867e55e3fdb8af6df38d90f2b5ed55c3e2540c))
- **tui:** Calmer session archiving and right-click archive in [#2025](https://github.com/agent-of-empires/agent-of-empires/pull/2025) by [@njbrake](https://github.com/njbrake) ([`8ae2770`](https://github.com/agent-of-empires/agent-of-empires/commit/8ae27702421cb7ad4f5b5977ce6271320b6d74ef))
- **session:** Keep wrapped-agent idle status instead of masking to Unknown in [#2030](https://github.com/agent-of-empires/agent-of-empires/pull/2030) by [@njbrake](https://github.com/njbrake) ([`ce6d11c`](https://github.com/agent-of-empires/agent-of-empires/commit/ce6d11cdc2c91381c71350cd1c43ce768a1438cd))
- **tui:** Surface on_create hook output in creation failure dialog in [#1992](https://github.com/agent-of-empires/agent-of-empires/pull/1992) by [@markphilipp](https://github.com/markphilipp) ([`e624006`](https://github.com/agent-of-empires/agent-of-empires/commit/e6240067bb85d227d2b658c9ab8eaf0dc5d6f42f))
- **theme:** Make the theme a single global preference in [#2031](https://github.com/agent-of-empires/agent-of-empires/pull/2031) by [@njbrake](https://github.com/njbrake) ([`72cdb9d`](https://github.com/agent-of-empires/agent-of-empires/commit/72cdb9dd1a95e7477b5801328622a19a417a8508))
- **tui:** Drop phantom project header when only archived sessions remain in [#2033](https://github.com/agent-of-empires/agent-of-empires/pull/2033) by [@njbrake](https://github.com/njbrake) ([`73f9677`](https://github.com/agent-of-empires/agent-of-empires/commit/73f96778cc4b4fb207e311cd14517518134052d0))
- **test:** Gate harness helpers behind serve feature in [#2034](https://github.com/agent-of-empires/agent-of-empires/pull/2034) by [@jerome-benoit](https://github.com/jerome-benoit) ([`a77c61a`](https://github.com/agent-of-empires/agent-of-empires/commit/a77c61a416d827968881b355b9cc04fdc28ac0ad))
- **web:** Keep in-progress profile description edit across the async load by [@njbrake](https://github.com/njbrake) ([`6152cf6`](https://github.com/agent-of-empires/agent-of-empires/commit/6152cf66431401ea8eab282151ffd38bdd544f7b))
- **sandbox:** Skip glob-like volume_ignores entries instead of mounting them literally in [#2037](https://github.com/agent-of-empires/agent-of-empires/pull/2037) by [@njbrake](https://github.com/njbrake) ([`556175c`](https://github.com/agent-of-empires/agent-of-empires/commit/556175c46ddfe5bd1c40ed92279a9173b4eeff8c))
- **hooks:** Honor agent config-dir env vars when installing status hooks in [#2038](https://github.com/agent-of-empires/agent-of-empires/pull/2038) by [@uzi](https://github.com/uzi) ([`21983cd`](https://github.com/agent-of-empires/agent-of-empires/commit/21983cd00b7f9e6f773c703bf28c779bd8bcfde1))
- **web:** Drop orphaned split-diff files left over from #1969 rewrite by [@njbrake](https://github.com/njbrake) ([`a0c4dc1`](https://github.com/agent-of-empires/agent-of-empires/commit/a0c4dc1a5a3d324ddd583fd33e749e3e9d7058b2))
- **lint:** Remove frontend ESLint warnings and max-warnings allowance in [#2040](https://github.com/agent-of-empires/agent-of-empires/pull/2040) by [@Eric162](https://github.com/Eric162) ([`0e26acd`](https://github.com/agent-of-empires/agent-of-empires/commit/0e26acd48f441ec12e908f3cb3f39c33b18fc4d0))
- **hooks:** Ignore empty CODEX_HOME when resolving codex config path in [#2043](https://github.com/agent-of-empires/agent-of-empires/pull/2043) by [@uzi](https://github.com/uzi) ([`4918208`](https://github.com/agent-of-empires/agent-of-empires/commit/49182086e3a6ddefcaa6b762019dd947a69e891b))
- **build:** Resolve git HEAD/index via rev-parse so worktree builds cache in [#2050](https://github.com/agent-of-empires/agent-of-empires/pull/2050) by [@njbrake](https://github.com/njbrake) ([`5717203`](https://github.com/agent-of-empires/agent-of-empires/commit/571720355de5a2523fa00af1fe592f666796f46b))
- **tui:** Exit live mode when a select-only click moves to another session in [#2057](https://github.com/agent-of-empires/agent-of-empires/pull/2057) by [@njbrake](https://github.com/njbrake) ([`fe1b776`](https://github.com/agent-of-empires/agent-of-empires/commit/fe1b776437278a7ba2059c5f64b361d2011b1930))
- **acp:** Re-adopt live orphan runners so a failed handshake self-heals in [#2053](https://github.com/agent-of-empires/agent-of-empires/pull/2053) by [@njbrake](https://github.com/njbrake) ([`3e14345`](https://github.com/agent-of-empires/agent-of-empires/commit/3e14345944d56450b3908adba9462cb29c167420))
- **server:** Drop stale last_error on healthy instance after recovery in [#2060](https://github.com/agent-of-empires/agent-of-empires/pull/2060) by [@njbrake](https://github.com/njbrake) ([`5f6a052`](https://github.com/agent-of-empires/agent-of-empires/commit/5f6a052aa7489a8ddf2da79e762279e20ba25e8d))
- **tui:** Unpin a project from every scope so empty headers clear in [#2059](https://github.com/agent-of-empires/agent-of-empires/pull/2059) by [@njbrake](https://github.com/njbrake) ([`f2663ab`](https://github.com/agent-of-empires/agent-of-empires/commit/f2663abcc2cda9dfaa093336bf1d1d25e320088d))
- **server:** Refresh recovery suppression marks so queued candidates don't age out in [#2061](https://github.com/agent-of-empires/agent-of-empires/pull/2061) by [@njbrake](https://github.com/njbrake) ([`a08ed77`](https://github.com/agent-of-empires/agent-of-empires/commit/a08ed776524c229c217112fc4e778e645738e5c6))
- **tui:** Boot live-send agent pane at the visible size to kill the resize race in [#2064](https://github.com/agent-of-empires/agent-of-empires/pull/2064) by [@njbrake](https://github.com/njbrake) ([`178be86`](https://github.com/agent-of-empires/agent-of-empires/commit/178be86871839f5901251fab992b76ec1fd68b0c))
- **web:** Run on_create hooks for sessions created via the web API in [#2069](https://github.com/agent-of-empires/agent-of-empires/pull/2069) by [@njbrake](https://github.com/njbrake) ([`ced38b8`](https://github.com/agent-of-empires/agent-of-empires/commit/ced38b8d20d6941173fd566a56e65cb00110376f))
- **tui:** Keep the pull-image banner from clobbering itself mid-pull in [#2073](https://github.com/agent-of-empires/agent-of-empires/pull/2073) by [@njbrake](https://github.com/njbrake) ([`5f372fd`](https://github.com/agent-of-empires/agent-of-empires/commit/5f372fde38939c67a7a86011fdc6b0d6a24639d4))
- **theme:** Align theme color projection in [#2074](https://github.com/agent-of-empires/agent-of-empires/pull/2074) by [@jerome-benoit](https://github.com/jerome-benoit) ([`42e051d`](https://github.com/agent-of-empires/agent-of-empires/commit/42e051d32b0a17fb470947c38096b1270ae16eb9))
- **tui:** Resolve empty project-header pin state by label so stale pins can be cleared in [#2076](https://github.com/agent-of-empires/agent-of-empires/pull/2076) by [@njbrake](https://github.com/njbrake) ([`4e76d89`](https://github.com/agent-of-empires/agent-of-empires/commit/4e76d89ba56fc6d7a0a41a2a3ff4ab8aa7f4d9ab))
- **theme:** Centralize projected dashboard colors in [#2080](https://github.com/agent-of-empires/agent-of-empires/pull/2080) by [@jerome-benoit](https://github.com/jerome-benoit) ([`f253b4e`](https://github.com/agent-of-empires/agent-of-empires/commit/f253b4e4315ee6414b9e5c8af1aed4826695cc1c))
- **tui:** Move settings status into the footer with auto-dismiss in [#2084](https://github.com/agent-of-empires/agent-of-empires/pull/2084) by [@njbrake](https://github.com/njbrake) ([`970bc38`](https://github.com/agent-of-empires/agent-of-empires/commit/970bc38bc0540d00a70b46f39c60a87f8c39156e))
- **web:** Keep the agent prompt visible under the mobile keyboard and keep streaming while reading scrollback in [#2087](https://github.com/agent-of-empires/agent-of-empires/pull/2087) by [@njbrake](https://github.com/njbrake) ([`fb9306b`](https://github.com/agent-of-empires/agent-of-empires/commit/fb9306be253aaa16a0f90f0c99a07de746ef8928))


### Features

- **acp:** Forward agent-native MCP config via live read-through, merged under global in [#1998](https://github.com/agent-of-empires/agent-of-empires/pull/1998) by [@Seluj78](https://github.com/Seluj78) ([`a6348f0`](https://github.com/agent-of-empires/agent-of-empires/commit/a6348f03f85cefc91c025a10db9370721f3628ed))
- **serve:** Persist web login sessions across daemon restart, fix devices page in [#1999](https://github.com/agent-of-empires/agent-of-empires/pull/1999) by [@Seluj78](https://github.com/Seluj78) ([`de5bf9f`](https://github.com/agent-of-empires/agent-of-empires/commit/de5bf9fde33bc96998dba8d4d727a17caf0421bb))
- **build:** Share dependency builds across worktrees via kache in [#2000](https://github.com/agent-of-empires/agent-of-empires/pull/2000) by [@Seluj78](https://github.com/Seluj78) ([`dcaa28c`](https://github.com/agent-of-empires/agent-of-empires/commit/dcaa28cdec4bcfdf4be1fccd3968fd0bac2df190))
- **tui:** Anchor preview selection to scrollback so it spans pages in [#1980](https://github.com/agent-of-empires/agent-of-empires/pull/1980) by [@njbrake](https://github.com/njbrake) ([`d178d06`](https://github.com/agent-of-empires/agent-of-empires/commit/d178d06b126e2deb15d3a11ab521b7e4d3ec31bd))
- **acp:** Per-profile and trusted project-local MCP config layers in [#2001](https://github.com/agent-of-empires/agent-of-empires/pull/2001) by [@Seluj78](https://github.com/Seluj78) ([`a62895a`](https://github.com/agent-of-empires/agent-of-empires/commit/a62895a8de06b39df4f3730fe9b7b9f2bdf78c74))
- **diff:** Render diffs with @pierre/diffs (virtualized, worker-pool highlighting, in-diff find) in [#1969](https://github.com/agent-of-empires/agent-of-empires/pull/1969) by [@Eric162](https://github.com/Eric162) ([`1353293`](https://github.com/agent-of-empires/agent-of-empires/commit/13532939f7e0802a6aa4a9ba7faaedc0165db5dd))
- **worktree:** Tie session title and worktree directory name together in [#1997](https://github.com/agent-of-empires/agent-of-empires/pull/1997) by [@Seluj78](https://github.com/Seluj78) ([`51fe1bd`](https://github.com/agent-of-empires/agent-of-empires/commit/51fe1bd9b8413707f5f2225cc7f6752bb740df57))
- **mcp:** Unified MCP management surface (read model, conflict, keep-on-removal, CLI, web) in [#2006](https://github.com/agent-of-empires/agent-of-empires/pull/2006) by [@Seluj78](https://github.com/Seluj78) ([`f4d7d74`](https://github.com/agent-of-empires/agent-of-empires/commit/f4d7d7411bd594b82e8e75e6013ef0feee77a8eb))
- **file-watch:** Server consumer migration; AppState event-driven storage mirror in [#1739](https://github.com/agent-of-empires/agent-of-empires/pull/1739) by [@jerome-benoit](https://github.com/jerome-benoit) ([`d0bf6b0`](https://github.com/agent-of-empires/agent-of-empires/commit/d0bf6b039fc692c1171e155a5cda403e851b44f3))
- **file-watch:** TUI HomeView event-driven storage reload in [#1740](https://github.com/agent-of-empires/agent-of-empires/pull/1740) by [@jerome-benoit](https://github.com/jerome-benoit) ([`9af7f59`](https://github.com/agent-of-empires/agent-of-empires/commit/9af7f59184ab8ec3674a22d7a3e3c162d2e8d15b))
- **tui:** Configure command override + extra args in the restart dialog in [#2041](https://github.com/agent-of-empires/agent-of-empires/pull/2041) by [@Eric162](https://github.com/Eric162) ([`b19c2b1`](https://github.com/agent-of-empires/agent-of-empires/commit/b19c2b18ebf338c40665b4be652a4eb9514d36cb))
- **web:** Single-source profile-settings write allowlist from the schema in [#2049](https://github.com/agent-of-empires/agent-of-empires/pull/2049) by [@njbrake](https://github.com/njbrake) ([`a700fb1`](https://github.com/agent-of-empires/agent-of-empires/commit/a700fb1a773d2908443385b6ac5622d5ae7b5c82))
- **tui:** Add "New Session" to the project/group right-click menu in [#2051](https://github.com/agent-of-empires/agent-of-empires/pull/2051) by [@njbrake](https://github.com/njbrake) ([`467aa89`](https://github.com/agent-of-empires/agent-of-empires/commit/467aa89c6f23adcce8804efc27da0d01ff27c939))
- **sandbox:** Expand glob volume_ignores at create time with a confirm gate in [#2054](https://github.com/agent-of-empires/agent-of-empires/pull/2054) by [@njbrake](https://github.com/njbrake) ([`92b1710`](https://github.com/agent-of-empires/agent-of-empires/commit/92b17109ec3e6afb0d2b7557b7b465c45114fcff))
- **tui:** Pin a project so it persists without sessions in [#2055](https://github.com/agent-of-empires/agent-of-empires/pull/2055) by [@njbrake](https://github.com/njbrake) ([`d3784a0`](https://github.com/agent-of-empires/agent-of-empires/commit/d3784a09d3ea0018f5c668dc2995f01de8d23b92))
- **archive:** Archive an entire project at once in [#2052](https://github.com/agent-of-empires/agent-of-empires/pull/2052) by [@njbrake](https://github.com/njbrake) ([`83cab6d`](https://github.com/agent-of-empires/agent-of-empires/commit/83cab6d267cb6969dc995cc03267e22b1042053b))
- **tui:** Add Snooze to the session right-click context menu in [#2058](https://github.com/agent-of-empires/agent-of-empires/pull/2058) by [@njbrake](https://github.com/njbrake) ([`2018c2b`](https://github.com/agent-of-empires/agent-of-empires/commit/2018c2b0b051d3502449e53b3d5e90f190d7150f))
- **tui:** Offer to pull a newer sandbox image when one is available in [#2065](https://github.com/agent-of-empires/agent-of-empires/pull/2065) by [@njbrake](https://github.com/njbrake) ([`3541740`](https://github.com/agent-of-empires/agent-of-empires/commit/35417408ab558667880ad279d1b9cf14f0ea6b82))
- **acp:** Bump claude-agent-acp floor to 0.44.0 in [#2077](https://github.com/agent-of-empires/agent-of-empires/pull/2077) by [@Seluj78](https://github.com/Seluj78) ([`39e2a95`](https://github.com/agent-of-empires/agent-of-empires/commit/39e2a953ddeabdc19078149cef487b7d6fe5060b))
- **web:** Stale-PWA update banner, cache headers, and ETag revalidation in [#2079](https://github.com/agent-of-empires/agent-of-empires/pull/2079) by [@njbrake](https://github.com/njbrake) ([`b9ab75d`](https://github.com/agent-of-empires/agent-of-empires/commit/b9ab75d5373737dcf8abbf2f027689ce8b714d36))
- Add bare repository clone option to web dashboard in [#2081](https://github.com/agent-of-empires/agent-of-empires/pull/2081) by [@flpdorea](https://github.com/flpdorea) ([`0b52207`](https://github.com/agent-of-empires/agent-of-empires/commit/0b52207e18416ded34b56ded3cfe1b8f517e55ed))
- **web:** Mobile terminals adopt the TUI's live-mode architecture (capture streaming, native scroll, no PTY) in [#2085](https://github.com/agent-of-empires/agent-of-empires/pull/2085) by [@njbrake](https://github.com/njbrake) ([`21be4f2`](https://github.com/agent-of-empires/agent-of-empires/commit/21be4f2427cdac5b917bb8c7d5555d6900d93bcb))


### Other

- Format files to oxfmt spec by [@Eric162](https://github.com/Eric162) ([`9f94141`](https://github.com/agent-of-empires/agent-of-empires/commit/9f9414197e890eda18b9d914b9c6d9406f8c0fda))
- Enforce prettier in CI by [@Eric162](https://github.com/Eric162) ([`12f4afc`](https://github.com/agent-of-empires/agent-of-empires/commit/12f4afcda5c406d06ca374fd0ce92990debc7f2f))
- Merge pull request #1993 from Eric162/format-web-phase-2-3 in [#1993](https://github.com/agent-of-empires/agent-of-empires/pull/1993) by [@njbrake](https://github.com/njbrake) ([`d81f654`](https://github.com/agent-of-empires/agent-of-empires/commit/d81f654ae238b7951c3791fa708c44be0ea6bc3d))
- Merge pull request #2032 from agent-of-empires/fix/profiles-live-test-flake in [#2032](https://github.com/agent-of-empires/agent-of-empires/pull/2032) by [@njbrake](https://github.com/njbrake) ([`8ac04e4`](https://github.com/agent-of-empires/agent-of-empires/commit/8ac04e43a620cd3d32515be9faad8d61ccb79a53))
- Merge pull request #2024 from Eric162/format-web-phase-4 in [#2024](https://github.com/agent-of-empires/agent-of-empires/pull/2024) by [@njbrake](https://github.com/njbrake) ([`24232ec`](https://github.com/agent-of-empires/agent-of-empires/commit/24232ec8421f3147430fba4d9fec4be6db1d3d3d))


### Performance

- **diff:** Cache contents + dedupe server scan to kill diff-switch lag in [#2042](https://github.com/agent-of-empires/agent-of-empires/pull/2042) by [@Eric162](https://github.com/Eric162) ([`6d604ad`](https://github.com/agent-of-empires/agent-of-empires/commit/6d604ad95c1533df2db21c4f0de0cb8c33b9b2b6))



### New Contributors

- [@blacksmith-sh[bot]](https://github.com/blacksmith-sh[bot]) made their first contribution in [#2063](https://github.com/agent-of-empires/agent-of-empires/pull/2063)
- [@uzi](https://github.com/uzi) made their first contribution in [#2043](https://github.com/agent-of-empires/agent-of-empires/pull/2043)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.10.1...v1.11.0
## [1.10.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.10.1) - 2026-06-05



### Bug Fixes

- Preserve inherited sandbox env resolution in [#1919](https://github.com/agent-of-empires/agent-of-empires/pull/1919) by [@njbrake](https://github.com/njbrake) ([`5e146f2`](https://github.com/agent-of-empires/agent-of-empires/commit/5e146f2aff260f2afc90da0fe8ba6c87343aeb2b))
- **cockpit:** Open transcript markdown links in a new tab by [@Seluj78](https://github.com/Seluj78) ([`47f2ef1`](https://github.com/agent-of-empires/agent-of-empires/commit/47f2ef1b687334f627759dcf4b0d256b70dd56f7))
- **cockpit:** Keep mobile composer footer actions reachable on narrow viewports by [@Seluj78](https://github.com/Seluj78) ([`d7d814e`](https://github.com/agent-of-empires/agent-of-empires/commit/d7d814eed61aa01033bafe029d25ff74250d84ae))
- **cockpit:** Clamp inlined tool label in working spinner by [@Seluj78](https://github.com/Seluj78) ([`b15e86f`](https://github.com/agent-of-empires/agent-of-empires/commit/b15e86f95b9b41ead24ad3d1d118b148a0748b59))
- **cockpit:** Show file path and structured diffs on Codex edit cards by [@Seluj78](https://github.com/Seluj78) ([`b9cba1f`](https://github.com/agent-of-empires/agent-of-empires/commit/b9cba1fffd685ad17769c0e7b00d397e3f2929ca))
- **cockpit:** Gemini permissioned tools, fix null args and missing tool cards by [@Seluj78](https://github.com/Seluj78) ([`ecd0b80`](https://github.com/agent-of-empires/agent-of-empires/commit/ecd0b802ccc8c0ef30dab3cce22416ca4b32436b))
- **web:** Add sidebar separator borders and align the sort tooltip style by [@Seluj78](https://github.com/Seluj78) ([`b1ccb50`](https://github.com/agent-of-empires/agent-of-empires/commit/b1ccb504214657cdfe7b40ac9fc4ef5a38e76094))
- **web:** Dedupe new-session wizard Recent projects on trailing slash by [@Seluj78](https://github.com/Seluj78) ([`0633956`](https://github.com/agent-of-empires/agent-of-empires/commit/0633956676d3fc01c7f9f66e83432cbc84e7c3e0))
- **cockpit:** Queue image attachments instead of dropping them mid-turn by [@Seluj78](https://github.com/Seluj78) ([`745bd6d`](https://github.com/agent-of-empires/agent-of-empires/commit/745bd6d1a379435c17561efc3bee743b6429089a))
- **web:** Stop diff comments writing empty localStorage keys and sweep orphans by [@Seluj78](https://github.com/Seluj78) ([`61b1911`](https://github.com/agent-of-empires/agent-of-empires/commit/61b19111f14a7cfcc0ef17d3dd7e1d8c0adbad79))
- **cockpit:** Recover end-of-turn wedge on background work and explain cockpit cancel by [@Seluj78](https://github.com/Seluj78) ([`c48d3f4`](https://github.com/agent-of-empires/agent-of-empires/commit/c48d3f4bb70abbf0549226fd27b8658c70d6e0cd))
- **test:** Unwrap hook_status_dir Result in urgent-flag test by [@Seluj78](https://github.com/Seluj78) ([`55bcb1f`](https://github.com/agent-of-empires/agent-of-empires/commit/55bcb1f019e23ff05a82881575381d14a6da4400))
- **session:** Upsert into the live registry on create to avoid duplicate ids by [@Seluj78](https://github.com/Seluj78) ([`f45bf4e`](https://github.com/agent-of-empires/agent-of-empires/commit/f45bf4e7030c1105368e900e2470702a1cdd2147))
- **cockpit:** Skip session/set_mode for modes the agent has not advertised by [@Seluj78](https://github.com/Seluj78) ([`9192539`](https://github.com/agent-of-empires/agent-of-empires/commit/9192539cc85216955778eabac09058224ba7ff7d))
- **cockpit:** Respect agent_command_override in aoe add --cmd gates by [@Seluj78](https://github.com/Seluj78) ([`071be17`](https://github.com/agent-of-empires/agent-of-empires/commit/071be17be98ee576280fd50a609ae229c80b3ec1))
- **cockpit:** Render opencode todo updates by [@Seluj78](https://github.com/Seluj78) ([`f1b04a9`](https://github.com/agent-of-empires/agent-of-empires/commit/f1b04a9313a550bb6d8de9cad96590726304fedb))
- **settings:** Mark cockpit_defaults #[setting(skip)] for the derived schema in [#1866](https://github.com/agent-of-empires/agent-of-empires/pull/1866) by [@Seluj78](https://github.com/Seluj78) ([`6b8d52e`](https://github.com/agent-of-empires/agent-of-empires/commit/6b8d52eac11146858ae46e31706a9074aa651ef4))
- **telemetry:** Robustness cleanups (RMW lock, single config load, throttle comment, interval jitter) in [#1938](https://github.com/agent-of-empires/agent-of-empires/pull/1938) by [@Seluj78](https://github.com/Seluj78) ([`74f61bf`](https://github.com/agent-of-empires/agent-of-empires/commit/74f61bf390e2f6e108420c5c88949266b992581b))
- **tui:** Forward typed semicolons in live mode in [#1949](https://github.com/agent-of-empires/agent-of-empires/pull/1949) by [@njbrake](https://github.com/njbrake) ([`9976535`](https://github.com/agent-of-empires/agent-of-empires/commit/997653575a26f0155df23f431738f28e1ce3e71f))
- **website:** Rebuild guides landing page and consolidate YouTube embed in [#1956](https://github.com/agent-of-empires/agent-of-empires/pull/1956) by [@njbrake](https://github.com/njbrake) ([`4b19740`](https://github.com/agent-of-empires/agent-of-empires/commit/4b19740b853ca2ef25687e9ed786044a7f3df68a))
- **tmux:** Lower paste-buffer threshold so short pastes use bracketed paste in [#1947](https://github.com/agent-of-empires/agent-of-empires/pull/1947) by [@BTForIT](https://github.com/BTForIT) ([`f75f9a7`](https://github.com/agent-of-empires/agent-of-empires/commit/f75f9a79ed079114d5bf3c4f80055233e8722139))
- **tui:** Peel trailing Enter off paste burst so Submit still fires in [#1944](https://github.com/agent-of-empires/agent-of-empires/pull/1944) by [@BTForIT](https://github.com/BTForIT) ([`6cb2bcc`](https://github.com/agent-of-empires/agent-of-empires/commit/6cb2bccf756abac459d35d45141ba6958dcd8dda))
- **tui:** Let 'j'/'k' reach list picker filters instead of navigating in [#1964](https://github.com/agent-of-empires/agent-of-empires/pull/1964) by [@markphilipp](https://github.com/markphilipp) ([`07e93fd`](https://github.com/agent-of-empires/agent-of-empires/commit/07e93fdbbf41c920da705d71b9de7c1360987782))
- **logging:** Silence no-op filter swaps to break file-watch OOM loop in [#1958](https://github.com/agent-of-empires/agent-of-empires/pull/1958) by [@Seluj78](https://github.com/Seluj78) ([`2cc67d8`](https://github.com/agent-of-empires/agent-of-empires/commit/2cc67d887c3777afbd00262f2b35dc34473fd5b0))
- **cockpit:** Bound the reconciler respawn loop and surface fast worker crashes in [#1955](https://github.com/agent-of-empires/agent-of-empires/pull/1955) by [@Seluj78](https://github.com/Seluj78) ([`ae20a33`](https://github.com/agent-of-empires/agent-of-empires/commit/ae20a3302d991fef9643a021dc69bc5f464dcf75))
- **cockpit:** Self-terminate orphaned runners so dead-daemon agents stop leaking in [#1922](https://github.com/agent-of-empires/agent-of-empires/pull/1922) by [@Seluj78](https://github.com/Seluj78) ([`a571db4`](https://github.com/agent-of-empires/agent-of-empires/commit/a571db4ba298dd7cf623d397c69d8e62db4396d0))
- **serve:** Require the default_base_branch key on PATCH /api/projects by [@Seluj78](https://github.com/Seluj78) ([`fabc2ef`](https://github.com/agent-of-empires/agent-of-empires/commit/fabc2eff541b82af71cb27de958aee78312f57ae))
- **web:** Lock project form and row actions while a save is in flight by [@Seluj78](https://github.com/Seluj78) ([`2344fd6`](https://github.com/agent-of-empires/agent-of-empires/commit/2344fd6393b3f53a0dc495d8447f00d4d4edd2af))
- **test:** Use a valid Playwright locator in the projects-edit spec by [@Seluj78](https://github.com/Seluj78) ([`6313d99`](https://github.com/agent-of-empires/agent-of-empires/commit/6313d99ea15243445b244997079f36aac14dfe39))
- **tmux:** Show Waiting when Claude is blocked on an approval prompt (#1913) in [#1981](https://github.com/agent-of-empires/agent-of-empires/pull/1981) by [@njbrake](https://github.com/njbrake) ([`f24d14a`](https://github.com/agent-of-empires/agent-of-empires/commit/f24d14afddfca5a59bee92e770c316e1fcae6571))


### Features

- **web:** Show rate-limited indicator on sidebar session rows by [@Seluj78](https://github.com/Seluj78) ([`fb1d2a1`](https://github.com/agent-of-empires/agent-of-empires/commit/fb1d2a1b206b2061a9b4032f81630d7d87e98b2e))
- **cockpit:** Respawn build-stale cockpit workers after aoe update by [@Seluj78](https://github.com/Seluj78) ([`f210afa`](https://github.com/agent-of-empires/agent-of-empires/commit/f210afa6361a57a46cc2e530b96aad93c31d5a81))
- **cockpit:** Opt-in auto-resume after rate-limit reset by [@Seluj78](https://github.com/Seluj78) ([`d8afe8e`](https://github.com/agent-of-empires/agent-of-empires/commit/d8afe8e9f07327ef40c0509c02cb10743e409749))
- **worktree:** Edit a session's workdir name after creation by [@Seluj78](https://github.com/Seluj78) ([`7886271`](https://github.com/agent-of-empires/agent-of-empires/commit/788627187588102ec4272eab3c9261005f4d4612))
- **web:** Nested repo+group sidebar grouping mode by [@Seluj78](https://github.com/Seluj78) ([`b925377`](https://github.com/agent-of-empires/agent-of-empires/commit/b925377a6421d010bd9f7db41398399d09504891))
- **web:** Edit an existing session's group from the sidebar by [@Seluj78](https://github.com/Seluj78) ([`d81c325`](https://github.com/agent-of-empires/agent-of-empires/commit/d81c3251c519bae62e59ddf92c2b6bd07f4f4a8e))
- **web:** Sidebar multi-select for bulk pin, archive, and snooze by [@Seluj78](https://github.com/Seluj78) ([`9fea4d1`](https://github.com/agent-of-empires/agent-of-empires/commit/9fea4d1fcc6f546aec22e3c17a944f4ec566e369))
- **cockpit:** Open transcript path:line file links in the in-app viewer by [@Seluj78](https://github.com/Seluj78) ([`09109a6`](https://github.com/agent-of-empires/agent-of-empires/commit/09109a60ecca166a75da43f3f83985e9523fa572))
- **web:** Add attention sort mode to the sidebar by [@Seluj78](https://github.com/Seluj78) ([`257a94a`](https://github.com/agent-of-empires/agent-of-empires/commit/257a94ab59593c398163a5eab05c705c50b90e4c))
- **cockpit:** Approval-card command preview and a compact-tools density toggle by [@Seluj78](https://github.com/Seluj78) ([`96fc154`](https://github.com/agent-of-empires/agent-of-empires/commit/96fc1543b0ac7a1091928a9fd975714cbae560d8))
- **serve:** Restart the aoe serve daemon after update by [@Seluj78](https://github.com/Seluj78) ([`13f1ab2`](https://github.com/agent-of-empires/agent-of-empires/commit/13f1ab2cb0ed371f8ed685e5b04bc19ff8866441))
- **cockpit:** Honor agent_command_override on cockpit spawn and preview it in the wizard by [@Seluj78](https://github.com/Seluj78) ([`0bbc4c2`](https://github.com/agent-of-empires/agent-of-empires/commit/0bbc4c2c7c3b354df63ef3d91ecab74dd6d69bb4))
- **web:** Show empty-state hint in sidebar when no sessions by [@Seluj78](https://github.com/Seluj78) ([`3546188`](https://github.com/agent-of-empires/agent-of-empires/commit/354618871653990f78fdd5db9995239ca662a1d2))
- **web:** Pick a theme during first-run onboarding by [@Seluj78](https://github.com/Seluj78) ([`e1eaf1c`](https://github.com/agent-of-empires/agent-of-empires/commit/e1eaf1c407f33a2a84061d7c096f73f04ce0e2df))
- Persist the dashboard first-run tour "seen" flag in the backend by [@Seluj78](https://github.com/Seluj78) ([`6c1364a`](https://github.com/agent-of-empires/agent-of-empires/commit/6c1364a57fecb56619ab4d7789857ac5248f29fa))
- **cockpit:** Add configurable OpenCode defaults by [@Seluj78](https://github.com/Seluj78) ([`6f0cf23`](https://github.com/agent-of-empires/agent-of-empires/commit/6f0cf235aac0c196f57e54e33ccb466f37e9b8d0))
- **cli:** Add --interactive session-name prompt to aoe add by [@Seluj78](https://github.com/Seluj78) ([`1cd329c`](https://github.com/agent-of-empires/agent-of-empires/commit/1cd329c8f1983d02d6d334ce686c27899d4638ad))
- **cockpit:** Show and edit the resolved launch command in the new-session wizard by [@Seluj78](https://github.com/Seluj78) ([`537bfa3`](https://github.com/agent-of-empires/agent-of-empires/commit/537bfa3869779c2e9e7471f4288103281a7a6305))
- **tui:** Focus title field for New from selection in [#1928](https://github.com/agent-of-empires/agent-of-empires/pull/1928) by [@Eric162](https://github.com/Eric162) ([`cd4aaae`](https://github.com/agent-of-empires/agent-of-empires/commit/cd4aaaef199f8fc27efad75a9090b64487088bae))
- **telemetry:** Capture serve deployment mode (auth + exposure) in [#1941](https://github.com/agent-of-empires/agent-of-empires/pull/1941) by [@Seluj78](https://github.com/Seluj78) ([`0fc1962`](https://github.com/agent-of-empires/agent-of-empires/commit/0fc19626f4b1c330126886a26b394b452dbb44e2))
- **telemetry:** Add mutually-exclusive session substrate census in [#1930](https://github.com/agent-of-empires/agent-of-empires/pull/1930) by [@Seluj78](https://github.com/Seluj78) ([`d093f4c`](https://github.com/agent-of-empires/agent-of-empires/commit/d093f4cdbbac15bf30286ab6f8eace90512d2aca))
- **telemetry:** Census of pinned/snoozed/archived sessions in usage_snapshot in [#1931](https://github.com/agent-of-empires/agent-of-empires/pull/1931) by [@Seluj78](https://github.com/Seluj78) ([`65a2fc7`](https://github.com/agent-of-empires/agent-of-empires/commit/65a2fc746f2419df6e569a225941c62622479d56))
- **telemetry:** Replace bespoke seen signals with an allowlisted usage-signal registry in [#1932](https://github.com/agent-of-empires/agent-of-empires/pull/1932) by [@Seluj78](https://github.com/Seluj78) ([`003b069`](https://github.com/agent-of-empires/agent-of-empires/commit/003b0694ca89a4acd3bde25a7cc1030160f48b5d))
- **telemetry:** Track which CLI subcommands run via cli_usage in [#1933](https://github.com/agent-of-empires/agent-of-empires/pull/1933) by [@Seluj78](https://github.com/Seluj78) ([`b8f011e`](https://github.com/agent-of-empires/agent-of-empires/commit/b8f011e0772a42cc4347fd6e75efb7b94e65082a))
- **telemetry:** Report data-schema version and update staleness in [#1934](https://github.com/agent-of-empires/agent-of-empires/pull/1934) by [@Seluj78](https://github.com/Seluj78) ([`3aa42f3`](https://github.com/agent-of-empires/agent-of-empires/commit/3aa42f3f2f9832e5a31988c9984355e473bfedfe))
- **web:** Enforce eslint in CI and add you-might-not-need-an-effect in [#1940](https://github.com/agent-of-empires/agent-of-empires/pull/1940) by [@Eric162](https://github.com/Eric162) ([`1da69d0`](https://github.com/agent-of-empires/agent-of-empires/commit/1da69d0b4e98f492012acad3295ff41f7f65ce48))
- **telemetry:** Per-event uuid idempotency key and model-family upkeep docs in [#1935](https://github.com/agent-of-empires/agent-of-empires/pull/1935) by [@Seluj78](https://github.com/Seluj78) ([`7a79b15`](https://github.com/agent-of-empires/agent-of-empires/commit/7a79b151fce90a9ce12c058fb7b97e9693a42cd2))
- **telemetry:** Capture client form-factor (desktop / mobile / PWA) on the seen ping in [#1936](https://github.com/agent-of-empires/agent-of-empires/pull/1936) by [@Seluj78](https://github.com/Seluj78) ([`91e5bc3`](https://github.com/agent-of-empires/agent-of-empires/commit/91e5bc3eacbedc7d6248887cf27212fef74827ff))
- **telemetry:** Time-aggregated serve usage snapshots (sample 30m, send 4h) in [#1939](https://github.com/agent-of-empires/agent-of-empires/pull/1939) by [@Seluj78](https://github.com/Seluj78) ([`a42d969`](https://github.com/agent-of-empires/agent-of-empires/commit/a42d9695d0c5a4546ff1994c8db1a29fc8357619))
- **telemetry:** Instrument diff/comments/terminal usage signals in [#1946](https://github.com/agent-of-empires/agent-of-empires/pull/1946) by [@Seluj78](https://github.com/Seluj78) ([`b739c4f`](https://github.com/agent-of-empires/agent-of-empires/commit/b739c4f2b504fd2b3e24042c9c2ba9c9e676549f))
- **cockpit:** Telemetry for cockpit interaction depth (approvals, switches, plan mode, queued prompts) in [#1937](https://github.com/agent-of-empires/agent-of-empires/pull/1937) by [@Seluj78](https://github.com/Seluj78) ([`284df22`](https://github.com/agent-of-empires/agent-of-empires/commit/284df2285cf774e034634af9794a6000b6d119d1))
- Add default base branch for new worktrees in [#1943](https://github.com/agent-of-empires/agent-of-empires/pull/1943) by [@njbrake](https://github.com/njbrake) ([`94b1f64`](https://github.com/agent-of-empires/agent-of-empires/commit/94b1f64f66cf76f39e1692f3d507e2606138f05d))
- **acp:** Retire cockpit, make the structured view the web dashboard default in [#1925](https://github.com/agent-of-empires/agent-of-empires/pull/1925) by [@njbrake](https://github.com/njbrake) ([`b9fcbfa`](https://github.com/agent-of-empires/agent-of-empires/commit/b9fcbfae7cfc8a7d0f22fd27ce00d22538f8637f))
- **config:** Support the XDG config path on macOS (opt-in, no forced move) in [#1968](https://github.com/agent-of-empires/agent-of-empires/pull/1968) by [@njbrake](https://github.com/njbrake) ([`83e2a54`](https://github.com/agent-of-empires/agent-of-empires/commit/83e2a5419b4ae428dd041901e56833c73ec44038))
- **worktree:** Honor per-project default base branch for the launch repo by [@Seluj78](https://github.com/Seluj78) ([`79d3a1a`](https://github.com/agent-of-empires/agent-of-empires/commit/79d3a1a4a529eded47adb224e40c4a56457b96bc))
- **serve:** Add PATCH /api/projects/{name} to edit a project's base branch by [@Seluj78](https://github.com/Seluj78) ([`8f6996a`](https://github.com/agent-of-empires/agent-of-empires/commit/8f6996a6b0474b15d62d6041d40a586f783ddbee))
- **web:** Edit a project's default base branch from the Projects view by [@Seluj78](https://github.com/Seluj78) ([`171a235`](https://github.com/agent-of-empires/agent-of-empires/commit/171a2354b631ce0c8eb830139b67c942a1e988cd))
- **web:** Edit projects in a modal with a labeled, explained base-branch field by [@Seluj78](https://github.com/Seluj78) ([`b293fa5`](https://github.com/agent-of-empires/agent-of-empires/commit/b293fa54ef67808dd2f8d8c45ae822ba19a301fc))
- **diff:** Default diff comparison to the worktree base branch in [#1978](https://github.com/agent-of-empires/agent-of-empires/pull/1978) by [@Seluj78](https://github.com/Seluj78) ([`6053770`](https://github.com/agent-of-empires/agent-of-empires/commit/6053770dc4990a966cd3c96e8097ff11fcb4a34f))
- **web:** Model channel, Gemini modes, media payloads, approval + TUI fixes in [#1929](https://github.com/agent-of-empires/agent-of-empires/pull/1929) by [@Seluj78](https://github.com/Seluj78) ([`a85a88b`](https://github.com/agent-of-empires/agent-of-empires/commit/a85a88be3aa540d25a6d1824fa9077f5de230d90))
- **xtask:** Optional --watch flag for cargo xtask dev to auto-rebuild and restart the backend in [#1983](https://github.com/agent-of-empires/agent-of-empires/pull/1983) by [@Seluj78](https://github.com/Seluj78) ([`3d701c3`](https://github.com/agent-of-empires/agent-of-empires/commit/3d701c3d6938f799e688995792c5020a77df50b4))
- **web:** Add oxfmt formatter (phase 1 of 3 — config + disabled CI) in [#1966](https://github.com/agent-of-empires/agent-of-empires/pull/1966) by [@Eric162](https://github.com/Eric162) ([`b318482`](https://github.com/agent-of-empires/agent-of-empires/commit/b318482ed0adce65301d3ba26d028fea4b3fd2cb))
- **serve:** Tee session-scoped tracing into per-session acp worker logs in [#1988](https://github.com/agent-of-empires/agent-of-empires/pull/1988) by [@Seluj78](https://github.com/Seluj78) ([`a23fc75`](https://github.com/agent-of-empires/agent-of-empires/commit/a23fc75a231ad8506c6cb393c93fd2a7ed2da501))
- **settings:** Migrate all web settings sections to the schema-driven renderer in [#1987](https://github.com/agent-of-empires/agent-of-empires/pull/1987) by [@Seluj78](https://github.com/Seluj78) ([`2a3fe08`](https://github.com/agent-of-empires/agent-of-empires/commit/2a3fe0884c9e7334285fad525402078bf32fb83f))
- **acp:** Forward configured MCP servers to agents via newSession/loadSession in [#1984](https://github.com/agent-of-empires/agent-of-empires/pull/1984) by [@Seluj78](https://github.com/Seluj78) ([`a7dd956`](https://github.com/agent-of-empires/agent-of-empires/commit/a7dd956666b11a3d16637cf9ca7dc8736e457e2a))


### Other

- Merge pull request #1973 from agent-of-empires/dependabot/npm_and_yarn/acp-worker/aoe-agent/ai-sdk-4480d72800 in [#1973](https://github.com/agent-of-empires/agent-of-empires/pull/1973) by [@njbrake](https://github.com/njbrake) ([`3729665`](https://github.com/agent-of-empires/agent-of-empires/commit/3729665d06cfa7e379051feb2550e785a5f4753d))
- Merge pull request #1979 from agent-of-empires/default-branch-in-project-viewer-editor in [#1979](https://github.com/agent-of-empires/agent-of-empires/pull/1979) by [@njbrake](https://github.com/njbrake) ([`79c4093`](https://github.com/agent-of-empires/agent-of-empires/commit/79c40939604528dc07ee61e5343d08b347c8c9ac))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.10.0...v1.10.1
## [1.10.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.10.0) - 2026-06-03



### Bug Fixes

- **cockpit:** Bump claude-agent-acp floor to 0.39.0 in [#1607](https://github.com/agent-of-empires/agent-of-empires/pull/1607) by [@Seluj78](https://github.com/Seluj78) ([`b841cb5`](https://github.com/agent-of-empires/agent-of-empires/commit/b841cb517c2d8fb81c91dbe463fbc5b5b4415c59))
- **tui:** Group scratch sessions under "scratch" in project sort in [#1618](https://github.com/agent-of-empires/agent-of-empires/pull/1618) by [@njbrake](https://github.com/njbrake) ([`777e31a`](https://github.com/agent-of-empires/agent-of-empires/commit/777e31a95f2faeb404201a8f239d497ec74377e8))
- **tui:** Remove FORCE_COLOR override from agent launch env in [#1619](https://github.com/agent-of-empires/agent-of-empires/pull/1619) by [@kafai-lam](https://github.com/kafai-lam) ([`6c1c2a7`](https://github.com/agent-of-empires/agent-of-empires/commit/6c1c2a71b4ae1dce614652cabf3bca2e1342b095))
- **tui:** Keep the agent preview from clipping; single-source the preview geometry in [#1621](https://github.com/agent-of-empires/agent-of-empires/pull/1621) by [@njbrake](https://github.com/njbrake) ([`0f2ce94`](https://github.com/agent-of-empires/agent-of-empires/commit/0f2ce94c0db4e58d698affb0ccbe0bda56b9692d))
- Horizontal scroll for long path inputs in TUI in [#1634](https://github.com/agent-of-empires/agent-of-empires/pull/1634) by [@handiism](https://github.com/handiism) ([`4a61e83`](https://github.com/agent-of-empires/agent-of-empires/commit/4a61e83043e31a13e329568555de360b6ee5fd65))
- Honor repo config sandbox.default_image in aoe add in [#1658](https://github.com/agent-of-empires/agent-of-empires/pull/1658) by [@markphilipp](https://github.com/markphilipp) ([`ce67c52`](https://github.com/agent-of-empires/agent-of-empires/commit/ce67c52b63f3dc46945aa0ed54c7e0a9518902bf))
- Show full merged hook list before running hooks in [#1655](https://github.com/agent-of-empires/agent-of-empires/pull/1655) by [@markphilipp](https://github.com/markphilipp) ([`6c04ba3`](https://github.com/agent-of-empires/agent-of-empires/commit/6c04ba328459f69b09b4fdad5e5b6e6bafe9c5b1))
- **tui:** Run session stop on a background thread in [#1624](https://github.com/agent-of-empires/agent-of-empires/pull/1624) by [@njbrake](https://github.com/njbrake) ([`6c0e78a`](https://github.com/agent-of-empires/agent-of-empires/commit/6c0e78a3d8c9be347ed6213606375578a5ba32b5))
- **cockpit:** Scroll long diff lines in Edit/Write tool card on mobile by [@Seluj78](https://github.com/Seluj78) ([`e785b80`](https://github.com/agent-of-empires/agent-of-empires/commit/e785b8095806e709b0dbc866c14c4caec7c29251))
- **cockpit:** Make ConfigOptionCategory forward-compatible on the TS and Rust sides by [@Seluj78](https://github.com/Seluj78) ([`e56c53f`](https://github.com/agent-of-empires/agent-of-empires/commit/e56c53f09f47f905e1f0fb4c4f541dd8fd9380fe))
- **server:** Demote loopback bypass logs to debug by [@Seluj78](https://github.com/Seluj78) ([`5278f5a`](https://github.com/agent-of-empires/agent-of-empires/commit/5278f5a41f3917f710077b540d10c5614577b465))
- **web:** Hide multi-repo workspaces from wizard Recent projects list by [@Seluj78](https://github.com/Seluj78) ([`a21b3fc`](https://github.com/agent-of-empires/agent-of-empires/commit/a21b3fc773b48ef773d440b192e5456992330c59))
- **cockpit:** Bound expanded queued and rejected prompt rows by [@Seluj78](https://github.com/Seluj78) ([`fb7237c`](https://github.com/agent-of-empires/agent-of-empires/commit/fb7237c44885295499d9dda3e595893e6426cee3))
- **serve:** Surface real create-session errors on the web dashboard by [@Seluj78](https://github.com/Seluj78) ([`216b30d`](https://github.com/agent-of-empires/agent-of-empires/commit/216b30df163292893b1d64948202d5cbb1fb480d))
- **cockpit:** Settle tool card to a terminal state when stopped mid-execution in [#1666](https://github.com/agent-of-empires/agent-of-empires/pull/1666) by [@Seluj78](https://github.com/Seluj78) ([`2c3f897`](https://github.com/agent-of-empires/agent-of-empires/commit/2c3f897b67c5ab8fbf9e6f37a876632958c1d939))
- **session:** Close cross-instance race on Claude session-id capture in [#1735](https://github.com/agent-of-empires/agent-of-empires/pull/1735) by [@jerome-benoit](https://github.com/jerome-benoit) ([`28dbb3b`](https://github.com/agent-of-empires/agent-of-empires/commit/28dbb3b5a8a010495c6d0e616fa9ec9612f84c99))
- **tui:** Chunk large live-send pastes so they don't overflow ARG_MAX in [#1761](https://github.com/agent-of-empires/agent-of-empires/pull/1761) by [@njbrake](https://github.com/njbrake) ([`449a051`](https://github.com/agent-of-empires/agent-of-empires/commit/449a05124be2f460157274ea36a0df84a8a3be2d))
- **session:** Split agent_session_id into observation and resume intent in [#1731](https://github.com/agent-of-empires/agent-of-empires/pull/1731) by [@jerome-benoit](https://github.com/jerome-benoit) ([`9e20052`](https://github.com/agent-of-empires/agent-of-empires/commit/9e20052990012fd2e2671de5c4263e0a0433afe8))
- **session:** Wipe hook sidecar in cascade cleanup + tighten sidecar tests and docs in [#1765](https://github.com/agent-of-empires/agent-of-empires/pull/1765) by [@jerome-benoit](https://github.com/jerome-benoit) ([`76f1922`](https://github.com/agent-of-empires/agent-of-empires/commit/76f1922082925971c37b6f30a992159e61000a22))
- **tui:** Highlight delete-dialog checkbox rows + guard stale hover rects in [#1779](https://github.com/agent-of-empires/agent-of-empires/pull/1779) by [@njbrake](https://github.com/njbrake) ([`489a76a`](https://github.com/agent-of-empires/agent-of-empires/commit/489a76a6341a03eec419f0f64d4fcceac225cabe))
- **web:** Stop settings fold collapsing on initial profile resolution by [@Seluj78](https://github.com/Seluj78) ([`5625cad`](https://github.com/agent-of-empires/agent-of-empires/commit/5625cadc2e091169a0028f5fa91fb0324025b9e5))
- **cockpit:** Show tool state in WorkingSpinner during tool execution by [@Seluj78](https://github.com/Seluj78) ([`ffc5b64`](https://github.com/agent-of-empires/agent-of-empires/commit/ffc5b64c39f601dff3bb399942caa42832110070))
- **web:** Restore terminal select-to-copy via OSC 52 after xterm.js swap by [@Seluj78](https://github.com/Seluj78) ([`6ffb5ff`](https://github.com/agent-of-empires/agent-of-empires/commit/6ffb5ff7cb1975761070d51b65e70ed56edc9d73))
- **cockpit:** Disarm resume-idle watchdog on first inbound notification by [@Seluj78](https://github.com/Seluj78) ([`2848a48`](https://github.com/agent-of-empires/agent-of-empires/commit/2848a48c5fa42031264e64988298f159690795cf))
- **cockpit:** Wake idle-dormant workers from the web composer by [@Seluj78](https://github.com/Seluj78) ([`bd85c25`](https://github.com/agent-of-empires/agent-of-empires/commit/bd85c25ac9647e3c55cf394b74f1011c37c736f1))
- **web:** Align wizard custom-agent Playwright assertion with updated notice copy by [@Seluj78](https://github.com/Seluj78) ([`fd0b829`](https://github.com/agent-of-empires/agent-of-empires/commit/fd0b82926e6cf77fd6b98beaa46fdbbf3689905f))
- **cockpit:** Preserve context across reversible shutdown by [@Seluj78](https://github.com/Seluj78) ([`42f879d`](https://github.com/agent-of-empires/agent-of-empires/commit/42f879da954e7c9795aafa6bdcf2e3cefaaca106))
- **cockpit:** Keep PromptCapabilities durable and propagate attachment write failures by [@Seluj78](https://github.com/Seluj78) ([`0118265`](https://github.com/agent-of-empires/agent-of-empires/commit/0118265d3a23c186d7609887781064b51a6200c7))
- **cockpit:** Resolve idle auto-stop per session profile and recover dormant marker by [@Seluj78](https://github.com/Seluj78) ([`a9cecde`](https://github.com/agent-of-empires/agent-of-empires/commit/a9cecde827d5dbfffca5458df154f5aa92f6729e))
- **server:** Harden cockpit attachment intake and agent validation by [@Seluj78](https://github.com/Seluj78) ([`bf25e25`](https://github.com/agent-of-empires/agent-of-empires/commit/bf25e25fe109b8a81b4fa729f5dfcb59d70f773f))
- **server:** Thread session profile into cockpit spawn regardless of sandboxing by [@Seluj78](https://github.com/Seluj78) ([`a1b546a`](https://github.com/agent-of-empires/agent-of-empires/commit/a1b546a5cfffd176b3a51dd1a8fb46784d76b1b3))
- **web:** Preserve staged attachments when cockpit prompt send fails by [@Seluj78](https://github.com/Seluj78) ([`f4f8670`](https://github.com/agent-of-empires/agent-of-empires/commit/f4f86705e262eced8a23c7618feff38d7521c5c1))
- **web:** Validate diff-comments payload shape before rendering the card by [@Seluj78](https://github.com/Seluj78) ([`4ad3e54`](https://github.com/agent-of-empires/agent-of-empires/commit/4ad3e54f865205ccf3299be7057c5680051f8b94))
- **web:** Reconcile staged attachments on capability change and cap intake before encoding by [@Seluj78](https://github.com/Seluj78) ([`3ec826f`](https://github.com/agent-of-empires/agent-of-empires/commit/3ec826f56b0e5c1154b29006bca3254dbc7c0cb9))
- **web:** Stop infinite re-drain of cockpit prompts rejected with 4xx by [@Seluj78](https://github.com/Seluj78) ([`52563a1`](https://github.com/agent-of-empires/agent-of-empires/commit/52563a149118d08a5f7cfc7f3722c19f508556df))
- **web:** Scope OSC 52 clipboard arm to its own drag by [@Seluj78](https://github.com/Seluj78) ([`b10d0e6`](https://github.com/agent-of-empires/agent-of-empires/commit/b10d0e6eca3a6f32fa6e5229b079522081a62293))
- **web:** Unify wizard ReviewStep cockpit predicate with AgentStep by [@Seluj78](https://github.com/Seluj78) ([`703bb1c`](https://github.com/agent-of-empires/agent-of-empires/commit/703bb1c3f6ccd60e9cfd9959026af600446db5d5))
- **web:** Normalize leading/trailing slashes in sidebar group_path bucketing by [@Seluj78](https://github.com/Seluj78) ([`08dbe52`](https://github.com/agent-of-empires/agent-of-empires/commit/08dbe52d0da934a889d6105b085f57719204f46c))
- **serve:** Persist session triage/notification/diff-base before mutating memory by [@Seluj78](https://github.com/Seluj78) ([`4fc5df7`](https://github.com/agent-of-empires/agent-of-empires/commit/4fc5df79db8e56b460dc8baefe0121462634c9ea))
- **cockpit:** Deliver the first prompt after idle auto-stop instead of dropping it with a 404 by [@Seluj78](https://github.com/Seluj78) ([`32a7e4e`](https://github.com/agent-of-empires/agent-of-empires/commit/32a7e4e74c8d27a7b098ba350582c488635d9365))
- **hooks:** Replace shell session_id extractor with Rust subcommand in [#1769](https://github.com/agent-of-empires/agent-of-empires/pull/1769) by [@jerome-benoit](https://github.com/jerome-benoit) ([`3226c19`](https://github.com/agent-of-empires/agent-of-empires/commit/3226c197e9aed09698bc9c6f484d3de5111aaf20))
- **cockpit:** Surface OpenCode's real modes in the mode picker (#1764) in [#1770](https://github.com/agent-of-empires/agent-of-empires/pull/1770) by [@Seluj78](https://github.com/Seluj78) ([`cea4ffd`](https://github.com/agent-of-empires/agent-of-empires/commit/cea4ffd6a0afe81dc0c5fe69743004e1169c167e))
- Reduce live mode echo latency in [#1829](https://github.com/agent-of-empires/agent-of-empires/pull/1829) by [@njbrake](https://github.com/njbrake) ([`56b1ccd`](https://github.com/agent-of-empires/agent-of-empires/commit/56b1ccd611e7597a0cda57a23201073314a9c7e1))
- **session:** Harden AOE_INSTANCE_ID validation across path-join and shell-interpolation consumers in [#1803](https://github.com/agent-of-empires/agent-of-empires/pull/1803) by [@jerome-benoit](https://github.com/jerome-benoit) ([`7236d5a`](https://github.com/agent-of-empires/agent-of-empires/commit/7236d5af628040c0d078d1cad8cb851c69e759b4))
- **session:** Align tmux env AOE_CAPTURED_SESSION_ID with disk on persist CAS skip in [#1804](https://github.com/agent-of-empires/agent-of-empires/pull/1804) by [@jerome-benoit](https://github.com/jerome-benoit) ([`4d42d85`](https://github.com/agent-of-empires/agent-of-empires/commit/4d42d85f94146f5c85f90eeae8a3790f4324fe80))
- **infra:** Skip pr template check on edited dependabot prs in [#1857](https://github.com/agent-of-empires/agent-of-empires/pull/1857) by [@Seluj78](https://github.com/Seluj78) ([`245fb5f`](https://github.com/agent-of-empires/agent-of-empires/commit/245fb5f713fae135010e4bfcf760d775da09dc02))
- **telemetry:** Dedup the exit usage_snapshot against the boot one by [@njbrake](https://github.com/njbrake) ([`74c2909`](https://github.com/agent-of-empires/agent-of-empires/commit/74c2909d48d1dd779dae7293a240d2ed32726ce2))
- **web:** Handle CRLF line endings in diffPair in [#1893](https://github.com/agent-of-empires/agent-of-empires/pull/1893) by [@Eric162](https://github.com/Eric162) ([`bc0968e`](https://github.com/agent-of-empires/agent-of-empires/commit/bc0968eec938620cb866420ac8c8e710c2c84fec))
- **cockpit:** Clear dormant resume_intent on cockpit_enable in [#1884](https://github.com/agent-of-empires/agent-of-empires/pull/1884) by [@jerome-benoit](https://github.com/jerome-benoit) ([`5c3cd8d`](https://github.com/agent-of-empires/agent-of-empires/commit/5c3cd8dbbfa629d99d1c8dc9716b959bc2c8c689))
- **recovery:** Timeout hung on_launch hook to release recovery lock in [#1872](https://github.com/agent-of-empires/agent-of-empires/pull/1872) by [@jerome-benoit](https://github.com/jerome-benoit) ([`31477e1`](https://github.com/agent-of-empires/agent-of-empires/commit/31477e1780d34af6407dbf5d9cdaaa32e2e57509))
- **sandbox:** Resolve container terminal login shell, add container_shell override in [#1862](https://github.com/agent-of-empires/agent-of-empires/pull/1862) by [@Seluj78](https://github.com/Seluj78) ([`4f4befd`](https://github.com/agent-of-empires/agent-of-empires/commit/4f4befd00423b97d38d198c2b47dce6345864a95))
- **session:** Kill all tmux session kinds on remove and recovery paths in [#1867](https://github.com/agent-of-empires/agent-of-empires/pull/1867) by [@jerome-benoit](https://github.com/jerome-benoit) ([`2d1e555`](https://github.com/agent-of-empires/agent-of-empires/commit/2d1e555eb200b170c5ec6c3772ffebd64dda459b))
- **ci:** Fix weekly release-PR schedule drops and template-check false positive in [#1900](https://github.com/agent-of-empires/agent-of-empires/pull/1900) by [@Seluj78](https://github.com/Seluj78) ([`ec44d00`](https://github.com/agent-of-empires/agent-of-empires/commit/ec44d00f6ceb3a9de752eeda5b762d7d76877566))
- **telemetry:** Correct create counter, send-failure handling, and aggregation accuracy in [#1898](https://github.com/agent-of-empires/agent-of-empires/pull/1898) by [@Seluj78](https://github.com/Seluj78) ([`e280c3e`](https://github.com/agent-of-empires/agent-of-empires/commit/e280c3eea786fc8790bb8c80817c151f88c99114))
- **web:** Activate cockpit_seen telemetry signal in [#1896](https://github.com/agent-of-empires/agent-of-empires/pull/1896) by [@Seluj78](https://github.com/Seluj78) ([`5f4c6d6`](https://github.com/agent-of-empires/agent-of-empires/commit/5f4c6d63b28bd94e3f5f86452b3cbe22cb55b868))
- **session:** Gate RECOVERY_HOOK_TIMEOUT_FLOOR to debug builds in [#1915](https://github.com/agent-of-empires/agent-of-empires/pull/1915) by [@njbrake](https://github.com/njbrake) ([`e7f57ab`](https://github.com/agent-of-empires/agent-of-empires/commit/e7f57ab707f4fc225b7634da65b9666bc802438c))


### Features

- **tui:** New session from saved project picker in [#1608](https://github.com/agent-of-empires/agent-of-empires/pull/1608) by [@markphilipp](https://github.com/markphilipp) ([`90c5624`](https://github.com/agent-of-empires/agent-of-empires/commit/90c56243ac4e5530940770a154749937b939ab4b))
- **tui:** Footer indicator when another aoe TUI is watching in [#1622](https://github.com/agent-of-empires/agent-of-empires/pull/1622) by [@njbrake](https://github.com/njbrake) ([`11cf893`](https://github.com/agent-of-empires/agent-of-empires/commit/11cf89329be9bd6165bbedc0381dffd29fb37eea))
- **sandbox:** Add named volume_ignores_strategy for macOS VirtioFS in [#1652](https://github.com/agent-of-empires/agent-of-empires/pull/1652) by [@Davicittod](https://github.com/Davicittod) ([`2e68d91`](https://github.com/agent-of-empires/agent-of-empires/commit/2e68d91594696fa10fce205fbbd83c248f44d24e))
- **tui:** Expose mouse-capture toggle in Settings in [#1662](https://github.com/agent-of-empires/agent-of-empires/pull/1662) by [@markphilipp](https://github.com/markphilipp) ([`96b45eb`](https://github.com/agent-of-empires/agent-of-empires/commit/96b45eb10edda182b7410a8cdd9883015a0e48e4))
- **tui:** Guard against accidental exit (Ctrl+Q + confirm-before-quit) in [#1665](https://github.com/agent-of-empires/agent-of-empires/pull/1665) by [@njbrake](https://github.com/njbrake) ([`10462e2`](https://github.com/agent-of-empires/agent-of-empires/commit/10462e282510996af3ed681e81dfe125d2fa0930))
- **cockpit:** Surface queued-prompt count on sidebar session rows by [@Seluj78](https://github.com/Seluj78) ([`b504e6b`](https://github.com/agent-of-empires/agent-of-empires/commit/b504e6bed710e893ec59d6d89f1646adb8f02e37))
- **web:** Fold new-session Session step behind Advanced, leave only title visible by [@Seluj78](https://github.com/Seluj78) ([`40512d8`](https://github.com/agent-of-empires/agent-of-empires/commit/40512d8302c85c46ceca37ab10c21e97f5a49bbd))
- **web:** Match TUI settings grouping, hide low-level knobs behind Advanced fold by [@Seluj78](https://github.com/Seluj78) ([`6d4b7a1`](https://github.com/agent-of-empires/agent-of-empires/commit/6d4b7a1e887230cdc5daf02a6c778ff3762f4bf6))
- **web:** First-run interactive tutorial for the dashboard by [@Seluj78](https://github.com/Seluj78) ([`bb4871d`](https://github.com/agent-of-empires/agent-of-empires/commit/bb4871dfd7889ea6593183455446be565520b1a9))
- **web:** Add "New scratch session" to the command palette by [@Seluj78](https://github.com/Seluj78) ([`922d454`](https://github.com/agent-of-empires/agent-of-empires/commit/922d454c93d60a3b362f18a8748b8f7694745078))
- **web:** Drag project headers to reorder sidebar groups by [@Seluj78](https://github.com/Seluj78) ([`b4f8cb8`](https://github.com/agent-of-empires/agent-of-empires/commit/b4f8cb84a8dd68d722d8dbbede6dc1359924642d))
- **sandbox:** Optional SELinux relabel (:z) on bind mounts in [#1683](https://github.com/agent-of-empires/agent-of-empires/pull/1683) by [@alepar](https://github.com/alepar) ([`5c46bda`](https://github.com/agent-of-empires/agent-of-empires/commit/5c46bda7b79cc769e6ad09ed4867f33a94767f8e))
- **github:** Add GitHub client and token-resolution auth foundation in [#1681](https://github.com/agent-of-empires/agent-of-empires/pull/1681) by [@Seluj78](https://github.com/Seluj78) ([`47b76be`](https://github.com/agent-of-empires/agent-of-empires/commit/47b76be0a6089a659bf645e3e08e6a7f2baa5852))
- **tui:** Tmux-style leader + collapsible sidebar in live mode in [#1773](https://github.com/agent-of-empires/agent-of-empires/pull/1773) by [@njbrake](https://github.com/njbrake) ([`a9cc076`](https://github.com/agent-of-empires/agent-of-empires/commit/a9cc0765c16cb1ea9b19f9a03745f41d030ffef6))
- **tui:** Add hover highlighting to confirm-style dialog buttons by [@njbrake](https://github.com/njbrake) ([`5d38b82`](https://github.com/agent-of-empires/agent-of-empires/commit/5d38b82987e1183be82ea830723fc16ca3d01f3d))
- **web:** Measure keystroke-to-echo latency in the dashboard terminal by [@Seluj78](https://github.com/Seluj78) ([`68ed5ea`](https://github.com/agent-of-empires/agent-of-empires/commit/68ed5ea50bbcb558bc5e310682e7e7cf1b00e445))
- **web:** Render user-defined groups as a sidebar axis by [@Seluj78](https://github.com/Seluj78) ([`a504c96`](https://github.com/agent-of-empires/agent-of-empires/commit/a504c9684a8c70eadb227d0157bba5bf2f99015b))
- **cockpit:** Auto-stop idle workers past a configurable timeout by [@Seluj78](https://github.com/Seluj78) ([`05bada3`](https://github.com/agent-of-empires/agent-of-empires/commit/05bada3d0687d710805f2acd850b0447fdd02348))
- **update:** Document shell completions and hint to refresh them on update by [@Seluj78](https://github.com/Seluj78) ([`25ba00f`](https://github.com/agent-of-empires/agent-of-empires/commit/25ba00f1efe681a9a6e076df36727b1102dab19e))
- **cockpit:** First-class event type for diff-comments prompts by [@Seluj78](https://github.com/Seluj78) ([`32859b6`](https://github.com/agent-of-empires/agent-of-empires/commit/32859b68bcfc732187b2664d3e028bd62a66344b))
- **web:** Add per-session cockpit toggle to the session wizard by [@Seluj78](https://github.com/Seluj78) ([`2842021`](https://github.com/agent-of-empires/agent-of-empires/commit/28420214035bc0fc4c9dd9f022c12a8f3ddda057))
- **cockpit:** Real attachment support in composer (image / audio / resource, paste & drop) by [@Seluj78](https://github.com/Seluj78) ([`b1dc7ae`](https://github.com/agent-of-empires/agent-of-empires/commit/b1dc7ae81855c27b0168fc45dc153e57286ea18d))
- **cockpit:** Support custom agents in web cockpit by [@Seluj78](https://github.com/Seluj78) ([`5f8089d`](https://github.com/agent-of-empires/agent-of-empires/commit/5f8089d4274bf7acfc56b6d3700fb80912368f08))
- **cockpit:** Always-available agent switcher (CLI + web) by [@Seluj78](https://github.com/Seluj78) ([`dac94e2`](https://github.com/agent-of-empires/agent-of-empires/commit/dac94e20265cf50f3cc7f269fbdf2a66b87c6f90))
- **cockpit:** Observable, force-stoppable cancel that kills runaway loops by [@Seluj78](https://github.com/Seluj78) ([`3eb47ad`](https://github.com/agent-of-empires/agent-of-empires/commit/3eb47adbd16382ed24a1b1e70d6c8b8d029d3e0e))
- **web:** Move "Switch agent" from composer toolbar to sidebar context menu by [@Seluj78](https://github.com/Seluj78) ([`61b816e`](https://github.com/agent-of-empires/agent-of-empires/commit/61b816e7fcb82cd759f585cc8fcadc0ef6579a42))
- **cockpit:** Render markdown in the TUI transcript by [@Seluj78](https://github.com/Seluj78) ([`5500ca2`](https://github.com/agent-of-empires/agent-of-empires/commit/5500ca23d6b30162e21a07a05afb71e0eff5fc28))
- **cockpit:** Queued-prompt UI above the TUI composer by [@Seluj78](https://github.com/Seluj78) ([`1612e8b`](https://github.com/agent-of-empires/agent-of-empires/commit/1612e8bec54452d24277c37245e74902327629b9))
- **cockpit:** Paginate replay endpoint for large sessions by [@Seluj78](https://github.com/Seluj78) ([`112864d`](https://github.com/agent-of-empires/agent-of-empires/commit/112864d0c441f6fe2f8107eb66e973334e11db2b))
- **cockpit:** Slash-command picker in the TUI composer by [@Seluj78](https://github.com/Seluj78) ([`148ca5d`](https://github.com/agent-of-empires/agent-of-empires/commit/148ca5db1eeaddbaa0b50f3073bbaee6537b890d))
- **cockpit:** Per-kind tool cards in the TUI cockpit transcript by [@Seluj78](https://github.com/Seluj78) ([`6ca8377`](https://github.com/agent-of-empires/agent-of-empires/commit/6ca837790b25d7bf0f1677f55dbe762e8944b18b))
- **tui:** @ file-mention picker in the cockpit composer in [#1733](https://github.com/agent-of-empires/agent-of-empires/pull/1733) by [@Seluj78](https://github.com/Seluj78) ([`eda97b9`](https://github.com/agent-of-empires/agent-of-empires/commit/eda97b90728ca7de22b06f6b855a0a12d3c1ff96))
- **web:** Add VITE_PROXY to point the dev server at any aoe serve in [#1771](https://github.com/agent-of-empires/agent-of-empires/pull/1771) by [@Eric162](https://github.com/Eric162) ([`4f2d29f`](https://github.com/agent-of-empires/agent-of-empires/commit/4f2d29fbd1bc67aaa2dfc7cabbe6b7ce19a99410))
- **file-watch:** Introduce FileWatchService primitive; migrate logging consumer in [#1734](https://github.com/agent-of-empires/agent-of-empires/pull/1734) by [@jerome-benoit](https://github.com/jerome-benoit) ([`38b4264`](https://github.com/agent-of-empires/agent-of-empires/commit/38b4264ec1681b78efea859a5074b2dd7b4c79f7))
- One-command hot-reload dev workflow for the web dashboard (cargo xtask dev) in [#1729](https://github.com/agent-of-empires/agent-of-empires/pull/1729) by [@Seluj78](https://github.com/Seluj78) ([`329d7c0`](https://github.com/agent-of-empires/agent-of-empires/commit/329d7c07489c867e590c2f895e9fc9d7b753cb6e))
- Add split (side-by-side) diff view to web and TUI in [#1806](https://github.com/agent-of-empires/agent-of-empires/pull/1806) by [@peteski22](https://github.com/peteski22) ([`bdcbf60`](https://github.com/agent-of-empires/agent-of-empires/commit/bdcbf60bd618ce76a4ddb991613dd48d61ef6e9d))
- **web:** Dedicated Profiles page with read-only lifecycle hooks in [#1757](https://github.com/agent-of-empires/agent-of-empires/pull/1757) by [@Seluj78](https://github.com/Seluj78) ([`0af36cc`](https://github.com/agent-of-empires/agent-of-empires/commit/0af36cc05c3b0eaeee768ea3aed0f1a31fc4c227))
- Copy a changed file's relative path from the diff file list (web + TUI) in [#1825](https://github.com/agent-of-empires/agent-of-empires/pull/1825) by [@peteski22](https://github.com/peteski22) ([`9ecb48f`](https://github.com/agent-of-empires/agent-of-empires/commit/9ecb48f33232b5cb1b9da29cc8a0427ce53aa761))
- **session:** Auto-stop idle tmux sessions for inactivity in [#1777](https://github.com/agent-of-empires/agent-of-empires/pull/1777) by [@Seluj78](https://github.com/Seluj78) ([`638d312`](https://github.com/agent-of-empires/agent-of-empires/commit/638d3125431a3b78f5cb2f7eef5a6edd39265b31))
- Publish aoe skill to the Hermes Agent Skills Hub in [#1860](https://github.com/agent-of-empires/agent-of-empires/pull/1860) by [@njbrake](https://github.com/njbrake) ([`f50782b`](https://github.com/agent-of-empires/agent-of-empires/commit/f50782b0712edddcfa2d0ff87f3ea09332b9f51c))
- Add anonymous opt-in usage telemetry in [#1863](https://github.com/agent-of-empires/agent-of-empires/pull/1863) by [@njbrake](https://github.com/njbrake) ([`ce1cdd4`](https://github.com/agent-of-empires/agent-of-empires/commit/ce1cdd4f06863e0ffb3070b1abf7757393dd15aa))


### Other

- Merge pull request #1871 from agent-of-empires/fix/telemetry-duplicate-snapshots in [#1871](https://github.com/agent-of-empires/agent-of-empires/pull/1871) by [@njbrake](https://github.com/njbrake) ([`f94fad4`](https://github.com/agent-of-empires/agent-of-empires/commit/f94fad4c2ee53bf7b88444aa1738ea4a1c0ef2c6))


### Performance

- **tui:** Move live-send preview capture off the render thread in [#1775](https://github.com/agent-of-empires/agent-of-empires/pull/1775) by [@njbrake](https://github.com/njbrake) ([`a747137`](https://github.com/agent-of-empires/agent-of-empires/commit/a7471373a0f9729e9a4cebc73cfce51933d3d0ce))
- **tui:** Capture every preview off the render thread, not just the agent in [#1824](https://github.com/agent-of-empires/agent-of-empires/pull/1824) by [@njbrake](https://github.com/njbrake) ([`8425433`](https://github.com/agent-of-empires/agent-of-empires/commit/8425433d8b6fd8361e178812763827f7a5ce3691))



### New Contributors

- [@markphilipp](https://github.com/markphilipp) made their first contribution in [#1662](https://github.com/agent-of-empires/agent-of-empires/pull/1662)
- [@Davicittod](https://github.com/Davicittod) made their first contribution in [#1652](https://github.com/agent-of-empires/agent-of-empires/pull/1652)
- [@handiism](https://github.com/handiism) made their first contribution in [#1634](https://github.com/agent-of-empires/agent-of-empires/pull/1634)
- [@kafai-lam](https://github.com/kafai-lam) made their first contribution in [#1619](https://github.com/agent-of-empires/agent-of-empires/pull/1619)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.9.5...v1.10.0
## [1.9.5](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.9.5) - 2026-05-29



### Bug Fixes

- **tui:** Show top preview row in live mode when info header is hidden in [#1604](https://github.com/agent-of-empires/agent-of-empires/pull/1604) by [@njbrake](https://github.com/njbrake) ([`45bacae`](https://github.com/agent-of-empires/agent-of-empires/commit/45bacae6701dbedbea4b2ac20f02fb510e5de82e))


### Features

- **tui:** Mouse click + hover support across dialogs, settings, diff in [#1593](https://github.com/agent-of-empires/agent-of-empires/pull/1593) by [@njbrake](https://github.com/njbrake) ([`47d5355`](https://github.com/agent-of-empires/agent-of-empires/commit/47d5355353a41d8f7ac01af67cdddf9bf348a1e5))
- **tui:** First-run intro walkthrough with theme + attach-mode picks in [#1605](https://github.com/agent-of-empires/agent-of-empires/pull/1605) by [@njbrake](https://github.com/njbrake) ([`02f404d`](https://github.com/agent-of-empires/agent-of-empires/commit/02f404d588b6597e03b715409ab9e3977f2a1d23))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.9.4...v1.9.5
## [1.9.4](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.9.4) - 2026-05-28



### Bug Fixes

- **tui:** Wrap multiline live-send pastes in bracketed paste markers in [#1553](https://github.com/agent-of-empires/agent-of-empires/pull/1553) by [@njbrake](https://github.com/njbrake) ([`26f5a39`](https://github.com/agent-of-empires/agent-of-empires/commit/26f5a394e0e6e3bc798d995f93b64511718b69be))
- **tui:** Wrap settings descriptions instead of clipping them in [#1552](https://github.com/agent-of-empires/agent-of-empires/pull/1552) by [@njbrake](https://github.com/njbrake) ([`80370ef`](https://github.com/agent-of-empires/agent-of-empires/commit/80370ef47a7f8970542fad6bd74dfe3811c62055))
- **session:** Anchor Claude session poller to its own session id in [#1523](https://github.com/agent-of-empires/agent-of-empires/pull/1523) by [@itisaevalex](https://github.com/itisaevalex) ([`bbceca5`](https://github.com/agent-of-empires/agent-of-empires/commit/bbceca582f500179ae7bfe6c58a2e7a9856a5b44))
- **tui:** Route 'm' and live mode to the paired terminal pane in Terminal view in [#1561](https://github.com/agent-of-empires/agent-of-empires/pull/1561) by [@njbrake](https://github.com/njbrake) ([`ef9ac2d`](https://github.com/agent-of-empires/agent-of-empires/commit/ef9ac2dd5ec2272e8c11a9086f97555f2e2ecfc9))
- **tui:** Prune source profile's empty group after restart-to-different-profile in [#1463](https://github.com/agent-of-empires/agent-of-empires/pull/1463) by [@BTForIT](https://github.com/BTForIT) ([`828bc3a`](https://github.com/agent-of-empires/agent-of-empires/commit/828bc3a4c476fb25d4cf6843f05bf32f252dbaae))
- **tui:** Show correct Enter/Tab labels in help overlay in [#1567](https://github.com/agent-of-empires/agent-of-empires/pull/1567) by [@njbrake](https://github.com/njbrake) ([`ab37c7f`](https://github.com/agent-of-empires/agent-of-empires/commit/ab37c7f59437117f5dcfd74d0186faf50dbc470f))
- **tui:** Size preview pane around info panel + add i toggle to Terminal/Tool views in [#1570](https://github.com/agent-of-empires/agent-of-empires/pull/1570) by [@njbrake](https://github.com/njbrake) ([`4a8fa5c`](https://github.com/agent-of-empires/agent-of-empires/commit/4a8fa5ce0d5885c04dafbbe4a757ec30b83e946a))
- **tui:** Make w jump to next waiting in Attention in [#1571](https://github.com/agent-of-empires/agent-of-empires/pull/1571) by [@grepsedawk](https://github.com/grepsedawk) ([`57833ed`](https://github.com/agent-of-empires/agent-of-empires/commit/57833ed420857f30269af2477a6a094cd744c323))
- **web:** Use bracketed paste for Shift+Enter in terminal in [#1560](https://github.com/agent-of-empires/agent-of-empires/pull/1560) by [@Eric162](https://github.com/Eric162) ([`16d91ad`](https://github.com/agent-of-empires/agent-of-empires/commit/16d91ad76450c54a3f2709db631118dd67504a09))
- **web:** Cut first-session-open WS retry storm from ~60s to <5s in [#1577](https://github.com/agent-of-empires/agent-of-empires/pull/1577) by [@Seluj78](https://github.com/Seluj78) ([`539aadc`](https://github.com/agent-of-empires/agent-of-empires/commit/539aadce01e367b68a427911399a42a3d73cc06a))
- **server,web:** Theme picker reverts after reload, narrow elevation gate to safe preference fields in [#1575](https://github.com/agent-of-empires/agent-of-empires/pull/1575) by [@Seluj78](https://github.com/Seluj78) ([`3990cd5`](https://github.com/agent-of-empires/agent-of-empires/commit/3990cd50504836464eac125bafa6a55e1a4b6a0d))
- **cockpit:** Always append trailing space when picking slash command in [#1573](https://github.com/agent-of-empires/agent-of-empires/pull/1573) by [@Seluj78](https://github.com/Seluj78) ([`e99d7a0`](https://github.com/agent-of-empires/agent-of-empires/commit/e99d7a076ad34ab0d9c413b47d10743f0c86933e))
- **worktree:** Surface fetch failures and apply canonical-remote scoring to explicit base branch in [#1574](https://github.com/agent-of-empires/agent-of-empires/pull/1574) by [@Seluj78](https://github.com/Seluj78) ([`4b27693`](https://github.com/agent-of-empires/agent-of-empires/commit/4b27693ab31958585ed6e9b831435f74d9db9e64))
- **session:** Keep stopped sessions stopped across aoe relaunches in [#1586](https://github.com/agent-of-empires/agent-of-empires/pull/1586) by [@njbrake](https://github.com/njbrake) ([`9a58c04`](https://github.com/agent-of-empires/agent-of-empires/commit/9a58c0402b965b7477f524d6cc4e0b020d71c116))
- **ci:** Build linux releases inside manylinux_2_28 for portable glibc floor in [#1584](https://github.com/agent-of-empires/agent-of-empires/pull/1584) by [@njbrake](https://github.com/njbrake) ([`5849635`](https://github.com/agent-of-empires/agent-of-empires/commit/584963592e0c72f84a7bfcbd72e5e62e0b6153e9))
- **ci:** Gate release workflows behind a required-reviewer environment in [#1594](https://github.com/agent-of-empires/agent-of-empires/pull/1594) by [@njbrake](https://github.com/njbrake) ([`12f65ed`](https://github.com/agent-of-empires/agent-of-empires/commit/12f65edcdbc30e3c17063cf3337e82deb2f90992))


### Features

- **web:** Add opt-in last-activity sort mode to sidebar (#1418) in [#1547](https://github.com/agent-of-empires/agent-of-empires/pull/1547) by [@Seluj78](https://github.com/Seluj78) ([`18f9d09`](https://github.com/agent-of-empires/agent-of-empires/commit/18f9d092a7ada12df93c9e5e8700ee1a7bf2fc81))
- **tui:** Allow preview drag-select outside live mode in [#1556](https://github.com/agent-of-empires/agent-of-empires/pull/1556) by [@njbrake](https://github.com/njbrake) ([`a364cdf`](https://github.com/agent-of-empires/agent-of-empires/commit/a364cdf750d91a9602c9ea4d2617cfd35a2866cd))
- **tui:** Nest Archived section by project and persist auto-unsink on re-enter in [#1557](https://github.com/agent-of-empires/agent-of-empires/pull/1557) by [@njbrake](https://github.com/njbrake) ([`65c1f1b`](https://github.com/agent-of-empires/agent-of-empires/commit/65c1f1bf99742acc9cedf0bc46cd180b74e403a9))
- **cockpit:** Model picker + reasoning effort selector (#1403) in [#1548](https://github.com/agent-of-empires/agent-of-empires/pull/1548) by [@Seluj78](https://github.com/Seluj78) ([`fd954f3`](https://github.com/agent-of-empires/agent-of-empires/commit/fd954f39dabb57e25c31a276146009cba379881a))
- Scratch-directory toggle for new sessions in [#1549](https://github.com/agent-of-empires/agent-of-empires/pull/1549) by [@Seluj78](https://github.com/Seluj78) ([`1c17f53`](https://github.com/agent-of-empires/agent-of-empires/commit/1c17f53428f08fe299b49e4fcdb8e9c054edf054))
- **web:** Pin, archive, and snooze triage on the sidebar in [#1585](https://github.com/agent-of-empires/agent-of-empires/pull/1585) by [@Seluj78](https://github.com/Seluj78) ([`f73cb94`](https://github.com/agent-of-empires/agent-of-empires/commit/f73cb948535c567f986a14a153defdf2d5f23a83))
- **cockpit:** Bump claude-agent-acp floor to 0.38.0 for Opus 4.8 support in [#1603](https://github.com/agent-of-empires/agent-of-empires/pull/1603) by [@Seluj78](https://github.com/Seluj78) ([`75a8ee0`](https://github.com/agent-of-empires/agent-of-empires/commit/75a8ee0bf10f6f8b1d1d95d156eec156cdccfe60))



### New Contributors

- [@Eric162](https://github.com/Eric162) made their first contribution in [#1560](https://github.com/agent-of-empires/agent-of-empires/pull/1560)
- [@grepsedawk](https://github.com/grepsedawk) made their first contribution in [#1571](https://github.com/agent-of-empires/agent-of-empires/pull/1571)
- [@itisaevalex](https://github.com/itisaevalex) made their first contribution in [#1523](https://github.com/agent-of-empires/agent-of-empires/pull/1523)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.9.3...v1.9.4
## [1.9.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.9.3) - 2026-05-27



### Bug Fixes

- **dependabot:** Drop semver-days from non-semver ecosystems in [#1528](https://github.com/agent-of-empires/agent-of-empires/pull/1528) by [@njbrake](https://github.com/njbrake) ([`94eb6a3`](https://github.com/agent-of-empires/agent-of-empires/commit/94eb6a3c1decf2c0023938e14d126af86986efb1))


### Features

- Relocate update checker and install scripts to new repo URL (3/4) in [#1505](https://github.com/agent-of-empires/agent-of-empires/pull/1505) by [@njbrake](https://github.com/njbrake) ([`365ce6a`](https://github.com/agent-of-empires/agent-of-empires/commit/365ce6ad4e470aa295571bf0832ce36568032d9d))
- Relocate sandbox image to ghcr.io/agent-of-empires namespace in [#1506](https://github.com/agent-of-empires/agent-of-empires/pull/1506) by [@njbrake](https://github.com/njbrake) ([`a19a074`](https://github.com/agent-of-empires/agent-of-empires/commit/a19a07431e243fbee78bd622bf346c3ac7ad2cfc))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.9.2...v1.9.3
## [1.9.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.9.2) - 2026-05-26



### Bug Fixes

- **tui:** Keep live-mode entry frame from rendering shifted up in [#1521](https://github.com/agent-of-empires/agent-of-empires/pull/1521) by [@njbrake](https://github.com/njbrake) ([`6bf747c`](https://github.com/agent-of-empires/agent-of-empires/commit/6bf747cdad18481cc8d0b885e64e58ec5fea7c46))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.9.1...v1.9.2
## [1.9.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.9.1) - 2026-05-26



### Bug Fixes

- Reconcile completed Codex hook prompts in [#1488](https://github.com/agent-of-empires/agent-of-empires/pull/1488) by [@microHoffman](https://github.com/microHoffman) ([`53112ad`](https://github.com/agent-of-empires/agent-of-empires/commit/53112adcb74f515268043342ddda9539f47c75d7))
- **tests:** De-flake cockpit live specs with expect.poll backoff in [#1494](https://github.com/agent-of-empires/agent-of-empires/pull/1494) by [@njbrake](https://github.com/njbrake) ([`dd9f0f7`](https://github.com/agent-of-empires/agent-of-empires/commit/dd9f0f7d7c5631f482172f2693549660cc21c4a3))
- **tui:** Clear hover highlight on keyboard nav in [#1497](https://github.com/agent-of-empires/agent-of-empires/pull/1497) by [@BTForIT](https://github.com/BTForIT) ([`125cc62`](https://github.com/agent-of-empires/agent-of-empires/commit/125cc623faebc9baeb548ce2b6d2ed82679f0390))
- **tmux:** Detect cursor and antigravity activity in [#1479](https://github.com/agent-of-empires/agent-of-empires/pull/1479) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`507360a`](https://github.com/agent-of-empires/agent-of-empires/commit/507360a21aa44f82b64799d06b8bb11b656c1db3))
- **tui:** Keep live-send preview alive past a single capture failure in [#1501](https://github.com/agent-of-empires/agent-of-empires/pull/1501) by [@njbrake](https://github.com/njbrake) ([`6f85c78`](https://github.com/agent-of-empires/agent-of-empires/commit/6f85c780581743c71f78de86207bc3b9faf816f9))
- **web:** Allow SPA bootstrap session routes in [#1489](https://github.com/agent-of-empires/agent-of-empires/pull/1489) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`6cbf7fe`](https://github.com/agent-of-empires/agent-of-empires/commit/6cbf7fe5d4dcb253ce94ee100e2496fcb5b7f000))
- **web:** Keep live terminal resize stable in [#1487](https://github.com/agent-of-empires/agent-of-empires/pull/1487) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`365c63c`](https://github.com/agent-of-empires/agent-of-empires/commit/365c63cc16a2ab4be73437c32c21d2ec626d41ef))
- **session:** Force color for codex launches in [#1478](https://github.com/agent-of-empires/agent-of-empires/pull/1478) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`2ab9910`](https://github.com/agent-of-empires/agent-of-empires/commit/2ab9910fc724f5961237408a621a39824b5cadda))
- **tui:** Right-pad row tag to mode-max width so activity column stays stable in [#1460](https://github.com/agent-of-empires/agent-of-empires/pull/1460) by [@BTForIT](https://github.com/BTForIT) ([`5a4b7eb`](https://github.com/agent-of-empires/agent-of-empires/commit/5a4b7eb905ef3a9274a927d84fbc2502293efa4c))
- **tui:** Remove clipboard tests that leak xclip/wl-copy daemons in [#1518](https://github.com/agent-of-empires/agent-of-empires/pull/1518) by [@njbrake](https://github.com/njbrake) ([`4de1292`](https://github.com/agent-of-empires/agent-of-empires/commit/4de1292044ed1b4a72e0d816d31fcb0a34f5d6bf))


### Features

- **tui:** Opt-in live-send as default attach for new sessions in [#1486](https://github.com/agent-of-empires/agent-of-empires/pull/1486) by [@njbrake](https://github.com/njbrake) ([`b70d6f0`](https://github.com/agent-of-empires/agent-of-empires/commit/b70d6f09e0de371e33b0dba8e26ff4eb4e3504fa))
- **tui:** Click-to-live + attach-mode setting + settings reorg in [#1493](https://github.com/agent-of-empires/agent-of-empires/pull/1493) by [@njbrake](https://github.com/njbrake) ([`3b62036`](https://github.com/agent-of-empires/agent-of-empires/commit/3b6203681352018ba66b93425a8449f22a3862a8))
- **tui:** Drag-to-select-and-copy in live mode + multi-line paste in [#1502](https://github.com/agent-of-empires/agent-of-empires/pull/1502) by [@njbrake](https://github.com/njbrake) ([`9f84b26`](https://github.com/agent-of-empires/agent-of-empires/commit/9f84b2691d65dc475aa9ed44e1950bfddb252c37))
- **tui:** Replace g/o cycle bindings with modal pickers in [#1508](https://github.com/agent-of-empires/agent-of-empires/pull/1508) by [@njbrake](https://github.com/njbrake) ([`b740428`](https://github.com/agent-of-empires/agent-of-empires/commit/b740428fd2519f693f5aaf466ce61d73bbd80d19))
- **tui:** Configurable single-click action on session rows in [#1520](https://github.com/agent-of-empires/agent-of-empires/pull/1520) by [@njbrake](https://github.com/njbrake) ([`aca2fab`](https://github.com/agent-of-empires/agent-of-empires/commit/aca2fabaccc0372c6c3bfd4dd131b321db01323f))


### Performance

- **tmux:** Route live-send preview captures through a long-lived tmux -C client in [#1490](https://github.com/agent-of-empires/agent-of-empires/pull/1490) by [@njbrake](https://github.com/njbrake) ([`be9df3e`](https://github.com/agent-of-empires/agent-of-empires/commit/be9df3e41636c3ec4dcd6020777cdfba3abe634a))
- **tui:** Split-render preview on live-send %output wakes in [#1495](https://github.com/agent-of-empires/agent-of-empires/pull/1495) by [@njbrake](https://github.com/njbrake) ([`7f07de6`](https://github.com/agent-of-empires/agent-of-empires/commit/7f07de66ecc3831281bc511af24898a07b944d47))
- **tui:** Rework live-send dispatch for reliability and lower latency in [#1519](https://github.com/agent-of-empires/agent-of-empires/pull/1519) by [@njbrake](https://github.com/njbrake) ([`adfc6fc`](https://github.com/agent-of-empires/agent-of-empires/commit/adfc6fc9418c2619b8feb21a4cdb11a84979c57c))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.9.0...v1.9.1
## [1.9.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.9.0) - 2026-05-25



### Bug Fixes

- **ci:** Switch nix-npm-hash bot to PR flow + validate hash on PRs in [#1420](https://github.com/agent-of-empires/agent-of-empires/pull/1420) by [@jerome-benoit](https://github.com/jerome-benoit) ([`3318b17`](https://github.com/agent-of-empires/agent-of-empires/commit/3318b17888fbd7e401d92e3e188e7d5685971cf1))
- **session:** Cross-process flock around Storage mutators in [#1398](https://github.com/agent-of-empires/agent-of-empires/pull/1398) by [@jerome-benoit](https://github.com/jerome-benoit) ([`b6ecdb4`](https://github.com/agent-of-empires/agent-of-empires/commit/b6ecdb4a59ec3fc7a3ff26ad4fd0c3603fb08a0d))
- **sandbox:** Inject git safe.directory via env vars to fix dubious ownership error in [#1458](https://github.com/agent-of-empires/agent-of-empires/pull/1458) by [@flpdorea](https://github.com/flpdorea) ([`8bba8ee`](https://github.com/agent-of-empires/agent-of-empires/commit/8bba8ee14ab85d088bc6a4319f392f5eeb5be47a))
- **update:** Re-check periodically inside the TUI in [#1473](https://github.com/agent-of-empires/agent-of-empires/pull/1473) by [@njbrake](https://github.com/njbrake) ([`4fe2d8b`](https://github.com/agent-of-empires/agent-of-empires/commit/4fe2d8b7965b7a8ad00dd57f62a066c91e66ef04))
- **hermes:** Implement real pane-based status detection in [#1477](https://github.com/agent-of-empires/agent-of-empires/pull/1477) by [@angelogalanti](https://github.com/angelogalanti) ([`0196150`](https://github.com/agent-of-empires/agent-of-empires/commit/01961509fe93943de59fec217d1385be36c96f47))


### Features

- **web:** Cockpit user-story foundation (mandate + harness + app fixes) in [#1443](https://github.com/agent-of-empires/agent-of-empires/pull/1443) by [@Seluj78](https://github.com/Seluj78) ([`f715abb`](https://github.com/agent-of-empires/agent-of-empires/commit/f715abbc2805fcf28ece4994a1b8ca8f4b0642ab))
- **tui:** Drag list/preview divider, click preview to send, click Yes/No on delete dialog in [#1464](https://github.com/agent-of-empires/agent-of-empires/pull/1464) by [@njbrake](https://github.com/njbrake) ([`210fe21`](https://github.com/agent-of-empires/agent-of-empires/commit/210fe21262a20ea93b41e044568e138e1e9ff8fd))
- **tui:** Live-send mode — Tab to passthrough keystrokes to a session pane in [#1482](https://github.com/agent-of-empires/agent-of-empires/pull/1482) by [@njbrake](https://github.com/njbrake) ([`3ba5337`](https://github.com/agent-of-empires/agent-of-empires/commit/3ba533719115c005a394dfd4ab8054c206ad3c69))


### Other

- Avoid false stale-shell error transitions in [#1433](https://github.com/agent-of-empires/agent-of-empires/pull/1433) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`ed8d5ea`](https://github.com/agent-of-empires/agent-of-empires/commit/ed8d5ea84354737e6f536c994a1837b833462774))



### New Contributors

- [@angelogalanti](https://github.com/angelogalanti) made their first contribution in [#1477](https://github.com/agent-of-empires/agent-of-empires/pull/1477)
- [@flpdorea](https://github.com/flpdorea) made their first contribution in [#1458](https://github.com/agent-of-empires/agent-of-empires/pull/1458)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.8.1...v1.9.0
## [1.8.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.8.1) - 2026-05-22



### Bug Fixes

- **ci:** Restore contributor attribution in release notes in [#1422](https://github.com/agent-of-empires/agent-of-empires/pull/1422) by [@Seluj78](https://github.com/Seluj78) ([`e58dcbc`](https://github.com/agent-of-empires/agent-of-empires/commit/e58dcbcc0d4b7049d05f0bd553b6630f1ea2384a))
- **tui:** Render git-cliff release notes cleanly in "What's New" popup in [#1423](https://github.com/agent-of-empires/agent-of-empires/pull/1423) by [@njbrake](https://github.com/njbrake) ([`fa629c7`](https://github.com/agent-of-empires/agent-of-empires/commit/fa629c7916510bbda1533c7fe5fe9c99b9eeac3a))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.8.0...v1.8.1
## [1.8.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.8.0) - 2026-05-22



### Bug Fixes

- **test:** De-flake live ensure-session-restart status check in [#1249](https://github.com/agent-of-empires/agent-of-empires/pull/1249) by [@Seluj78](https://github.com/Seluj78) ([`39d4b40`](https://github.com/agent-of-empires/agent-of-empires/commit/39d4b40b2ba5a886c4d9b9cff52644580aa3fc9c))
- **sandbox:** Propagate env to ACP terminal/create + surface missing-host-var warnings in [#1253](https://github.com/agent-of-empires/agent-of-empires/pull/1253) by [@njbrake](https://github.com/njbrake) ([`8bc31da`](https://github.com/agent-of-empires/agent-of-empires/commit/8bc31da2bed0c0eccf3edd6aa20699559d6f2d8e))
- **session:** Defense in depth for resume-fallback cascade races in [#1250](https://github.com/agent-of-empires/agent-of-empires/pull/1250) by [@jerome-benoit](https://github.com/jerome-benoit) ([`3b8756c`](https://github.com/agent-of-empires/agent-of-empires/commit/3b8756ca4516d823a44a5ac830f7143a2ca6c521))
- **session:** Per-profile in-process lock around Storage in [#1257](https://github.com/agent-of-empires/agent-of-empires/pull/1257) by [@jerome-benoit](https://github.com/jerome-benoit) ([`2dd3442`](https://github.com/agent-of-empires/agent-of-empires/commit/2dd3442555b5532ef7176988bb36488ee8328290))
- **sandbox:** Silence false-positive env warnings for terminal defaults in [#1268](https://github.com/agent-of-empires/agent-of-empires/pull/1268) by [@njbrake](https://github.com/njbrake) ([`466b448`](https://github.com/agent-of-empires/agent-of-empires/commit/466b448b48063a8059387496108e72f93bd374ee))
- **cockpit:** Keep Escape from cancelling the active turn in [#1280](https://github.com/agent-of-empires/agent-of-empires/pull/1280) by [@Seluj78](https://github.com/Seluj78) ([`1591df6`](https://github.com/agent-of-empires/agent-of-empires/commit/1591df6f4cf0958b23c07cdc862b8264641c2804))
- **sandbox:** Bundle cockpit ACP adapters in sandbox image in [#1278](https://github.com/agent-of-empires/agent-of-empires/pull/1278) by [@Seluj78](https://github.com/Seluj78) ([`8ef73a7`](https://github.com/agent-of-empires/agent-of-empires/commit/8ef73a725b57ed02758ecac2afd5655ab0d254ef))
- **recovery:** Skip startup recovery on tmux probe failure in [#1276](https://github.com/agent-of-empires/agent-of-empires/pull/1276) by [@jerome-benoit](https://github.com/jerome-benoit) ([`26ea2ce`](https://github.com/agent-of-empires/agent-of-empires/commit/26ea2ce7e814af84b7927479c2f4d94f57bb9fe8))
- **cockpit:** Drain stdout, stderr, and wait concurrently in terminal_handler in [#1283](https://github.com/agent-of-empires/agent-of-empires/pull/1283) by [@jerome-benoit](https://github.com/jerome-benoit) ([`9a71416`](https://github.com/agent-of-empires/agent-of-empires/commit/9a7141684eb6998187c1d59630a200567284c959))
- **cockpit:** Suppress force-end-turn while a tool is in flight in [#1279](https://github.com/agent-of-empires/agent-of-empires/pull/1279) by [@Seluj78](https://github.com/Seluj78) ([`a30c9d5`](https://github.com/agent-of-empires/agent-of-empires/commit/a30c9d5eefecbd502720e5edd5dc87e1ffabad7f))
- **server:** Run read-only check before body validation on mutating POST/PATCH in [#1258](https://github.com/agent-of-empires/agent-of-empires/pull/1258) by [@Seluj78](https://github.com/Seluj78) ([`30e0d6b`](https://github.com/agent-of-empires/agent-of-empires/commit/30e0d6b3b3a50615676534964fe81049da946157))
- **server:** Respect state.shutdown in background cleanup loops in [#1289](https://github.com/agent-of-empires/agent-of-empires/pull/1289) by [@jerome-benoit](https://github.com/jerome-benoit) ([`244899c`](https://github.com/agent-of-empires/agent-of-empires/commit/244899c516a7575d785977a8729f62f049309ee8))
- **server:** Drop tunnel child guard before restart_tunnel + select on cancel in [#1290](https://github.com/agent-of-empires/agent-of-empires/pull/1290) by [@jerome-benoit](https://github.com/jerome-benoit) ([`2fc541a`](https://github.com/agent-of-empires/agent-of-empires/commit/2fc541ae959d3b5d01e406478b9ecabc45298c16))
- **server:** Push wake fire respects SEND_CONCURRENCY semaphore in [#1294](https://github.com/agent-of-empires/agent-of-empires/pull/1294) by [@jerome-benoit](https://github.com/jerome-benoit) ([`3663f1e`](https://github.com/agent-of-empires/agent-of-empires/commit/3663f1e05c9a46675ef584a822b19018c59436d5))
- **web:** Revive LoginPage live spec, stop /api/login 401 token-screen swap in [#1302](https://github.com/agent-of-empires/agent-of-empires/pull/1302) by [@Seluj78](https://github.com/Seluj78) ([`8c3dfd4`](https://github.com/agent-of-empires/agent-of-empires/commit/8c3dfd425c4f2c5a7f1af8ad9f0162d495331d21))
- **server:** Use MissedTickBehavior::Skip for cleanup intervals + unify CancellationToken import in [#1312](https://github.com/agent-of-empires/agent-of-empires/pull/1312) by [@jerome-benoit](https://github.com/jerome-benoit) ([`f72304f`](https://github.com/agent-of-empires/agent-of-empires/commit/f72304fb4cd53948a2a276158467ef40b54de216))
- **cockpit:** Warn on blocking-task JoinError instead of silent fallback in [#1314](https://github.com/agent-of-empires/agent-of-empires/pull/1314) by [@jerome-benoit](https://github.com/jerome-benoit) ([`410de2c`](https://github.com/agent-of-empires/agent-of-empires/commit/410de2c217c5e3c2f23cb7b19f4590bff258d4cb))
- **cockpit:** Extract spawn_blocking_fs helper, drop fs handler clones in [#1315](https://github.com/agent-of-empires/agent-of-empires/pull/1315) by [@jerome-benoit](https://github.com/jerome-benoit) ([`a9f3eae`](https://github.com/agent-of-empires/agent-of-empires/commit/a9f3eaeb7eb7f601776bfa6a3d5b9fba682955f9))
- **cockpit/ws:** Restore drop-cancels-reader semantics + drop mut from shutdown in [#1318](https://github.com/agent-of-empires/agent-of-empires/pull/1318) by [@jerome-benoit](https://github.com/jerome-benoit) ([`a9718ab`](https://github.com/agent-of-empires/agent-of-empires/commit/a9718ab3ec159d23129230ef494b7474c8f7cb88))
- **cockpit:** Drop stale 50ms doc + bump notify regression test timeout in [#1320](https://github.com/agent-of-empires/agent-of-empires/pull/1320) by [@jerome-benoit](https://github.com/jerome-benoit) ([`b7bfec0`](https://github.com/agent-of-empires/agent-of-empires/commit/b7bfec03720db319d3ae2477b570e410319edb3d))
- **cockpit:** Close attach-vs-shutdown race + restore test rustdocs (#1284 follow-ups) in [#1308](https://github.com/agent-of-empires/agent-of-empires/pull/1308) by [@jerome-benoit](https://github.com/jerome-benoit) ([`e510b74`](https://github.com/agent-of-empires/agent-of-empires/commit/e510b746e79533535e9eb73ced59ca8f803da292))
- **cockpit:** Assert exit_code in concurrent-drain test and document lossy decode in [#1304](https://github.com/agent-of-empires/agent-of-empires/pull/1304) by [@jerome-benoit](https://github.com/jerome-benoit) ([`a8386db`](https://github.com/agent-of-empires/agent-of-empires/commit/a8386db36f8892bdf1f7442fce3e8010967ad48e))
- **cockpit:** Silent-orphan watchdog for adapter wedges in [#1248](https://github.com/agent-of-empires/agent-of-empires/pull/1248) by [@Seluj78](https://github.com/Seluj78) ([`fe6b95e`](https://github.com/agent-of-empires/agent-of-empires/commit/fe6b95e95cc5183da733e87ec6d993e6c7e91dfa))
- **hooks:** Accept "error" in status legend file → Status::Error in [#1326](https://github.com/agent-of-empires/agent-of-empires/pull/1326) by [@BTForIT](https://github.com/BTForIT) ([`2f56e21`](https://github.com/agent-of-empires/agent-of-empires/commit/2f56e214109927ddde26d34d97544fa2afae2530))
- **session:** Resolve repo config from main repo for worktree sessions in [#1329](https://github.com/agent-of-empires/agent-of-empires/pull/1329) by [@weedgrease](https://github.com/weedgrease) ([`df50ed9`](https://github.com/agent-of-empires/agent-of-empires/commit/df50ed9c1735ccd8079896c335aa2a64037761b0))
- **ci,tests:** Unbreak main test suite + upload vitest coverage on failure in [#1342](https://github.com/agent-of-empires/agent-of-empires/pull/1342) by [@Seluj78](https://github.com/Seluj78) ([`8699fa0`](https://github.com/agent-of-empires/agent-of-empires/commit/8699fa0cfe08954a2b4398b19cbf0761ff562a9a))
- **tests/live:** Poll for cockpit supervisor readiness instead of fixed sleep in [#1353](https://github.com/agent-of-empires/agent-of-empires/pull/1353) by [@njbrake](https://github.com/njbrake) ([`1e1bacb`](https://github.com/agent-of-empires/agent-of-empires/commit/1e1bacbb8ace8880c66c84767ae39ac56984d4f1))
- **web:** Prevent QuotaExceeded crash and harden localStorage writes in [#1348](https://github.com/agent-of-empires/agent-of-empires/pull/1348) by [@Seluj78](https://github.com/Seluj78) ([`9cee126`](https://github.com/agent-of-empires/agent-of-empires/commit/9cee126b6407538210f5015c50339088b9097e13))
- **tui:** Keep wheel scroll inside the pane the cursor is over in [#1367](https://github.com/agent-of-empires/agent-of-empires/pull/1367) by [@njbrake](https://github.com/njbrake) ([`323c2d6`](https://github.com/agent-of-empires/agent-of-empires/commit/323c2d602628163880f4cbcfa396736a6f793c4c))
- **tui:** Stop screen flash on Ctrl+x and drop favorite/archive toasts in [#1369](https://github.com/agent-of-empires/agent-of-empires/pull/1369) by [@njbrake](https://github.com/njbrake) ([`246dcf3`](https://github.com/agent-of-empires/agent-of-empires/commit/246dcf38bd4ec10bc422062932083ada15c66165))
- **tui:** Selected row keeps status color when contrast clears 3:1 in [#1376](https://github.com/agent-of-empires/agent-of-empires/pull/1376) by [@njbrake](https://github.com/njbrake) ([`22db953`](https://github.com/agent-of-empires/agent-of-empires/commit/22db9535d07a97ad7fbfc4b9338f2f6198ff0222))
- **session/recovery:** Skip archived/snoozed rows in startup recovery in [#1391](https://github.com/agent-of-empires/agent-of-empires/pull/1391) by [@njbrake](https://github.com/njbrake) ([`46196a2`](https://github.com/agent-of-empires/agent-of-empires/commit/46196a2cffaa21c5703f1788e6e842caff631532))
- **web/diff,server/csp:** Restore WASM-compile CSP so Shiki works; defense-in-depth for invisible diff text in [#1355](https://github.com/agent-of-empires/agent-of-empires/pull/1355) by [@Seluj78](https://github.com/Seluj78) ([`7b77d02`](https://github.com/agent-of-empires/agent-of-empires/commit/7b77d02e8b127edd72c475f803daeb163f09dca1))
- **tui:** Help overlay advertised stale H/L resize binding in [#1393](https://github.com/agent-of-empires/agent-of-empires/pull/1393) by [@njbrake](https://github.com/njbrake) ([`ac437a6`](https://github.com/agent-of-empires/agent-of-empires/commit/ac437a6b785d14699520c7fb17138a045c26aa41))
- Force color for Antigravity launches in [#1382](https://github.com/agent-of-empires/agent-of-empires/pull/1382) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`e2c9a02`](https://github.com/agent-of-empires/agent-of-empires/commit/e2c9a02f6d1b80d7557bf6f77dec40ecd154897d))
- **cockpit:** Suppress silent-orphan watchdog during Claude SDK async-agent waits in [#1364](https://github.com/agent-of-empires/agent-of-empires/pull/1364) by [@Seluj78](https://github.com/Seluj78) ([`7cf82a3`](https://github.com/agent-of-empires/agent-of-empires/commit/7cf82a34b5bb49a9e4583bb55a96ebb9ab34bcfa))
- **hooks:** Make status hook tolerant + drop fragile orphan sweep in [#1394](https://github.com/agent-of-empires/agent-of-empires/pull/1394) by [@njbrake](https://github.com/njbrake) ([`9b6efae`](https://github.com/agent-of-empires/agent-of-empires/commit/9b6efaebfc360b85186b1071778ea999c4227836))
- **cockpit:** Rebase session cost on /clear and /compact boundaries in [#1374](https://github.com/agent-of-empires/agent-of-empires/pull/1374) by [@Seluj78](https://github.com/Seluj78) ([`8258420`](https://github.com/agent-of-empires/agent-of-empires/commit/8258420ff180494e962885e656daeed40b120adb))
- **web:** Gate session route on first sessions fetch in [#1375](https://github.com/agent-of-empires/agent-of-empires/pull/1375) by [@Seluj78](https://github.com/Seluj78) ([`f9185ac`](https://github.com/agent-of-empires/agent-of-empires/commit/f9185acd3b2831e3695534bdadcf7baba21c2c2e))
- **cockpit/web:** Standalone /clear in combined-mode drain (#1356) in [#1378](https://github.com/agent-of-empires/agent-of-empires/pull/1378) by [@Seluj78](https://github.com/Seluj78) ([`9b41265`](https://github.com/agent-of-empires/agent-of-empires/commit/9b412653b4106f61e0cfe626d09b5327c81d18b2))
- **cockpit:** Queue and auto-send composer message when session inactive in [#1379](https://github.com/agent-of-empires/agent-of-empires/pull/1379) by [@Seluj78](https://github.com/Seluj78) ([`2727840`](https://github.com/agent-of-empires/agent-of-empires/commit/272784096ecf31743dba33572149de0170d1b499))
- **web:** Cockpit composer drafts lose tail keystrokes on refresh + orphan keys never pruned in [#1380](https://github.com/agent-of-empires/agent-of-empires/pull/1380) by [@Seluj78](https://github.com/Seluj78) ([`002a823`](https://github.com/agent-of-empires/agent-of-empires/commit/002a823f2cb63cd6d1c7a0505e83184225411a55))
- **tui:** Accept uppercase Q to close help in strict mode in [#1412](https://github.com/agent-of-empires/agent-of-empires/pull/1412) by [@njbrake](https://github.com/njbrake) ([`fb405d0`](https://github.com/agent-of-empires/agent-of-empires/commit/fb405d08e68147623a04c5bf24cd53dfc917d4cc))
- **tui:** Collapse E/F5 help row, restore strict-mode h and Ctrl+G in [#1409](https://github.com/agent-of-empires/agent-of-empires/pull/1409) by [@njbrake](https://github.com/njbrake) ([`2c57654`](https://github.com/agent-of-empires/agent-of-empires/commit/2c57654a8da833cca80e977b626fa7b81bc015fe))
- **test:** De-flake recovery_lock test by removing env-var dependency in [#1413](https://github.com/agent-of-empires/agent-of-empires/pull/1413) by [@njbrake](https://github.com/njbrake) ([`07bf0c2`](https://github.com/agent-of-empires/agent-of-empires/commit/07bf0c2a6391e705b046d14f7916f50daeabbe79))
- **tui:** Honor project grouping under Attention sort in [#1414](https://github.com/agent-of-empires/agent-of-empires/pull/1414) by [@njbrake](https://github.com/njbrake) ([`a65b379`](https://github.com/agent-of-empires/agent-of-empires/commit/a65b379fee10361120e1148e0cf5d5f9733a6085))
- **web:** Bump @assistant-ui to pick up tap out-of-bounds fix in [#1400](https://github.com/agent-of-empires/agent-of-empires/pull/1400) by [@Seluj78](https://github.com/Seluj78) ([`5e001fe`](https://github.com/agent-of-empires/agent-of-empires/commit/5e001fea3075df017b4687b334d0e7e3c3765fcd))
- Let directory browser load more entries in [#1399](https://github.com/agent-of-empires/agent-of-empires/pull/1399) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`14479fa`](https://github.com/agent-of-empires/agent-of-empires/commit/14479fa5e800194f1936e8a16bc882b06a414c47))
- **cockpit:** Silent-orphan watchdog suppression for background Bash + ScheduleWakeup in [#1406](https://github.com/agent-of-empires/agent-of-empires/pull/1406) by [@Seluj78](https://github.com/Seluj78) ([`f6d0905`](https://github.com/agent-of-empires/agent-of-empires/commit/f6d09052698db45221998233dc2c78a7a2a68e93))
- **web/test:** Unmount React trees after each test to stop jsdom-teardown flake in [#1416](https://github.com/agent-of-empires/agent-of-empires/pull/1416) by [@njbrake](https://github.com/njbrake) ([`3b8bbf5`](https://github.com/agent-of-empires/agent-of-empires/commit/3b8bbf56f1292b19223d6cbb414ca56ad5ce7025))


### Features

- Add custom agent creation support for CLI and Web in [#1252](https://github.com/agent-of-empires/agent-of-empires/pull/1252) by [@flyinghail](https://github.com/flyinghail) ([`5e8815c`](https://github.com/agent-of-empires/agent-of-empires/commit/5e8815ce0fe6b82750c4367636f6f0f5d2cee3b7))
- **session:** Startup auto-recovery for missing tmux panes in [#1251](https://github.com/agent-of-empires/agent-of-empires/pull/1251) by [@jerome-benoit](https://github.com/jerome-benoit) ([`999f4e0`](https://github.com/agent-of-empires/agent-of-empires/commit/999f4e04167f4a8bcdc1e0870f2157649f5d564e))
- **web:** Confirm session delete with Enter key in [#1267](https://github.com/agent-of-empires/agent-of-empires/pull/1267) by [@njbrake](https://github.com/njbrake) ([`b5dd15b`](https://github.com/agent-of-empires/agent-of-empires/commit/b5dd15bfd2394422bad410189039a0f8f333ba38))
- **tui:** Surface current sort in list title; drop noisy [all] tag in [#1270](https://github.com/agent-of-empires/agent-of-empires/pull/1270) by [@njbrake](https://github.com/njbrake) ([`1a2469b`](https://github.com/agent-of-empires/agent-of-empires/commit/1a2469b970b578a0fb70bdbe487d1be0331beaf4))
- **web:** Surface debug-vs-release build flavor as topbar DEV badge in [#1272](https://github.com/agent-of-empires/agent-of-empires/pull/1272) by [@njbrake](https://github.com/njbrake) ([`d108a29`](https://github.com/agent-of-empires/agent-of-empires/commit/d108a29d4fa7e2986116cf6f502405ce4b059b24))
- **profile:** Add optional description field surfaced in pickers in [#1274](https://github.com/agent-of-empires/agent-of-empires/pull/1274) by [@njbrake](https://github.com/njbrake) ([`1b3292a`](https://github.com/agent-of-empires/agent-of-empires/commit/1b3292afe7aa5c8874be5df5dca02aaa67092f95))
- **web:** Replace wterm with xterm.js in [#1275](https://github.com/agent-of-empires/agent-of-empires/pull/1275) by [@njbrake](https://github.com/njbrake) ([`45f280d`](https://github.com/agent-of-empires/agent-of-empires/commit/45f280def4f15c89800cae1603ebcb63f524c24e))
- **util:** Add spawn_supervised helper for panic logging + span propagation in [#1293](https://github.com/agent-of-empires/agent-of-empires/pull/1293) by [@jerome-benoit](https://github.com/jerome-benoit) ([`60ae49e`](https://github.com/agent-of-empires/agent-of-empires/commit/60ae49ea28d8c54090a7d723a45bdffda0728189))
- Add status transition command hooks in [#1311](https://github.com/agent-of-empires/agent-of-empires/pull/1311) by [@microHoffman](https://github.com/microHoffman) ([`7458cb5`](https://github.com/agent-of-empires/agent-of-empires/commit/7458cb5b4ff1484cbb86443f67ab6521dc3ac9ab))
- **cockpit:** Rate-limit park and switch-agent recovery (closes #1281, #1282) in [#1300](https://github.com/agent-of-empires/agent-of-empires/pull/1300) by [@Seluj78](https://github.com/Seluj78) ([`ab5f590`](https://github.com/agent-of-empires/agent-of-empires/commit/ab5f590aefdc1a20792da974551e74336c62a7d8))
- **tui:** Attention sort foundation + snooze primitive in [#1084](https://github.com/agent-of-empires/agent-of-empires/pull/1084) by [@BTForIT](https://github.com/BTForIT) ([`1593ec8`](https://github.com/agent-of-empires/agent-of-empires/commit/1593ec81cf5d8c7ded31b99422428bac1b23bf79))
- Favorite session primitive in [#1085](https://github.com/agent-of-empires/agent-of-empires/pull/1085) by [@BTForIT](https://github.com/BTForIT) ([`485ef6e`](https://github.com/agent-of-empires/agent-of-empires/commit/485ef6e6c5ef73c2bfc67056ab9156e4c79141d4))
- Archive primitive (TUI z/Z + CLI session archive/unarchive) in [#1086](https://github.com/agent-of-empires/agent-of-empires/pull/1086) by [@BTForIT](https://github.com/BTForIT) ([`828bbae`](https://github.com/agent-of-empires/agent-of-empires/commit/828bbae9b4664c1fe3fb5b2b03f2614f0b87bf61))
- **send:** Auto-wake archived/snoozed rows + remap status on `aoe send` in [#1087](https://github.com/agent-of-empires/agent-of-empires/pull/1087) by [@BTForIT](https://github.com/BTForIT) ([`2e1a907`](https://github.com/agent-of-empires/agent-of-empires/commit/2e1a90779a0eb038f146465f2e29d513ce45a750))
- Restart-session keybind (e/E/F5) with post-restart wake-up in [#1180](https://github.com/agent-of-empires/agent-of-empires/pull/1180) by [@BTForIT](https://github.com/BTForIT) ([`b0cc124`](https://github.com/agent-of-empires/agent-of-empires/commit/b0cc1249cd7729ad702eb85020c187d0e1042472))
- **tui:** Restart dialog with profile + AI engine pickers in [#1184](https://github.com/agent-of-empires/agent-of-empires/pull/1184) by [@BTForIT](https://github.com/BTForIT) ([`4c755fb`](https://github.com/agent-of-empires/agent-of-empires/commit/4c755fbd2129692d329444950d87d8e85f39f11d))
- **tui:** Per-row profile tag in all-profiles view in [#1244](https://github.com/agent-of-empires/agent-of-empires/pull/1244) by [@BTForIT](https://github.com/BTForIT) ([`245ee33`](https://github.com/agent-of-empires/agent-of-empires/commit/245ee33e27bef5475503df21ddc7c3b5eb4ecc59))
- **hooks:** Expose session env vars to lifecycle hooks in [#1372](https://github.com/agent-of-empires/agent-of-empires/pull/1372) by [@njbrake](https://github.com/njbrake) ([`73c0708`](https://github.com/agent-of-empires/agent-of-empires/commit/73c070805583433d00a9826275e4ad4c815b73ce))
- **session/poller:** Runtime-configurable thread cap via TUI Settings in [#1381](https://github.com/agent-of-empires/agent-of-empires/pull/1381) by [@jerome-benoit](https://github.com/jerome-benoit) ([`ac4a2ad`](https://github.com/agent-of-empires/agent-of-empires/commit/ac4a2ad6c14b23896810c4592e7548bdb74b3d17))
- **tui:** Click + double-click + hover on session list in [#1392](https://github.com/agent-of-empires/agent-of-empires/pull/1392) by [@njbrake](https://github.com/njbrake) ([`cf81bd4`](https://github.com/agent-of-empires/agent-of-empires/commit/cf81bd4b1c5275009b888a4ad656de31ee26d62e))
- **updates:** Rework release cadence + update notification UX in [#1386](https://github.com/agent-of-empires/agent-of-empires/pull/1386) by [@Seluj78](https://github.com/Seluj78) ([`3d83978`](https://github.com/agent-of-empires/agent-of-empires/commit/3d83978caa0b3d923e4481295d9e372f018bd892))
- **tui:** Full-screen multi-column help overlay with scroll in [#1410](https://github.com/agent-of-empires/agent-of-empires/pull/1410) by [@njbrake](https://github.com/njbrake) ([`4ceed86`](https://github.com/agent-of-empires/agent-of-empires/commit/4ceed863f11c6ac42ed410f9232e9d89c18a903f))
- **tui:** Toggle preview info header with i in [#1411](https://github.com/agent-of-empires/agent-of-empires/pull/1411) by [@njbrake](https://github.com/njbrake) ([`1501bf1`](https://github.com/agent-of-empires/agent-of-empires/commit/1501bf1d33b4885712ebb1bb949f4c47f3423f1d))
- Keep web terminals alive behind beta setting in [#1388](https://github.com/agent-of-empires/agent-of-empires/pull/1388) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`87e6b24`](https://github.com/agent-of-empires/agent-of-empires/commit/87e6b24e22d2505a8b30cb424cab78889a2dac05))
- **tui:** Group + clean the "What's New" popup in [#1415](https://github.com/agent-of-empires/agent-of-empires/pull/1415) by [@njbrake](https://github.com/njbrake) ([`331105a`](https://github.com/agent-of-empires/agent-of-empires/commit/331105afe35d8aadcf1ae799b7ea1f11b01d5181))
- **ci:** Adopt git-cliff for CHANGELOG.md and release notes in [#1417](https://github.com/agent-of-empires/agent-of-empires/pull/1417) by [@njbrake](https://github.com/njbrake) ([`0137d52`](https://github.com/agent-of-empires/agent-of-empires/commit/0137d52d495809f8765a919bfafa123c4fa7585f))
- Add web project aliases and colors in [#1407](https://github.com/agent-of-empires/agent-of-empires/pull/1407) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`91d60b7`](https://github.com/agent-of-empires/agent-of-empires/commit/91d60b769be8053ed8ea37e77c38ed10b68fbfef))
- **cockpit:** Align with claude-agent-acp v0.37.0 (pin, version check, memory_recall, native cancelled) in [#1402](https://github.com/agent-of-empires/agent-of-empires/pull/1402) by [@Seluj78](https://github.com/Seluj78) ([`f9b2529`](https://github.com/agent-of-empires/agent-of-empires/commit/f9b2529387a75e975f8bfce567750c81f523cbfb))


### Other

- Fix web terminal wheel coordinates for fullscreen TUIs in [#1344](https://github.com/agent-of-empires/agent-of-empires/pull/1344) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`e6eebd6`](https://github.com/agent-of-empires/agent-of-empires/commit/e6eebd6029ec9bc9f2c5dd27ca4399169e4b78e7))
- Add Antigravity CLI agent support in [#1349](https://github.com/agent-of-empires/agent-of-empires/pull/1349) by [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) ([`2ed20f2`](https://github.com/agent-of-empires/agent-of-empires/commit/2ed20f2f327bfc4a0f92439baf5f1291a0ae0205))
- Update README to encourage stars for AoE project by [@njbrake](https://github.com/njbrake) ([`b6d3df1`](https://github.com/agent-of-empires/agent-of-empires/commit/b6d3df176c1fd3eb3edfcbb02885a85a3265380e))


### Performance

- **cockpit:** Offload fs_handler::handle_read/write to spawn_blocking in [#1292](https://github.com/agent-of-empires/agent-of-empires/pull/1292) by [@jerome-benoit](https://github.com/jerome-benoit) ([`110a3da`](https://github.com/agent-of-empires/agent-of-empires/commit/110a3dace7097e2628e725c529ee7c6d0ff8e1a5))
- **cockpit:** Offload EventStore SQLite to block_in_place + spawn_blocking in [#1291](https://github.com/agent-of-empires/agent-of-empires/pull/1291) by [@jerome-benoit](https://github.com/jerome-benoit) ([`a03b50d`](https://github.com/agent-of-empires/agent-of-empires/commit/a03b50d8ffcb7e62d11d0017ec32b4471ddbf855))
- **web:** Parallelize mocked Playwright suite (5m -> ~1m) in [#1385](https://github.com/agent-of-empires/agent-of-empires/pull/1385) by [@njbrake](https://github.com/njbrake) ([`ebf4182`](https://github.com/agent-of-empires/agent-of-empires/commit/ebf4182576a50040b4b97dbc245effd1a8eb5cf7))



### New Contributors

- [@MovieHolic-Plex](https://github.com/MovieHolic-Plex) made their first contribution in [#1407](https://github.com/agent-of-empires/agent-of-empires/pull/1407)
- [@flyinghail](https://github.com/flyinghail) made their first contribution in [#1252](https://github.com/agent-of-empires/agent-of-empires/pull/1252)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.7.1...v1.8.0
## [1.7.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.7.1) - 2026-05-19



### Bug Fixes

- Tighten codex status detection in [#1125](https://github.com/agent-of-empires/agent-of-empires/pull/1125) by [@microHoffman](https://github.com/microHoffman) ([`0956668`](https://github.com/agent-of-empires/agent-of-empires/commit/095666815fedd8b83487186e7f150c4dfef89c7d))
- **web:** Add interactive-widget=resizes-content to viewport meta in [#1150](https://github.com/agent-of-empires/agent-of-empires/pull/1150) by [@njbrake](https://github.com/njbrake) ([`480a178`](https://github.com/agent-of-empires/agent-of-empires/commit/480a17873a2de1b2ef36f281f46f32de16ac6f33))
- **cockpit:** Collapse composer bottom gap when soft keyboard is open in [#1152](https://github.com/agent-of-empires/agent-of-empires/pull/1152) by [@njbrake](https://github.com/njbrake) ([`b237f95`](https://github.com/agent-of-empires/agent-of-empires/commit/b237f9592fceec07b29e3ab6bf449e33346bfd58))
- **cockpit:** Switch queued-prompt strip from amber to sky palette in [#1153](https://github.com/agent-of-empires/agent-of-empires/pull/1153) by [@njbrake](https://github.com/njbrake) ([`600071f`](https://github.com/agent-of-empires/agent-of-empires/commit/600071f6939a4a0f54bdac360ca17a88c16fdc18))
- **cockpit:** Dispatch InputEvent from toolbar inserts so popover trigger removeOnExecute works in [#1154](https://github.com/agent-of-empires/agent-of-empires/pull/1154) by [@njbrake](https://github.com/njbrake) ([`0fdc04e`](https://github.com/agent-of-empires/agent-of-empires/commit/0fdc04e1e46e24eadcebf8d792637e662bfe3607))
- **cockpit:** Preserve queued prompts on reconnect race in [#1155](https://github.com/agent-of-empires/agent-of-empires/pull/1155) by [@njbrake](https://github.com/njbrake) ([`f6633c6`](https://github.com/agent-of-empires/agent-of-empires/commit/f6633c602870c000e30ce538cd42b3d3692e9c2b))
- **cockpit:** Slow working-spinner verb cycle from 4s to 18s in [#1151](https://github.com/agent-of-empires/agent-of-empires/pull/1151) by [@njbrake](https://github.com/njbrake) ([`427d805`](https://github.com/agent-of-empires/agent-of-empires/commit/427d805d1aa51dbb0e490040197df9068ebe30d4))
- **cockpit:** Derive turnActive from prompt/stop seq counters to survive Stopped race in [#1172](https://github.com/agent-of-empires/agent-of-empires/pull/1172) by [@njbrake](https://github.com/njbrake) ([`b2f26e1`](https://github.com/agent-of-empires/agent-of-empires/commit/b2f26e1b9f80094bcb6b3d3f3a80f8b07325c8d4))
- **wizard:** Seed yoloMode from profile config on mount in [#1156](https://github.com/agent-of-empires/agent-of-empires/pull/1156) by [@njbrake](https://github.com/njbrake) ([`dfb7850`](https://github.com/agent-of-empires/agent-of-empires/commit/dfb7850f81b4323a2245c5d148717a5acf95034a))
- Kill terminal and container terminal tmux sessions on removal in [#1210](https://github.com/agent-of-empires/agent-of-empires/pull/1210) by [@raphaeldavidf](https://github.com/raphaeldavidf) ([`fe987cd`](https://github.com/agent-of-empires/agent-of-empires/commit/fe987cd90c7783a48eed50da5161af0a6d4b9a9d))
- Anchor IME candidate windows to active TUI inputs in [#1202](https://github.com/agent-of-empires/agent-of-empires/pull/1202) by [@raytrun](https://github.com/raytrun) ([`f2a32c6`](https://github.com/agent-of-empires/agent-of-empires/commit/f2a32c6ca6bc7f3b040298e7a666428031c7020a))
- **tui:** Strip ST-terminated OSC sequences so hyperlink text appears in preview in [#1182](https://github.com/agent-of-empires/agent-of-empires/pull/1182) by [@raphaeldavidf](https://github.com/raphaeldavidf) ([`eb502c9`](https://github.com/agent-of-empires/agent-of-empires/commit/eb502c9169cf0a428e9cc50c06c7d3187d697fcc))
- **session:** Use atomic writes for all session/config persistence in [#1208](https://github.com/agent-of-empires/agent-of-empires/pull/1208) by [@raphaeldavidf](https://github.com/raphaeldavidf) ([`9ec7d45`](https://github.com/agent-of-empires/agent-of-empires/commit/9ec7d45320cbb4d81a327a1a366e5e5309106247))
- **tui:** Keep command palette selection visible past viewport in [#1187](https://github.com/agent-of-empires/agent-of-empires/pull/1187) by [@bell-hyun](https://github.com/bell-hyun) ([`3275ba2`](https://github.com/agent-of-empires/agent-of-empires/commit/3275ba2916ec3b1d036ce2e3d251dc777d84d1c2))
- **session:** Resume-fallback cascade for restart/start paths in [#1173](https://github.com/agent-of-empires/agent-of-empires/pull/1173) by [@jerome-benoit](https://github.com/jerome-benoit) ([`1dda0d5`](https://github.com/agent-of-empires/agent-of-empires/commit/1dda0d532a60f1b027cc63ed2e0792848091ed59))
- **web:** Pin sidebar session order to created_at desc, no status reshuffle in [#1171](https://github.com/agent-of-empires/agent-of-empires/pull/1171) by [@njbrake](https://github.com/njbrake) ([`7d782ff`](https://github.com/agent-of-empires/agent-of-empires/commit/7d782ffc9c0f4998d81cdeeb03f7131c94ffc3ae))
- **cockpit:** Exempt loopback from passphrase factor and surface TUI startup errors in [#1190](https://github.com/agent-of-empires/agent-of-empires/pull/1190) by [@Seluj78](https://github.com/Seluj78) ([`c687bab`](https://github.com/agent-of-empires/agent-of-empires/commit/c687bab5c10f49fa3698f59ca400681b4a15a98c))
- **cockpit:** Fire web push and play browser chime on approval requests in [#1191](https://github.com/agent-of-empires/agent-of-empires/pull/1191) by [@Seluj78](https://github.com/Seluj78) ([`5a783bd`](https://github.com/agent-of-empires/agent-of-empires/commit/5a783bddc554faa36c063bfaa414dd1b6c711f9b))
- **cockpit, serve:** Mobile composer polish and push notification origin tracking in [#1194](https://github.com/agent-of-empires/agent-of-empires/pull/1194) by [@Seluj78](https://github.com/Seluj78) ([`0abb8c7`](https://github.com/agent-of-empires/agent-of-empires/commit/0abb8c7fbfac507fccbab0678cd0c8635b074d85))
- **push:** Delay test notification by 3s so user can lock phone in [#1193](https://github.com/agent-of-empires/agent-of-empires/pull/1193) by [@Seluj78](https://github.com/Seluj78) ([`b571468`](https://github.com/agent-of-empires/agent-of-empires/commit/b5714684947f03f301bba605478340d1d750c1a7))
- **cockpit,serve:** Exit on Ctrl-C with open WS, surface dropped prompts, escalate stuck cancels in [#1211](https://github.com/agent-of-empires/agent-of-empires/pull/1211) by [@Seluj78](https://github.com/Seluj78) ([`830a81e`](https://github.com/agent-of-empires/agent-of-empires/commit/830a81e3378c45618bbf85005834a5351626704c))
- Pi install hint → @earendil-works package + correct Pi/Hermes confusion in [#1238](https://github.com/agent-of-empires/agent-of-empires/pull/1238) by [@jerome-benoit](https://github.com/jerome-benoit) ([`deb666c`](https://github.com/agent-of-empires/agent-of-empires/commit/deb666c9a4ac3e4bb1ad5d38f491d3df042a60c0))


### Features

- **logging:** Consolidate sink + rotation under logging in [#1127](https://github.com/agent-of-empires/agent-of-empires/pull/1127) by [@Seluj78](https://github.com/Seluj78) ([`a806e6f`](https://github.com/agent-of-empires/agent-of-empires/commit/a806e6f80d3fd85df1a9aa71a1ee8100a37336ed))
- **cockpit:** Comment on diff + more polishing fixes in [#1122](https://github.com/agent-of-empires/agent-of-empires/pull/1122) by [@Seluj78](https://github.com/Seluj78) ([`6b65255`](https://github.com/agent-of-empires/agent-of-empires/commit/6b65255191fa0f8663bab012d0f5d7e52e7f2dc6))
- **auth:** Keep bound devices signed in across token rotation in [#1167](https://github.com/agent-of-empires/agent-of-empires/pull/1167) by [@njbrake](https://github.com/njbrake) ([`1e3a0a0`](https://github.com/agent-of-empires/agent-of-empires/commit/1e3a0a01e78dbe89848ea31ec113d0e509349812))
- **serve:** Add --auth=<mode> selector and --behind-proxy for reverse-proxy deployments in [#1162](https://github.com/agent-of-empires/agent-of-empires/pull/1162) by [@Seluj78](https://github.com/Seluj78) ([`6507ca3`](https://github.com/agent-of-empires/agent-of-empires/commit/6507ca31cd01f33441d4dc362a9867061dd566fb))
- **cockpit:** Honor sandbox mode in cockpit sessions in [#1161](https://github.com/agent-of-empires/agent-of-empires/pull/1161) by [@Seluj78](https://github.com/Seluj78) ([`c003053`](https://github.com/agent-of-empires/agent-of-empires/commit/c003053ae613e168afc56f90e88a996a42d45619))
- **logging:** Comprehensive coverage + frontend forwarding pipeline in [#1179](https://github.com/agent-of-empires/agent-of-empires/pull/1179) by [@Seluj78](https://github.com/Seluj78) ([`e692ec8`](https://github.com/agent-of-empires/agent-of-empires/commit/e692ec871a8ec4306597e6ef129565e2b1a14814))
- Add configurable tool sessions (lazygit, yazi, etc.) in [#1204](https://github.com/agent-of-empires/agent-of-empires/pull/1204) by [@raphaeldavidf](https://github.com/raphaeldavidf) ([`6be67b5`](https://github.com/agent-of-empires/agent-of-empires/commit/6be67b5a4cfbb06b43074ab2ca46c5d50810dd05))
- **theme:** Web dashboard runtime palette swap in [#1197](https://github.com/agent-of-empires/agent-of-empires/pull/1197) by [@Seluj78](https://github.com/Seluj78) ([`9b5426b`](https://github.com/agent-of-empires/agent-of-empires/commit/9b5426b8fd063da082a0bc8883f038c0cadbb470))
- **cockpit:** Per-agent profile abstraction for codex/opencode/gemini parity in [#1192](https://github.com/agent-of-empires/agent-of-empires/pull/1192) by [@Seluj78](https://github.com/Seluj78) ([`8e73d0a`](https://github.com/agent-of-empires/agent-of-empires/commit/8e73d0ac5af55a9ae8527f7c101d7e43357f9a18))
- **cockpit:** Surface set_mode rejection, fold tall queued-prompts strip in [#1236](https://github.com/agent-of-empires/agent-of-empires/pull/1236) by [@Seluj78](https://github.com/Seluj78) ([`212af18`](https://github.com/agent-of-empires/agent-of-empires/commit/212af1894e7a83898f98a4794dee336f49634cf3))
- **theme:** Add Material Deep Ocean builtin in [#1241](https://github.com/agent-of-empires/agent-of-empires/pull/1241) by [@Seluj78](https://github.com/Seluj78) ([`2850418`](https://github.com/agent-of-empires/agent-of-empires/commit/285041843b6c9bd8557ed2d39e5bcca4cbc817c1))
- **theme:** Split default and empire into two distinct builtins in [#1239](https://github.com/agent-of-empires/agent-of-empires/pull/1239) by [@Seluj78](https://github.com/Seluj78) ([`24a1eb9`](https://github.com/agent-of-empires/agent-of-empires/commit/24a1eb95bbafcc87da3ca0d1fdccd0eb2f1792c4))


### Other

- Cockpit in the TUI (native ratatui view + CLI verbs + cross-machine) in [#1114](https://github.com/agent-of-empires/agent-of-empires/pull/1114) by [@Seluj78](https://github.com/Seluj78) ([`945e431`](https://github.com/agent-of-empires/agent-of-empires/commit/945e431a64afb98e69b2b96116b1b4dbcd0703a7))
- Cockpit polishing 5: WorkerHandle leak, approval recovery, stuck spinners, viewport/banner/spinner polish in [#1115](https://github.com/agent-of-empires/agent-of-empires/pull/1115) by [@Seluj78](https://github.com/Seluj78) ([`cf42eaa`](https://github.com/agent-of-empires/agent-of-empires/commit/cf42eaabec181d1503f0b239c1f9a14cec8ba718))
- Cockpit polishing 7: state persistence, WS auto-reconnect, mobile Enter, /clear palette, device binding in [#1137](https://github.com/agent-of-empires/agent-of-empires/pull/1137) by [@Seluj78](https://github.com/Seluj78) ([`7993c22`](https://github.com/agent-of-empires/agent-of-empires/commit/7993c22467a1ab0ad05a19a90510c0b6a6bbc719))
- Add Codex hook-based status detection in [#1141](https://github.com/agent-of-empires/agent-of-empires/pull/1141) by [@microHoffman](https://github.com/microHoffman) ([`e1890cb`](https://github.com/agent-of-empires/agent-of-empires/commit/e1890cb5793e190a806d6fbbf0006ca7d767aa83))



### New Contributors

- [@microHoffman](https://github.com/microHoffman) made their first contribution in [#1141](https://github.com/agent-of-empires/agent-of-empires/pull/1141)
- [@raphaeldavidf](https://github.com/raphaeldavidf) made their first contribution in [#1204](https://github.com/agent-of-empires/agent-of-empires/pull/1204)
- [@bell-hyun](https://github.com/bell-hyun) made their first contribution in [#1187](https://github.com/agent-of-empires/agent-of-empires/pull/1187)
- [@raytrun](https://github.com/raytrun) made their first contribution in [#1202](https://github.com/agent-of-empires/agent-of-empires/pull/1202)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.7.0...v1.7.1
## [1.7.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.7.0) - 2026-05-14



### Bug Fixes

- **deletion:** Tear down tmux + container before host worktree in [#1023](https://github.com/agent-of-empires/agent-of-empires/pull/1023) by [@njbrake](https://github.com/njbrake) ([`14c9529`](https://github.com/agent-of-empires/agent-of-empires/commit/14c9529730795ec54b14e948355fb5201ed305e8))
- **deletion:** Restore dirty-worktree check + handle anonymous-volume mount-point cruft in [#1066](https://github.com/agent-of-empires/agent-of-empires/pull/1066) by [@njbrake](https://github.com/njbrake) ([`31cd0b2`](https://github.com/agent-of-empires/agent-of-empires/commit/31cd0b2174f08b7756a85f5f6392a0e0a8bf3a27))
- Copy pi agent config directory into sandbox in [#1069](https://github.com/agent-of-empires/agent-of-empires/pull/1069) by [@njbrake](https://github.com/njbrake) ([`1348038`](https://github.com/agent-of-empires/agent-of-empires/commit/1348038c51958f2d32555e22fa44ff728981be95))
- **status:** Detect codex request_user_input as Waiting in [#1121](https://github.com/agent-of-empires/agent-of-empires/pull/1121) by [@Seluj78](https://github.com/Seluj78) ([`9cc88b8`](https://github.com/agent-of-empires/agent-of-empires/commit/9cc88b80340527966ad842a0a40277455b8a1deb))
- **tui:** Surface restart errors in attach via status toast in [#1079](https://github.com/agent-of-empires/agent-of-empires/pull/1079) by [@BTForIT](https://github.com/BTForIT) ([`b8f7af6`](https://github.com/agent-of-empires/agent-of-empires/commit/b8f7af6dfd05bb75658db6233064815d1f2dfaeb))
- **tui:** Voice/paste consolidated — routing, burst, archive-respect, \r normalize in [#1081](https://github.com/agent-of-empires/agent-of-empires/pull/1081) by [@BTForIT](https://github.com/BTForIT) ([`61f5bc9`](https://github.com/agent-of-empires/agent-of-empires/commit/61f5bc9a2c656dd18216970893ba5bf83495e394))


### Features

- **tui:** Ctrl+U/Ctrl+K line-edit + Ctrl+P restore in send-message in [#1053](https://github.com/agent-of-empires/agent-of-empires/pull/1053) by [@njbrake](https://github.com/njbrake) ([`623496d`](https://github.com/agent-of-empires/agent-of-empires/commit/623496d6112f90bff6d6e663986582e465e727b1))
- **cockpit:** Persist ACP workers across `aoe serve` restart (#1037) in [#1045](https://github.com/agent-of-empires/agent-of-empires/pull/1045) by [@Seluj78](https://github.com/Seluj78) ([`07da57a`](https://github.com/agent-of-empires/agent-of-empires/commit/07da57a7ccd49f93a8ee5092ca4bed8055935766))
- Add Rosé Pine built-in theme in [#1015](https://github.com/agent-of-empires/agent-of-empires/pull/1015) by [@jerome-benoit](https://github.com/jerome-benoit) ([`d742694`](https://github.com/agent-of-empires/agent-of-empires/commit/d7426946d0c17fd383e115f4c3549672beefb85e))
- **send:** Respawn dead panes and start stopped sessions before send in [#1078](https://github.com/agent-of-empires/agent-of-empires/pull/1078) by [@BTForIT](https://github.com/BTForIT) ([`dd4224f`](https://github.com/agent-of-empires/agent-of-empires/commit/dd4224f9fa6aacc4613acc8e1d286233e65362c7))
- **cockpit:** Remove AOE_EXPERIMENTAL_COCKPIT env-var gate in [#1098](https://github.com/agent-of-empires/agent-of-empires/pull/1098) by [@njbrake](https://github.com/njbrake) ([`b610a6d`](https://github.com/agent-of-empires/agent-of-empires/commit/b610a6d795f578d58c80411f2aae564a2e586f4d))
- **new-session:** Show path field before title in [#1070](https://github.com/agent-of-empires/agent-of-empires/pull/1070) by [@BTForIT](https://github.com/BTForIT) ([`494a07e`](https://github.com/agent-of-empires/agent-of-empires/commit/494a07e55a88a38eb14f8ad6b2c5babe9c0e6820))
- **profile:** Per-profile host environment variables in [#1117](https://github.com/agent-of-empires/agent-of-empires/pull/1117) by [@BTForIT](https://github.com/BTForIT) ([`7ac3630`](https://github.com/agent-of-empires/agent-of-empires/commit/7ac363097ad5e1a9929c1228c564266644004a5a))
- **tui:** Auto-disable mouse capture under Mosh in [#1116](https://github.com/agent-of-empires/agent-of-empires/pull/1116) by [@BTForIT](https://github.com/BTForIT) ([`7cf0876`](https://github.com/agent-of-empires/agent-of-empires/commit/7cf0876c5e333bcfc41921078ba26dab18fe92a1))
- Observability + logging umbrella (closes #1096) in [#1118](https://github.com/agent-of-empires/agent-of-empires/pull/1118) by [@Seluj78](https://github.com/Seluj78) ([`7461a63`](https://github.com/agent-of-empires/agent-of-empires/commit/7461a63465d56dbcffa0c17848fee25437f33ef0))


### Other

- Cockpit polishing: 9 small fixes across wizard, cockpit, and logs in [#1040](https://github.com/agent-of-empires/agent-of-empires/pull/1040) by [@Seluj78](https://github.com/Seluj78) ([`c505fbc`](https://github.com/agent-of-empires/agent-of-empires/commit/c505fbce143f496ae64b4f6d6bc0eeb7a4a7e1b4))
- Cockpit polishing 2: More cockpit / worktree / sidebar fixes ! in [#1067](https://github.com/agent-of-empires/agent-of-empires/pull/1067) by [@Seluj78](https://github.com/Seluj78) ([`7fdae7b`](https://github.com/agent-of-empires/agent-of-empires/commit/7fdae7b17a6598c12db48d4313e8439b87427941))
- Cockpit polishing 3: memory, diff, subagent, streaming, multi-repo in [#1076](https://github.com/agent-of-empires/agent-of-empires/pull/1076) by [@Seluj78](https://github.com/Seluj78) ([`2044f61`](https://github.com/agent-of-empires/agent-of-empires/commit/2044f61db0d7fb10ce42b9aa2650e1b13d4f240e))
- Two small TUI polish fixes carved from #1022 in [#1077](https://github.com/agent-of-empires/agent-of-empires/pull/1077) by [@njbrake](https://github.com/njbrake) ([`38247ef`](https://github.com/agent-of-empires/agent-of-empires/commit/38247efbb7982a4e0e9a7783fab0f9c8c424af39))
- Cockpit polishing 4: context primer, update banner, base branch picker, sidebar fixes in [#1094](https://github.com/agent-of-empires/agent-of-empires/pull/1094) by [@Seluj78](https://github.com/Seluj78) ([`4a2d872`](https://github.com/agent-of-empires/agent-of-empires/commit/4a2d8726e021707b60b48e51523aa00108c5e29c))



### New Contributors

- [@kimjune01](https://github.com/kimjune01) made their first contribution in [#1042](https://github.com/agent-of-empires/agent-of-empires/pull/1042)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.6.2...v1.7.0
## [1.6.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.6.2) - 2026-05-11



### Bug Fixes

- Webui debug log noise + idempotent session branch deletion in [#992](https://github.com/agent-of-empires/agent-of-empires/pull/992) by [@Seluj78](https://github.com/Seluj78) ([`6516600`](https://github.com/agent-of-empires/agent-of-empires/commit/65166002f06bf4e8803d77ba27f06e3abacb296c))
- **serve:** Web terminal logging + auto-respawn dead pane (#1009) in [#1011](https://github.com/agent-of-empires/agent-of-empires/pull/1011) by [@Seluj78](https://github.com/Seluj78) ([`172fc9a`](https://github.com/agent-of-empires/agent-of-empires/commit/172fc9a33b99eac4183e80f5ca15a1cce922cc85))
- **cli:** Cleaner error when add/init path does not exist in [#987](https://github.com/agent-of-empires/agent-of-empires/pull/987) by [@Seluj78](https://github.com/Seluj78) ([`4ebcf78`](https://github.com/agent-of-empires/agent-of-empires/commit/4ebcf78347b3a6f97271d56fde2f26e158d894e6))


### Features

- Isolate debug-build state from release (#985) in [#995](https://github.com/agent-of-empires/agent-of-empires/pull/995) by [@Seluj78](https://github.com/Seluj78) ([`00fbe3b`](https://github.com/agent-of-empires/agent-of-empires/commit/00fbe3b4508c5d5d457f65e51c6dd7acfdbc7c95))
- **cli:** Add `aoe logs` to view debug/serve logs with a pretty viewer in [#1014](https://github.com/agent-of-empires/agent-of-empires/pull/1014) by [@Seluj78](https://github.com/Seluj78) ([`c3a60ff`](https://github.com/agent-of-empires/agent-of-empires/commit/c3a60fff9d245766f0488da0d8de143dd9fe8e5a))
- **worktree:** Add init_submodules config to skip recursive submodule init in [#1021](https://github.com/agent-of-empires/agent-of-empires/pull/1021) by [@mguthaus](https://github.com/mguthaus) ([`334431b`](https://github.com/agent-of-empires/agent-of-empires/commit/334431b36ba79831a68412d2384362edd23a5ec0))


### Other

- Cockpit polish: SQLite persistence, session/load resume, tool/markdown rendering, offline state in [#1008](https://github.com/agent-of-empires/agent-of-empires/pull/1008) by [@Seluj78](https://github.com/Seluj78) ([`da4df4e`](https://github.com/agent-of-empires/agent-of-empires/commit/da4df4e2678f04eecfd4bdaaefcadbe3f9e20a75))
- Add Qwen Code support and improve container exec test reliability in [#626](https://github.com/agent-of-empires/agent-of-empires/pull/626) by [@ellecer](https://github.com/ellecer) ([`0ab9333`](https://github.com/agent-of-empires/agent-of-empires/commit/0ab93332ff29a33e4bfb55e76d5d34cf5513666c))
- Batch fixes: sidebar sort, browse-dir memory, aoe url, aoe serve --open in [#1012](https://github.com/agent-of-empires/agent-of-empires/pull/1012) by [@Seluj78](https://github.com/Seluj78) ([`efb4106`](https://github.com/agent-of-empires/agent-of-empires/commit/efb4106d0460efc9a9c2fe1df88c6a3d79241ed7))


### Performance

- **worktree:** Parallel workspace creation + tolerate post-checkout hook failures in [#994](https://github.com/agent-of-empires/agent-of-empires/pull/994) by [@Seluj78](https://github.com/Seluj78) ([`9f97d55`](https://github.com/agent-of-empires/agent-of-empires/commit/9f97d5516d5e8edb174aa23a8a5b521628cd7e48))



### New Contributors

- [@ellecer](https://github.com/ellecer) made their first contribution in [#626](https://github.com/agent-of-empires/agent-of-empires/pull/626)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.6.1...v1.6.2
## [1.6.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.6.1) - 2026-05-09



### Bug Fixes

- **docker:** Add unzip to base sandbox image for Kiro CLI installer in [#999](https://github.com/agent-of-empires/agent-of-empires/pull/999) by [@njbrake](https://github.com/njbrake) ([`bf04967`](https://github.com/agent-of-empires/agent-of-empires/commit/bf04967c1609e00c339c0468e2032cf0e2279038))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.6.0...v1.6.1
## [1.6.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.6.0) - 2026-05-09



### Bug Fixes

- **session:** Read Hermes sessions via rusqlite instead of sqlite3 CLI in [#908](https://github.com/agent-of-empires/agent-of-empires/pull/908) by [@jerome-benoit](https://github.com/jerome-benoit) ([`6b45e14`](https://github.com/agent-of-empires/agent-of-empires/commit/6b45e14164abbade10dc94f3a95a10eb1a5cf540))
- **session:** Align opencode DB path resolution with upstream in [#907](https://github.com/agent-of-empires/agent-of-empires/pull/907) by [@jerome-benoit](https://github.com/jerome-benoit) ([`6f121d1`](https://github.com/agent-of-empires/agent-of-empires/commit/6f121d1f27d1b047dabef321003948d4b783a70d))
- **update:** Suppress update prompt while Homebrew formula lags in [#916](https://github.com/agent-of-empires/agent-of-empires/pull/916) by [@njbrake](https://github.com/njbrake) ([`31c4f45`](https://github.com/agent-of-empires/agent-of-empires/commit/31c4f450c5baa3d26a564babfc588d0f87cb8e65))
- Log silently swallowed errors instead of discarding them in [#915](https://github.com/agent-of-empires/agent-of-empires/pull/915) by [@jerome-benoit](https://github.com/jerome-benoit) ([`905b6bf`](https://github.com/agent-of-empires/agent-of-empires/commit/905b6bfdfc1542a98bacc7b7954c9d3dcad529b7))
- **server:** Replace blocking I/O in async functions with tokio equivalents in [#912](https://github.com/agent-of-empires/agent-of-empires/pull/912) by [@jerome-benoit](https://github.com/jerome-benoit) ([`f4a1aa5`](https://github.com/agent-of-empires/agent-of-empires/commit/f4a1aa5f74e5c361e8b8d7e3a99fd86446b8acbe))
- Clarify existing-project session wizard title in [#936](https://github.com/agent-of-empires/agent-of-empires/pull/936) by [@njbrake](https://github.com/njbrake) ([`a18a16e`](https://github.com/agent-of-empires/agent-of-empires/commit/a18a16e4ff3851c56bc2c8dea53606ec3abc8be6))
- Remove topbar settings action in [#937](https://github.com/agent-of-empires/agent-of-empires/pull/937) by [@njbrake](https://github.com/njbrake) ([`61c8d57`](https://github.com/agent-of-empires/agent-of-empires/commit/61c8d572025307c5c1dcf9cd5ae7dc1b67bb2ec8))
- Clarify worktree template setting descriptions in [#938](https://github.com/agent-of-empires/agent-of-empires/pull/938) by [@njbrake](https://github.com/njbrake) ([`2a11732`](https://github.com/agent-of-empires/agent-of-empires/commit/2a11732455bc21e66483e7558fa8807fc35050b1))
- Use links for sidebar session navigation in [#939](https://github.com/agent-of-empires/agent-of-empires/pull/939) by [@njbrake](https://github.com/njbrake) ([`c61ffd6`](https://github.com/agent-of-empires/agent-of-empires/commit/c61ffd67246f1148c27fd8223e3a81422d27b177))
- Respect remote default branch detection in [#940](https://github.com/agent-of-empires/agent-of-empires/pull/940) by [@njbrake](https://github.com/njbrake) ([`b6bbc1d`](https://github.com/agent-of-empires/agent-of-empires/commit/b6bbc1da6099587b0b8e4bbb8f1dce8735bb0718))
- Clarify workflow preset picker in [#941](https://github.com/agent-of-empires/agent-of-empires/pull/941) by [@njbrake](https://github.com/njbrake) ([`dd8ac21`](https://github.com/agent-of-empires/agent-of-empires/commit/dd8ac21c87049a9fd0a1dd9f394864115b14a307))
- Initialize submodules in new worktrees in [#942](https://github.com/agent-of-empires/agent-of-empires/pull/942) by [@njbrake](https://github.com/njbrake) ([`af9b2ea`](https://github.com/agent-of-empires/agent-of-empires/commit/af9b2eaa2c5fda76f49646a3c19f310a71a4e53b))
- **web:** Avoid leaking IME pre-edit keys in [#918](https://github.com/agent-of-empires/agent-of-empires/pull/918) by [@mintisan](https://github.com/mintisan) ([`cd8af79`](https://github.com/agent-of-empires/agent-of-empires/commit/cd8af79bd2aadd0753788a8fa25901061a4b0f34))
- Separate session title from branch in [#943](https://github.com/agent-of-empires/agent-of-empires/pull/943) by [@njbrake](https://github.com/njbrake) ([`96671cc`](https://github.com/agent-of-empires/agent-of-empires/commit/96671cc17bb4697b898647f9c97ad89060b20455))
- Reframe web project flow as session creation in [#944](https://github.com/agent-of-empires/agent-of-empires/pull/944) by [@njbrake](https://github.com/njbrake) ([`c43318f`](https://github.com/agent-of-empires/agent-of-empires/commit/c43318f5ad5e06a406f383314631a9ffbe9b59e3))
- Sync dashboard idle decay from settings in [#947](https://github.com/agent-of-empires/agent-of-empires/pull/947) by [@zerone0x](https://github.com/zerone0x) ([`334dc86`](https://github.com/agent-of-empires/agent-of-empires/commit/334dc8681f4bc2e2d3c0f6683fee09e069463b69))
- **serve:** Raise RLIMIT_NOFILE and clean up tmux child on PTY init failure in [#971](https://github.com/agent-of-empires/agent-of-empires/pull/971) by [@Seluj78](https://github.com/Seluj78) ([`8878473`](https://github.com/agent-of-empires/agent-of-empires/commit/887847384f38790d8fcf85ecd66d5b7517efaa46))
- **serve:** WebSocket heartbeat and idle reaper for terminal connections in [#981](https://github.com/agent-of-empires/agent-of-empires/pull/981) by [@njbrake](https://github.com/njbrake) ([`5e2f6fd`](https://github.com/agent-of-empires/agent-of-empires/commit/5e2f6fdf751647d971a4b7d16e3276a73db2d47c))
- Clean up empty wrapper dirs after worktree removal in [#988](https://github.com/agent-of-empires/agent-of-empires/pull/988) by [@njbrake](https://github.com/njbrake) ([`a7f3cd9`](https://github.com/agent-of-empires/agent-of-empires/commit/a7f3cd94de5b8a897ed90aa760fad86c2c6cff5a))
- **web:** Make sidebar session row a block link so active border and hover fill the row in [#998](https://github.com/agent-of-empires/agent-of-empires/pull/998) by [@Seluj78](https://github.com/Seluj78) ([`8a3879d`](https://github.com/agent-of-empires/agent-of-empires/commit/8a3879d719c1bed30843c04f1065800799a642c4))


### Features

- **cli:** Aoe session restart --all in [#910](https://github.com/agent-of-empires/agent-of-empires/pull/910) by [@BTForIT](https://github.com/BTForIT) ([`edaa1bd`](https://github.com/agent-of-empires/agent-of-empires/commit/edaa1bd28767dbce5fbaefc2a16246baee97087c))
- **sandbox:** Support Podman as a container runtime in [#903](https://github.com/agent-of-empires/agent-of-empires/pull/903) by [@njbrake](https://github.com/njbrake) ([`ff98490`](https://github.com/agent-of-empires/agent-of-empires/commit/ff98490868e44e5835966807ad9a1c0f4edaaefb))
- **container:** Add claude vertex auth forwarding with GCP credential support in [#954](https://github.com/agent-of-empires/agent-of-empires/pull/954) by [@CharlyRipp](https://github.com/CharlyRipp) ([`011001d`](https://github.com/agent-of-empires/agent-of-empires/commit/011001d24f28f80c7ff752826cafb969b1724b67))
- Add Kiro CLI agent support in [#958](https://github.com/agent-of-empires/agent-of-empires/pull/958) by [@nycjay](https://github.com/nycjay) ([`1d8a93c`](https://github.com/agent-of-empires/agent-of-empires/commit/1d8a93c54e6dfdb01775cf19214db41de8a87193))
- **web:** Worktree toggle + cleaner new-session wizard in [#978](https://github.com/agent-of-empires/agent-of-empires/pull/978) by [@X-Skoprio](https://github.com/X-Skoprio) ([`1efbaef`](https://github.com/agent-of-empires/agent-of-empires/commit/1efbaeff8d417fbde70faae3b26514bcd5601c78))
- Multi-repo workspace support (project registry + pickers + dashboard) in [#974](https://github.com/agent-of-empires/agent-of-empires/pull/974) by [@Seluj78](https://github.com/Seluj78) ([`598549e`](https://github.com/agent-of-empires/agent-of-empires/commit/598549e322ffecbf02ccae24ecfc40c4a8e313cc))
- **cockpit:** Native ACP rendering surface (Beta) for all supported agents in [#868](https://github.com/agent-of-empires/agent-of-empires/pull/868) by [@njbrake](https://github.com/njbrake) ([`ffb3794`](https://github.com/agent-of-empires/agent-of-empires/commit/ffb3794ab2e644707755d22806b0da1d78b1de86))


### Other

- Create FUNDING.yml by [@njbrake](https://github.com/njbrake) ([`406417b`](https://github.com/agent-of-empires/agent-of-empires/commit/406417b07f19ef4f8caf9770581d4e9fe0576d26))
- Add Trendshift badge to README by [@njbrake](https://github.com/njbrake) ([`08be017`](https://github.com/agent-of-empires/agent-of-empires/commit/08be0174e7412d625b3aadc290a49cc7d40003bd))
- Compact tool selector with bidirectional cycle navigation in [#977](https://github.com/agent-of-empires/agent-of-empires/pull/977) by [@flowq-C](https://github.com/flowq-C) ([`b83023b`](https://github.com/agent-of-empires/agent-of-empires/commit/b83023b9f12f103e48aed58d69d24d4a0863c1f3))
- Make worktree creation checkbox-driven in the TUI in [#979](https://github.com/agent-of-empires/agent-of-empires/pull/979) by [@dadegallx](https://github.com/dadegallx) ([`0cb9e97`](https://github.com/agent-of-empires/agent-of-empires/commit/0cb9e97853874a37f202dde2849f2bc91576a667))



### New Contributors

- [@Seluj78](https://github.com/Seluj78) made their first contribution in [#998](https://github.com/agent-of-empires/agent-of-empires/pull/998)
- [@X-Skoprio](https://github.com/X-Skoprio) made their first contribution in [#978](https://github.com/agent-of-empires/agent-of-empires/pull/978)
- [@dadegallx](https://github.com/dadegallx) made their first contribution in [#979](https://github.com/agent-of-empires/agent-of-empires/pull/979)
- [@flowq-C](https://github.com/flowq-C) made their first contribution in [#977](https://github.com/agent-of-empires/agent-of-empires/pull/977)
- [@nycjay](https://github.com/nycjay) made their first contribution in [#959](https://github.com/agent-of-empires/agent-of-empires/pull/959)
- [@CharlyRipp](https://github.com/CharlyRipp) made their first contribution in [#954](https://github.com/agent-of-empires/agent-of-empires/pull/954)
- [@mintisan](https://github.com/mintisan) made their first contribution in [#918](https://github.com/agent-of-empires/agent-of-empires/pull/918)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.5.2...v1.6.0
## [1.5.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.5.2) - 2026-05-05



### Bug Fixes

- **hooks:** Detach streamed hooks from controlling TTY (#901) in [#902](https://github.com/agent-of-empires/agent-of-empires/pull/902) by [@njbrake](https://github.com/njbrake) ([`39662df`](https://github.com/agent-of-empires/agent-of-empires/commit/39662df09ce449a55cf1d83c4360b5a938e18cc9))
- **session:** Read opencode session list from SQLite, not subprocess in [#905](https://github.com/agent-of-empires/agent-of-empires/pull/905) by [@njbrake](https://github.com/njbrake) ([`67624b8`](https://github.com/agent-of-empires/agent-of-empires/commit/67624b8275cfd795ba1a5f856d1921c69a5f1599))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.5.1...v1.5.2
## [1.5.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.5.1) - 2026-05-04



### Bug Fixes

- **web:** Cancel momentum decay on exitScrollback in [#858](https://github.com/agent-of-empires/agent-of-empires/pull/858) by [@njbrake](https://github.com/njbrake) ([`39c992b`](https://github.com/agent-of-empires/agent-of-empires/commit/39c992bbde706fa0dd8e1a6e8430c50e65ae1c73))
- **cli:** Use 'aoe' instead of 'agent-of-empires' in CLI hints in [#859](https://github.com/agent-of-empires/agent-of-empires/pull/859) by [@njbrake](https://github.com/njbrake) ([`7158297`](https://github.com/agent-of-empires/agent-of-empires/commit/7158297b7dd29246250d558862ad9728ebea8315))
- Warn on config parse errors instead of silently using defaults in [#867](https://github.com/agent-of-empires/agent-of-empires/pull/867) by [@tun1r](https://github.com/tun1r) ([`8b48e08`](https://github.com/agent-of-empires/agent-of-empires/commit/8b48e087639158bc8e8a6f0f5fe183bf129904ec))
- **tui,web:** Default idle freshness signal to off (opt-in) in [#876](https://github.com/agent-of-empires/agent-of-empires/pull/876) by [@njbrake](https://github.com/njbrake) ([`7f302b2`](https://github.com/agent-of-empires/agent-of-empires/commit/7f302b27bed3a04a12fe2ba77409ce22c9f99798))
- **web:** Sync agent_session_id back to in-memory state after restart in [#877](https://github.com/agent-of-empires/agent-of-empires/pull/877) by [@njbrake](https://github.com/njbrake) ([`e89cd86`](https://github.com/agent-of-empires/agent-of-empires/commit/e89cd865179c83112b2b1601dfac7dc78fd92dc3))
- **web,serve:** Stop login-required token loop, refresh serve.url on rotation in [#878](https://github.com/agent-of-empires/agent-of-empires/pull/878) by [@njbrake](https://github.com/njbrake) ([`dc450f9`](https://github.com/agent-of-empires/agent-of-empires/commit/dc450f9d84683ad1fdedbc423f32c295a59d2172))
- **web:** Stop SIGWINCH on every soft-keyboard cycle on mobile in [#880](https://github.com/agent-of-empires/agent-of-empires/pull/880) by [@njbrake](https://github.com/njbrake) ([`308d12e`](https://github.com/agent-of-empires/agent-of-empires/commit/308d12ed9184c30218f876a6a03b88659a140a4f))
- **tui:** Use actual tmux prefix in welcome dialog and status bar in [#887](https://github.com/agent-of-empires/agent-of-empires/pull/887) by [@redhelix](https://github.com/redhelix) ([`0817195`](https://github.com/agent-of-empires/agent-of-empires/commit/0817195b96b12e22be2c0c6f8374d67f4b5c062e))
- **tui:** Add breathing room between ↵ icon and description in [#895](https://github.com/agent-of-empires/agent-of-empires/pull/895) by [@njbrake](https://github.com/njbrake) ([`bd73cd0`](https://github.com/agent-of-empires/agent-of-empires/commit/bd73cd0e89d015f78c89456999ae66b20e6f859e))
- **tmux:** Pane-based fallback for Claude Code status (#890) in [#893](https://github.com/agent-of-empires/agent-of-empires/pull/893) by [@njbrake](https://github.com/njbrake) ([`0d24b13`](https://github.com/agent-of-empires/agent-of-empires/commit/0d24b13bf20a0f338afc972de642e1eaa8a3809a))
- UTF-8 safe truncate_id in [#896](https://github.com/agent-of-empires/agent-of-empires/pull/896) by [@swamy18](https://github.com/swamy18) ([`3f9617e`](https://github.com/agent-of-empires/agent-of-empires/commit/3f9617e4edf96ef8075defa1ea24952141714bce))


### Features

- **session:** Add Pi session resume in [#852](https://github.com/agent-of-empires/agent-of-empires/pull/852) by [@jerome-benoit](https://github.com/jerome-benoit) ([`942ffb6`](https://github.com/agent-of-empires/agent-of-empires/commit/942ffb66d4128f05fb8030b7725f86876702c5f1))
- **session:** Add Codex session resume in [#853](https://github.com/agent-of-empires/agent-of-empires/pull/853) by [@jerome-benoit](https://github.com/jerome-benoit) ([`db8c9e5`](https://github.com/agent-of-empires/agent-of-empires/commit/db8c9e57e85bc18c1763ec4683168e018f75dac6))
- **session:** Add Gemini CLI session resume in [#854](https://github.com/agent-of-empires/agent-of-empires/pull/854) by [@jerome-benoit](https://github.com/jerome-benoit) ([`ff95113`](https://github.com/agent-of-empires/agent-of-empires/commit/ff95113e7897aa7f33c1fc27c5e45dc2a9b62c61))
- **web:** Toggle terminal focus with Cmd/Ctrl+` in [#857](https://github.com/agent-of-empires/agent-of-empires/pull/857) by [@njbrake](https://github.com/njbrake) ([`80ccff9`](https://github.com/agent-of-empires/agent-of-empires/commit/80ccff9789bfceef6c075eb5c9583576c2592b6e))
- **session:** Adaptive polling with backoff and thread budget in [#860](https://github.com/agent-of-empires/agent-of-empires/pull/860) by [@jerome-benoit](https://github.com/jerome-benoit) ([`ac1bced`](https://github.com/agent-of-empires/agent-of-empires/commit/ac1bced1180cf7d38cba1d24260d058f118ac8af))
- **session:** Add Hermes session resume in [#866](https://github.com/agent-of-empires/agent-of-empires/pull/866) by [@jerome-benoit](https://github.com/jerome-benoit) ([`5daae69`](https://github.com/agent-of-empires/agent-of-empires/commit/5daae698fe536753ecbce7d8cc8e9d6d65755d8b))
- **tui:** Responsive layout for narrow viewports (Mosh/iPhone) in [#865](https://github.com/agent-of-empires/agent-of-empires/pull/865) by [@BTForIT](https://github.com/BTForIT) ([`800e422`](https://github.com/agent-of-empires/agent-of-empires/commit/800e42216bc5e5124680723accb9854962246439))
- **web:** Add merch page and shorten tagline in [#869](https://github.com/agent-of-empires/agent-of-empires/pull/869) by [@njbrake](https://github.com/njbrake) ([`99ca115`](https://github.com/agent-of-empires/agent-of-empires/commit/99ca115c2a79642e78b9dd0a2ba4f64ac78e5181))
- **api:** POST /sessions/{id}/send + GET /sessions/{id}/output in [#861](https://github.com/agent-of-empires/agent-of-empires/pull/861) by [@BTForIT](https://github.com/BTForIT) ([`29ea433`](https://github.com/agent-of-empires/agent-of-empires/commit/29ea433048e816345607331dfa3166e179be7a52))
- **tui:** IPad-friendly ±10 nav (Shift+Up/Down, { / }) + tmux send-keys -- separator in [#862](https://github.com/agent-of-empires/agent-of-empires/pull/862) by [@BTForIT](https://github.com/BTForIT) ([`9185fb0`](https://github.com/agent-of-empires/agent-of-empires/commit/9185fb02fb749ffa4370f940c5596fcc6134e083))
- **tui:** Shorten home title to 'aoe', show full name in help footer in [#871](https://github.com/agent-of-empires/agent-of-empires/pull/871) by [@njbrake](https://github.com/njbrake) ([`2f9b6bf`](https://github.com/agent-of-empires/agent-of-empires/commit/2f9b6bf760f919610232e6860e0f63171bd99cec))
- **tui,web:** Fresh-idle pulse + configurable decay for Stop hook (#863) in [#872](https://github.com/agent-of-empires/agent-of-empires/pull/872) by [@njbrake](https://github.com/njbrake) ([`9c20269`](https://github.com/agent-of-empires/agent-of-empires/commit/9c20269654ae132ecc8df85e2d21c72e3b2db19d))
- **tui:** Add Ctrl+K command palette in [#892](https://github.com/agent-of-empires/agent-of-empires/pull/892) by [@njbrake](https://github.com/njbrake) ([`e169569`](https://github.com/agent-of-empires/agent-of-empires/commit/e1695690693e0901d048f76d18c7c10b4a6dee43))
- **tui:** Tighten status bar footer in [#894](https://github.com/agent-of-empires/agent-of-empires/pull/894) by [@njbrake](https://github.com/njbrake) ([`5290aa6`](https://github.com/agent-of-empires/agent-of-empires/commit/5290aa613b8300153f6d4db4c4468c21330491af))
- **tmux:** Forward OSC 52 clipboard from wrapped agents in [#899](https://github.com/agent-of-empires/agent-of-empires/pull/899) by [@njbrake](https://github.com/njbrake) ([`7ce51b1`](https://github.com/agent-of-empires/agent-of-empires/commit/7ce51b1744a44f4ffea3e20b645e196ff7e99506))



### New Contributors

- [@swamy18](https://github.com/swamy18) made their first contribution in [#896](https://github.com/agent-of-empires/agent-of-empires/pull/896)
- [@redhelix](https://github.com/redhelix) made their first contribution in [#887](https://github.com/agent-of-empires/agent-of-empires/pull/887)
- [@tun1r](https://github.com/tun1r) made their first contribution in [#867](https://github.com/agent-of-empires/agent-of-empires/pull/867)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.5.0...v1.5.1
## [1.5.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.5.0) - 2026-04-29



### Bug Fixes

- **tui:** Show last-activity column at common narrow-pane widths in [#777](https://github.com/agent-of-empires/agent-of-empires/pull/777) by [@BTForIT](https://github.com/BTForIT) ([`bd66795`](https://github.com/agent-of-empires/agent-of-empires/commit/bd66795454009fb0385e29fcc01785ce6a7799bd))
- **tui:** Release mouse capture while the serve URL view is open in [#806](https://github.com/agent-of-empires/agent-of-empires/pull/806) by [@njbrake](https://github.com/njbrake) ([`156d12f`](https://github.com/agent-of-empires/agent-of-empires/commit/156d12fa35c2e3bcfb7e54a141f846a9bc75e4d9))
- **web:** Mobile paste reads URL types and skips iOS keyboard popup in [#809](https://github.com/agent-of-empires/agent-of-empires/pull/809) by [@njbrake](https://github.com/njbrake) ([`c42580c`](https://github.com/agent-of-empires/agent-of-empires/commit/c42580cc0ec43bfeaf2d2056cf4b7feefba79a3a))
- **web:** Pin terminal scroll on wterm scrollTop reset in [#810](https://github.com/agent-of-empires/agent-of-empires/pull/810) by [@njbrake](https://github.com/njbrake) ([`72d0111`](https://github.com/agent-of-empires/agent-of-empires/commit/72d01113e9b2752f84ba8e6d51422643682bf889))
- **web:** Mobile scroll activation + tmux scrollback corruption in [#811](https://github.com/agent-of-empires/agent-of-empires/pull/811) by [@njbrake](https://github.com/njbrake) ([`b961465`](https://github.com/agent-of-empires/agent-of-empires/commit/b961465379afc4f8c2351fd8081bede02f32aaf6))
- **web:** Apply selected profile's overrides when launching sessions in [#812](https://github.com/agent-of-empires/agent-of-empires/pull/812) by [@njbrake](https://github.com/njbrake) ([`a8b2bd4`](https://github.com/agent-of-empires/agent-of-empires/commit/a8b2bd423bba157a02a16347ffdbdaa253098e7f))
- **serve:** Only require cloudflared when tailscale can't carry --remote in [#820](https://github.com/agent-of-empires/agent-of-empires/pull/820) by [@njbrake](https://github.com/njbrake) ([`9bd26bd`](https://github.com/agent-of-empires/agent-of-empires/commit/9bd26bd0dd7612cef97be6d175b058ed7073bd60))
- **tui:** Remove redundant exec to fix sandbox pane death on shells like bash in [#819](https://github.com/agent-of-empires/agent-of-empires/pull/819) by [@blaisepic](https://github.com/blaisepic) ([`e01dd72`](https://github.com/agent-of-empires/agent-of-empires/commit/e01dd7222a98e438aa54624a5120d842fe187dfb))
- **serve:** Stop daemon child from self-detecting via its own PID file in [#821](https://github.com/agent-of-empires/agent-of-empires/pull/821) by [@njbrake](https://github.com/njbrake) ([`7831256`](https://github.com/agent-of-empires/agent-of-empires/commit/78312560cd1bc9cac15684925de9199b8cd8b0a7))
- **agents:** Correct install hints for pi, vibe, droid, and settl in [#823](https://github.com/agent-of-empires/agent-of-empires/pull/823) by [@njbrake](https://github.com/njbrake) ([`0959e21`](https://github.com/agent-of-empires/agent-of-empires/commit/0959e21b4cc777d909fb5bdf5b31e79c0ad3570c))
- **web:** Collapse init-time PTY resize storm causing #807 garbled output in [#822](https://github.com/agent-of-empires/agent-of-empires/pull/822) by [@njbrake](https://github.com/njbrake) ([`843ab99`](https://github.com/agent-of-empires/agent-of-empires/commit/843ab998ccea3b9a8122c6669b30d890a9a714ff))
- **tui:** Show $AOE_INSTANCE_ID in hooks install dialog example in [#824](https://github.com/agent-of-empires/agent-of-empires/pull/824) by [@njbrake](https://github.com/njbrake) ([`e565341`](https://github.com/agent-of-empires/agent-of-empires/commit/e565341a36dc1a4c48b768f12432e6f42fe44bed))
- **serve:** Strip tmux DEC alternate charset to work around wterm#49 in [#837](https://github.com/agent-of-empires/agent-of-empires/pull/837) by [@njbrake](https://github.com/njbrake) ([`c9fd2fe`](https://github.com/agent-of-empires/agent-of-empires/commit/c9fd2fe9b4881756dd64ccd7c22dce7376d8479a))
- **tui:** List dirty files when worktree delete fails (#826) in [#847](https://github.com/agent-of-empires/agent-of-empires/pull/847) by [@njbrake](https://github.com/njbrake) ([`45a6685`](https://github.com/agent-of-empires/agent-of-empires/commit/45a6685d3e8e7112ee3ec2e70d8b5a41c3834711))
- Replace deprecated GenericArray::as_slice with as_ref in [#856](https://github.com/agent-of-empires/agent-of-empires/pull/856) by [@njbrake](https://github.com/njbrake) ([`eab185a`](https://github.com/agent-of-empires/agent-of-empires/commit/eab185af4e1996607ce737716aaf6d0d6d3313ba))


### Features

- **web:** Expose full settings surface in web UI in [#793](https://github.com/agent-of-empires/agent-of-empires/pull/793) by [@njbrake](https://github.com/njbrake) ([`8446b69`](https://github.com/agent-of-empires/agent-of-empires/commit/8446b6910ac147c3689da7f5f7ea7fc53e631bb3))
- **tui:** Mouse scroll and position indicator for preview pane in [#795](https://github.com/agent-of-empires/agent-of-empires/pull/795) by [@hansonkim](https://github.com/hansonkim) ([`f7b3581`](https://github.com/agent-of-empires/agent-of-empires/commit/f7b35810e345d437b3f49cced24dbe537aabff3b))
- **tui:** Add w/W hotkeys to jump to next waiting session in [#796](https://github.com/agent-of-empires/agent-of-empires/pull/796) by [@mguthaus](https://github.com/mguthaus) ([`52746ac`](https://github.com/agent-of-empires/agent-of-empires/commit/52746ac2e3c40113b114fdc5476aec209f26471e))
- **web:** Add URL-based routing for dashboard views in [#808](https://github.com/agent-of-empires/agent-of-empires/pull/808) by [@njbrake](https://github.com/njbrake) ([`68a24a3`](https://github.com/agent-of-empires/agent-of-empires/commit/68a24a3ce737b03b5071e1925d211909867538d0))
- Detect Claude fullscreen renderer to simplify mobile path in [#829](https://github.com/agent-of-empires/agent-of-empires/pull/829) by [@njbrake](https://github.com/njbrake) ([`150d331`](https://github.com/agent-of-empires/agent-of-empires/commit/150d33133b0bdcbdebab8b617edffde2fee2ccf3))
- **session:** Claude session resume MVP in [#838](https://github.com/agent-of-empires/agent-of-empires/pull/838) by [@njbrake](https://github.com/njbrake) ([`3013a83`](https://github.com/agent-of-empires/agent-of-empires/commit/3013a83c8fc0639f62adfa1ef4a82998c92fc5c2))
- Add Hermes agent support in [#846](https://github.com/agent-of-empires/agent-of-empires/pull/846) by [@huilang021x](https://github.com/huilang021x) ([`91df915`](https://github.com/agent-of-empires/agent-of-empires/commit/91df9156c1aeb5f71acd7a74457e615c6aafd884))
- **session:** Add OpenCode session resume in [#850](https://github.com/agent-of-empires/agent-of-empires/pull/850) by [@jerome-benoit](https://github.com/jerome-benoit) ([`0f2e191`](https://github.com/agent-of-empires/agent-of-empires/commit/0f2e1910b535d2569ebbca8fb2c36818424096f3))
- **session:** Add Mistral Vibe session resume in [#851](https://github.com/agent-of-empires/agent-of-empires/pull/851) by [@jerome-benoit](https://github.com/jerome-benoit) ([`6a962ae`](https://github.com/agent-of-empires/agent-of-empires/commit/6a962ae709676e6b6e528c603ea099cb0e03b585))
- In-app self-update with aoe update and a TUI hotkey in [#835](https://github.com/agent-of-empires/agent-of-empires/pull/835) by [@weedgrease](https://github.com/weedgrease) ([`f3d6d88`](https://github.com/agent-of-empires/agent-of-empires/commit/f3d6d88ca4dbacc54c10be1a582c7d7dc9b94378))


### Other

- Update README.md by [@njbrake](https://github.com/njbrake) ([`0858a32`](https://github.com/agent-of-empires/agent-of-empires/commit/0858a3216b1fa6cdf52c578ccdb8f9e533a41863))



### New Contributors

- [@weedgrease](https://github.com/weedgrease) made their first contribution in [#835](https://github.com/agent-of-empires/agent-of-empires/pull/835)
- [@huilang021x](https://github.com/huilang021x) made their first contribution in [#846](https://github.com/agent-of-empires/agent-of-empires/pull/846)
- [@mguthaus](https://github.com/mguthaus) made their first contribution in [#796](https://github.com/agent-of-empires/agent-of-empires/pull/796)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.4.6...v1.5.0
## [1.4.6](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.4.6) - 2026-04-24



### Bug Fixes

- **server:** Prevent daemon orphaning on failed re-spawn in [#742](https://github.com/agent-of-empires/agent-of-empires/pull/742) by [@njbrake](https://github.com/njbrake) ([`ddb05bc`](https://github.com/agent-of-empires/agent-of-empires/commit/ddb05bc8fe08002ea72458988ad59485b2231df2))
- **ci:** Apply cargo fmt to serve.rs in [#745](https://github.com/agent-of-empires/agent-of-empires/pull/745) by [@njbrake](https://github.com/njbrake) ([`2b86fb7`](https://github.com/agent-of-empires/agent-of-empires/commit/2b86fb703e7f5d40003901cc7ed3a7bfa43295a9))
- **web:** Fix mobile keyboard hiding terminal content and FAB state in [#746](https://github.com/agent-of-empires/agent-of-empires/pull/746) by [@njbrake](https://github.com/njbrake) ([`ba98279`](https://github.com/agent-of-empires/agent-of-empires/commit/ba9827944da144f50979b62de41562206eb9fa0f))
- Delay Enter after send-keys for Codex paste-burst suppression in [#749](https://github.com/agent-of-empires/agent-of-empires/pull/749) by [@njbrake](https://github.com/njbrake) ([`db01533`](https://github.com/agent-of-empires/agent-of-empires/commit/db01533c79511d8e0e4d040777675e8e3706800c))
- **web:** Wrap localStorage.setItem in try/catch in useWebSettings update() in [#751](https://github.com/agent-of-empires/agent-of-empires/pull/751) by [@njbrake](https://github.com/njbrake) ([`c028546`](https://github.com/agent-of-empires/agent-of-empires/commit/c028546be24016b98ccd7ff2f436fb3f8c784c92))
- Update rand 0.10.0 → 0.10.1 to resolve RUSTSEC-2026-0097 in [#750](https://github.com/agent-of-empires/agent-of-empires/pull/750) by [@njbrake](https://github.com/njbrake) ([`477f6f3`](https://github.com/agent-of-empires/agent-of-empires/commit/477f6f33a1afb621fa583c93887270d3ff2a8d53))
- **docs:** Resolve asset paths for guides in subdirectories in [#752](https://github.com/agent-of-empires/agent-of-empires/pull/752) by [@njbrake](https://github.com/njbrake) ([`726078f`](https://github.com/agent-of-empires/agent-of-empires/commit/726078f616cd11982274eef08a34fffffa045709))
- **web:** Prevent mobile context menu from closing on finger lift in [#753](https://github.com/agent-of-empires/agent-of-empires/pull/753) by [@njbrake](https://github.com/njbrake) ([`12b4f13`](https://github.com/agent-of-empires/agent-of-empires/commit/12b4f139c18e90fbae83fed3e987b29caa3b0ace))
- **web:** Keep terminal cursor visible when mobile keyboard opens in [#759](https://github.com/agent-of-empires/agent-of-empires/pull/759) by [@njbrake](https://github.com/njbrake) ([`e7c0baa`](https://github.com/agent-of-empires/agent-of-empires/commit/e7c0baa041386a2735ebee384794057a4c76d65e))
- Stop session restart loop for fish/nu/pwsh shell users in [#758](https://github.com/agent-of-empires/agent-of-empires/pull/758) by [@njbrake](https://github.com/njbrake) ([`d08de28`](https://github.com/agent-of-empires/agent-of-empires/commit/d08de282df60b8d23dd74c0590022a0c2e525d62))
- Exec tmux default shell to prevent fish reattach restart loop (#757) in [#760](https://github.com/agent-of-empires/agent-of-empires/pull/760) by [@njbrake](https://github.com/njbrake) ([`0be8d78`](https://github.com/agent-of-empires/agent-of-empires/commit/0be8d78a04393691644f5083cd608a6833d344b1))
- Cleanup unused fields, sort refactor, and small fixes from #762 review in [#766](https://github.com/agent-of-empires/agent-of-empires/pull/766) by [@njbrake](https://github.com/njbrake) ([`19ad0d0`](https://github.com/agent-of-empires/agent-of-empires/commit/19ad0d0bb85872cc38b20b624e1bc3ccb895b05f))
- Rustfmt violation and rustls-webpki security advisory (RUSTSEC-2026-0104) in [#774](https://github.com/agent-of-empires/agent-of-empires/pull/774) by [@njbrake](https://github.com/njbrake) ([`22d5fe9`](https://github.com/agent-of-empires/agent-of-empires/commit/22d5fe931fc90aef9373243b09cae6c2c0f1951c))
- **tests:** Use ControlOrMeta+k for cross-platform Playwright compat in [#769](https://github.com/agent-of-empires/agent-of-empires/pull/769) by [@gdw2vs](https://github.com/gdw2vs) ([`d03b4e8`](https://github.com/agent-of-empires/agent-of-empires/commit/d03b4e844f08e14497f55f54587a0f24a87f1d96))
- **web:** Enable mouse wheel scrolling in desktop terminal pane in [#779](https://github.com/agent-of-empires/agent-of-empires/pull/779) by [@njbrake](https://github.com/njbrake) ([`75a72c9`](https://github.com/agent-of-empires/agent-of-empires/commit/75a72c9d00200b09a38eaea8a39affdde42c6fd7))
- **web:** Pause claude while user reads scrollback on mobile & desktop in [#781](https://github.com/agent-of-empires/agent-of-empires/pull/781) by [@njbrake](https://github.com/njbrake) ([`5408973`](https://github.com/agent-of-empires/agent-of-empires/commit/540897378745ae023ff2a901b1820bf0072ef49e))
- **diff:** Scroll branch select dialog when branches overflow in [#780](https://github.com/agent-of-empires/agent-of-empires/pull/780) by [@hansonkim](https://github.com/hansonkim) ([`88ad06f`](https://github.com/agent-of-empires/agent-of-empires/commit/88ad06f85236049718b2ad4c522463283c5f4f9b))
- **web:** Dismiss settings overlay when selecting a session in [#783](https://github.com/agent-of-empires/agent-of-empires/pull/783) by [@njbrake](https://github.com/njbrake) ([`9a95b09`](https://github.com/agent-of-empires/agent-of-empires/commit/9a95b09be45c839613a09ba7c66d3dd6a3e89e2e))
- **web:** Focus agent terminal instead of shell on new session in [#784](https://github.com/agent-of-empires/agent-of-empires/pull/784) by [@njbrake](https://github.com/njbrake) ([`f8793d9`](https://github.com/agent-of-empires/agent-of-empires/commit/f8793d9fc10ae69171e0e6f49c4b77c9c155360b))
- **web:** Remove "Repeat last session" sidebar button in [#785](https://github.com/agent-of-empires/agent-of-empires/pull/785) by [@njbrake](https://github.com/njbrake) ([`add1816`](https://github.com/agent-of-empires/agent-of-empires/commit/add1816307f124eb0d2f010bf73434944d780f04))
- **web:** Remove diff file count badge from top bar in [#786](https://github.com/agent-of-empires/agent-of-empires/pull/786) by [@njbrake](https://github.com/njbrake) ([`bb9778b`](https://github.com/agent-of-empires/agent-of-empires/commit/bb9778b7296400cb26270f20117924a6d12142f8))
- **tui:** Keep TUI responsive during worktree creation in [#790](https://github.com/agent-of-empires/agent-of-empires/pull/790) by [@njbrake](https://github.com/njbrake) ([`558db86`](https://github.com/agent-of-empires/agent-of-empires/commit/558db86d86c9adc28752e523c13386b6a924d3a6))


### Features

- Web Push notifications for the dashboard in [#741](https://github.com/agent-of-empires/agent-of-empires/pull/741) by [@njbrake](https://github.com/njbrake) ([`5a8320e`](https://github.com/agent-of-empires/agent-of-empires/commit/5a8320e5d2468ff10190d88807b15d5cd520e784))
- Prefer Tailscale Funnel over Cloudflare quick tunnel for stable PWA-installable HTTPS in [#744](https://github.com/agent-of-empires/agent-of-empires/pull/744) by [@njbrake](https://github.com/njbrake) ([`7e21f0b`](https://github.com/agent-of-empires/agent-of-empires/commit/7e21f0b46ef4367796136c29e95905bd1798f58a))
- **diff:** Add merge conflict support to diff view in [#747](https://github.com/agent-of-empires/agent-of-empires/pull/747) by [@blaisepic](https://github.com/blaisepic) ([`d2faa0a`](https://github.com/agent-of-empires/agent-of-empires/commit/d2faa0a9065892937081adc839ac437f1c8df176))
- **tui:** Opt-in palette color_mode for 256-color terminals in [#756](https://github.com/agent-of-empires/agent-of-empires/pull/756) by [@BTForIT](https://github.com/BTForIT) ([`360600f`](https://github.com/agent-of-empires/agent-of-empires/commit/360600f2e9d5374915e510e65fed973eabc4433d))
- **tui:** Opt-in strict_hotkeys mode — require Shift/Ctrl for destructive actions in [#755](https://github.com/agent-of-empires/agent-of-empires/pull/755) by [@BTForIT](https://github.com/BTForIT) ([`2809052`](https://github.com/agent-of-empires/agent-of-empires/commit/2809052c3b417cb1dc1dd6157f70f49463383177))
- **web:** Primary-client model for multi-device terminal resize in [#761](https://github.com/agent-of-empires/agent-of-empires/pull/761) by [@njbrake](https://github.com/njbrake) ([`86882ef`](https://github.com/agent-of-empires/agent-of-empires/commit/86882efe3da53974bcfc6b3b49e2d25a7db82f49))
- **git:** Fetch remote before creating worktrees in [#763](https://github.com/agent-of-empires/agent-of-empires/pull/763) by [@njbrake](https://github.com/njbrake) ([`9e1896d`](https://github.com/agent-of-empires/agent-of-empires/commit/9e1896d228d15af93fbe0e0609fcd096a44c1d88))
- **tui:** Last-activity column + LastActivity sort in [#762](https://github.com/agent-of-empires/agent-of-empires/pull/762) by [@BTForIT](https://github.com/BTForIT) ([`16bdfad`](https://github.com/agent-of-empires/agent-of-empires/commit/16bdfad3a83247dd8f01eb2b2239df17b7f08bf1))
- **ci:** Add Playwright tests to GitHub Actions (#764) in [#767](https://github.com/agent-of-empires/agent-of-empires/pull/767) by [@njbrake](https://github.com/njbrake) ([`52f39f4`](https://github.com/agent-of-empires/agent-of-empires/commit/52f39f48384d5378e78472cdb12225bc8fb9e38a))
- Persist serve passphrase and open session on notification tap in [#770](https://github.com/agent-of-empires/agent-of-empires/pull/770) by [@njbrake](https://github.com/njbrake) ([`ed44287`](https://github.com/agent-of-empires/agent-of-empires/commit/ed44287e0339ee112805298b1d42d8fcf903c907))
- **push:** Suppress notifications when user is actively using aoe in [#773](https://github.com/agent-of-empires/agent-of-empires/pull/773) by [@njbrake](https://github.com/njbrake) ([`930121c`](https://github.com/agent-of-empires/agent-of-empires/commit/930121c1f78b45a97cd19af51ca0b2fa388b9e3c))
- **serve:** Persistent passphrase, full-page view, edit/restart controls in [#775](https://github.com/agent-of-empires/agent-of-empires/pull/775) by [@njbrake](https://github.com/njbrake) ([`ca813d3`](https://github.com/agent-of-empires/agent-of-empires/commit/ca813d342077c9507f3b8e6fc99e7de3a69e7ee7))
- **web:** Syntax highlighting in the diff viewer in [#776](https://github.com/agent-of-empires/agent-of-empires/pull/776) by [@njbrake](https://github.com/njbrake) ([`710d263`](https://github.com/agent-of-empires/agent-of-empires/commit/710d26330156ee42603f83d55a9e6c53df9ebf4e))
- **web:** Keyboard FAB and touch drag handle for paired terminal in [#782](https://github.com/agent-of-empires/agent-of-empires/pull/782) by [@njbrake](https://github.com/njbrake) ([`84e4008`](https://github.com/agent-of-empires/agent-of-empires/commit/84e4008257dd28e73ad492c97658d1f1fa21e054))
- **web:** Focus ring and embedded styling for terminal panels in [#787](https://github.com/agent-of-empires/agent-of-empires/pull/787) by [@njbrake](https://github.com/njbrake) ([`3428053`](https://github.com/agent-of-empires/agent-of-empires/commit/34280532105892e201919d0262d951a16ee3904c))
- Onboarding experience when no AI agents are installed in [#788](https://github.com/agent-of-empires/agent-of-empires/pull/788) by [@njbrake](https://github.com/njbrake) ([`2e89e37`](https://github.com/agent-of-empires/agent-of-empires/commit/2e89e379c6561a4178a5b7f3c9d87fb925f8e96d))
- **web:** File tree in diff viewer with per-file status in [#791](https://github.com/agent-of-empires/agent-of-empires/pull/791) by [@njbrake](https://github.com/njbrake) ([`d5127bf`](https://github.com/agent-of-empires/agent-of-empires/commit/d5127bfe126eb66be1f0758bf3fa73d555170457))
- **web:** Token entry page for re-authentication after token rotation in [#792](https://github.com/agent-of-empires/agent-of-empires/pull/792) by [@njbrake](https://github.com/njbrake) ([`3fa56e9`](https://github.com/agent-of-empires/agent-of-empires/commit/3fa56e98be559d64fa8d322c51f23df0eb670552))


### Other

- Dashboard hardening: WS backoff, CSP, cleanup-cache struct in [#739](https://github.com/agent-of-empires/agent-of-empires/pull/739) by [@njbrake](https://github.com/njbrake) ([`5c3db0d`](https://github.com/agent-of-empires/agent-of-empires/commit/5c3db0d2f70fa640665850d1929dfafbab29f4b7))



### New Contributors

- [@BTForIT](https://github.com/BTForIT) made their first contribution in [#762](https://github.com/agent-of-empires/agent-of-empires/pull/762)
- [@blaisepic](https://github.com/blaisepic) made their first contribution in [#747](https://github.com/agent-of-empires/agent-of-empires/pull/747)
- [@TheSteinn](https://github.com/TheSteinn) made their first contribution in [#743](https://github.com/agent-of-empires/agent-of-empires/pull/743)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.4.5...v1.4.6
## [1.4.5](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.4.5) - 2026-04-18



### Bug Fixes

- **ci:** Skip nix npm hash commit when hash is unchanged in [#702](https://github.com/agent-of-empires/agent-of-empires/pull/702) by [@njbrake](https://github.com/njbrake) ([`980b5a9`](https://github.com/agent-of-empires/agent-of-empires/commit/980b5a9663e0eb2e0d31d871b87d57602331c36a))
- **tui:** Restore inline container/host indicator in terminal session list in [#708](https://github.com/agent-of-empires/agent-of-empires/pull/708) by [@njbrake](https://github.com/njbrake) ([`bbebd00`](https://github.com/agent-of-empires/agent-of-empires/commit/bbebd00a74692e31e4c5a3ab6de1a34630317042))
- **web:** Replace disconnect toast spam with persistent banner in [#711](https://github.com/agent-of-empires/agent-of-empires/pull/711) by [@njbrake](https://github.com/njbrake) ([`3e87c01`](https://github.com/agent-of-empires/agent-of-empires/commit/3e87c0106f526845c3f13d4a596ce7850b39dfd4))
- **ci:** Chain cachix build after npm hash update to prevent race in [#712](https://github.com/agent-of-empires/agent-of-empires/pull/712) by [@njbrake](https://github.com/njbrake) ([`7337550`](https://github.com/agent-of-empires/agent-of-empires/commit/7337550f5107295a7d9ac1f4397d47d6cb6dd649))
- **web:** Persist TUI serve port so dashboard URL stays stable in [#713](https://github.com/agent-of-empires/agent-of-empires/pull/713) by [@njbrake](https://github.com/njbrake) ([`aa2d9fb`](https://github.com/agent-of-empires/agent-of-empires/commit/aa2d9fb8910430e9034e7c271ca74988064fb964))
- **nix:** Restore resolved URLs in package-lock.json in [#714](https://github.com/agent-of-empires/agent-of-empires/pull/714) by [@njbrake](https://github.com/njbrake) ([`01926b8`](https://github.com/agent-of-empires/agent-of-empires/commit/01926b8f1b54741d9a047c571bdc65c30618b7f6))
- **web:** Increase EMPIRES title glow visibility on desktop in [#720](https://github.com/agent-of-empires/agent-of-empires/pull/720) by [@njbrake](https://github.com/njbrake) ([`8caeb21`](https://github.com/agent-of-empires/agent-of-empires/commit/8caeb21a44db4d44963f79965aa9925a7a2e337b))
- **server:** Prevent daemon from dying on SIGHUP/SIGTERM in [#727](https://github.com/agent-of-empires/agent-of-empires/pull/727) by [@njbrake](https://github.com/njbrake) ([`5494e8b`](https://github.com/agent-of-empires/agent-of-empires/commit/5494e8b8b06618f742de4edda29b391df2a63fe7))
- **web:** Prevent mobile sidebar from overlapping header in [#725](https://github.com/agent-of-empires/agent-of-empires/pull/725) by [@njbrake](https://github.com/njbrake) ([`95213be`](https://github.com/agent-of-empires/agent-of-empires/commit/95213bef763822f572506afa7b23e52385f9493b))
- **web:** Mobile UX improvements: sidebar keyboard dismiss, auto-navigate, iOS FAB fix in [#726](https://github.com/agent-of-empires/agent-of-empires/pull/726) by [@njbrake](https://github.com/njbrake) ([`cd0c2c4`](https://github.com/agent-of-empires/agent-of-empires/commit/cd0c2c4f4c36e5f8caa5cfd4c430c4299c4297be))
- **server:** Drop useless .into() flagged by clippy::useless_conversion in [#738](https://github.com/agent-of-empires/agent-of-empires/pull/738) by [@njbrake](https://github.com/njbrake) ([`ea39a4b`](https://github.com/agent-of-empires/agent-of-empires/commit/ea39a4b5b3f43a34f7f6975b07df1d53cac75678))


### Features

- **web:** Mobile-first project creation, profile selection, and settings in [#701](https://github.com/agent-of-empires/agent-of-empires/pull/701) by [@njbrake](https://github.com/njbrake) ([`cb63d06`](https://github.com/agent-of-empires/agent-of-empires/commit/cb63d06bd6bca427536e6500efbeda61ccdec6c3))
- Add aoe-with-web Nix package target with embedded web UI in [#700](https://github.com/agent-of-empires/agent-of-empires/pull/700) by [@gdw2vs](https://github.com/gdw2vs) ([`f22b2ab`](https://github.com/agent-of-empires/agent-of-empires/commit/f22b2abbe3685ee258dc030963c425c63b8795a3))
- **web:** Replace xterm.js with wterm in [#705](https://github.com/agent-of-empires/agent-of-empires/pull/705) by [@njbrake](https://github.com/njbrake) ([`8fd0d7b`](https://github.com/agent-of-empires/agent-of-empires/commit/8fd0d7b68145949529151a763d3a1351ac3fbeb8))
- **web:** Optimistic session creation, sidebar shortcuts, Mac-only Cmd+K in [#709](https://github.com/agent-of-empires/agent-of-empires/pull/709) by [@njbrake](https://github.com/njbrake) ([`d9a63c8`](https://github.com/agent-of-empires/agent-of-empires/commit/d9a63c830a87b1f026dcfc072c324276be09495f))
- **web:** Add per-project "new session" button to dashboard cards in [#710](https://github.com/agent-of-empires/agent-of-empires/pull/710) by [@njbrake](https://github.com/njbrake) ([`7099281`](https://github.com/agent-of-empires/agent-of-empires/commit/70992816ff933d5cda9bd1bc75c716288b502ab2))
- **web:** Ability to delete sessions in [#707](https://github.com/agent-of-empires/agent-of-empires/pull/707) by [@njbrake](https://github.com/njbrake) ([`558bbdc`](https://github.com/agent-of-empires/agent-of-empires/commit/558bbdc589fcd2bc9f6776c2b660fddcebf35311))
- **web:** Show repo owner avatar next to project name in [#716](https://github.com/agent-of-empires/agent-of-empires/pull/716) by [@njbrake](https://github.com/njbrake) ([`aedf6fe`](https://github.com/agent-of-empires/agent-of-empires/commit/aedf6fe4d8b71720b5b4d0ebfc482dd4e2da6cc6))
- **web:** Clone from URL, centered wizard, launch shortcut in [#717](https://github.com/agent-of-empires/agent-of-empires/pull/717) by [@njbrake](https://github.com/njbrake) ([`04cd699`](https://github.com/agent-of-empires/agent-of-empires/commit/04cd6998f5485ab0bdbdabf0adb121c69def412d))
- **web:** Better home screen with branded launch pad in [#719](https://github.com/agent-of-empires/agent-of-empires/pull/719) by [@njbrake](https://github.com/njbrake) ([`566539f`](https://github.com/agent-of-empires/agent-of-empires/commit/566539f4d50304b57d1b1abf7dfc0f685bc434e9))
- **web:** Add right-edge swipe to open diff/shell panel on mobile in [#723](https://github.com/agent-of-empires/agent-of-empires/pull/723) by [@njbrake](https://github.com/njbrake) ([`248f4fa`](https://github.com/agent-of-empires/agent-of-empires/commit/248f4fae82b53a56d92f6e982ccc2aaff7903249))
- **web:** Add virtual keyboard bar to right panel terminal on mobile in [#724](https://github.com/agent-of-empires/agent-of-empires/pull/724) by [@njbrake](https://github.com/njbrake) ([`9650560`](https://github.com/agent-of-empires/agent-of-empires/commit/9650560101690c8a8268839f4841443535b6d83c))
- **web:** IOS mobile terminal improvements (scroll, paste, keyboard, backspace) in [#728](https://github.com/agent-of-empires/agent-of-empires/pull/728) by [@njbrake](https://github.com/njbrake) ([`8a10e5e`](https://github.com/agent-of-empires/agent-of-empires/commit/8a10e5e826943c9f9dd570f73d93a65550473573))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.4.3...v1.4.5
## [1.4.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.4.3) - 2026-04-16



### Bug Fixes

- **web:** Make auth survive iOS PWA home-screen launches in [#694](https://github.com/agent-of-empires/agent-of-empires/pull/694) by [@njbrake](https://github.com/njbrake) ([`ae0b0d6`](https://github.com/agent-of-empires/agent-of-empires/commit/ae0b0d6bc13930e87f48b740f452034de54ff44a))
- **web:** Fix iOS mobile keyboard detection, layout, and key handling in [#696](https://github.com/agent-of-empires/agent-of-empires/pull/696) by [@njbrake](https://github.com/njbrake) ([`12cce28`](https://github.com/agent-of-empires/agent-of-empires/commit/12cce28176a7d1610b67e9c5ced09400a8777d94))
- **tui:** Cursor follows selected session after deletion in [#699](https://github.com/agent-of-empires/agent-of-empires/pull/699) by [@njbrake](https://github.com/njbrake) ([`aec70bb`](https://github.com/agent-of-empires/agent-of-empires/commit/aec70bbc73ccf1aa045d007a372a77c44a886383))


### Features

- **web:** Mobile sidebar swipe + long-press rename in [#695](https://github.com/agent-of-empires/agent-of-empires/pull/695) by [@njbrake](https://github.com/njbrake) ([`d840fad`](https://github.com/agent-of-empires/agent-of-empires/commit/d840fadbff14f9c9c23ae35fc51e785144d303d4))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.4.2...v1.4.3
## [1.4.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.4.2) - 2026-04-15



### Bug Fixes

- **web:** Restart dead agent sessions on attach in [#690](https://github.com/agent-of-empires/agent-of-empires/pull/690) by [@njbrake](https://github.com/njbrake) ([`2b696ac`](https://github.com/agent-of-empires/agent-of-empires/commit/2b696ac23e8f0b1b164fceda6a78da72b08d9a52))


### Features

- **web:** Pinch-to-zoom for terminal font size in [#691](https://github.com/agent-of-empires/agent-of-empires/pull/691) by [@njbrake](https://github.com/njbrake) ([`f9d12dc`](https://github.com/agent-of-empires/agent-of-empires/commit/f9d12dcb9f084280b1e08d99ff73317884b77e1c))
- **tui:** Serve dialog picks local network or Cloudflare tunnel in [#692](https://github.com/agent-of-empires/agent-of-empires/pull/692) by [@njbrake](https://github.com/njbrake) ([`0e91d68`](https://github.com/agent-of-empires/agent-of-empires/commit/0e91d683db2783b1fc4b246b322e403b4eae4b0c))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.4.1...v1.4.2
## [1.4.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.4.1) - 2026-04-15



### Features

- **web:** Working mobile terminal scroll via PTY wheel events in [#688](https://github.com/agent-of-empires/agent-of-empires/pull/688) by [@njbrake](https://github.com/njbrake) ([`2be4b25`](https://github.com/agent-of-empires/agent-of-empires/commit/2be4b2583cb15d67fa41608a1aecdf27ce96ab41))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.4.0...v1.4.1
## [1.4.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.4.0) - 2026-04-15



### Bug Fixes

- **build:** Reinstall web deps when package.json/package-lock.json is newer in [#685](https://github.com/agent-of-empires/agent-of-empires/pull/685) by [@njbrake](https://github.com/njbrake) ([`dc69d61`](https://github.com/agent-of-empires/agent-of-empires/commit/dc69d61451487cac198f3cf4a7e431c744560941))
- Remove stale version field from OpenClaw SKILL.md frontmatter in [#686](https://github.com/agent-of-empires/agent-of-empires/pull/686) by [@njbrake](https://github.com/njbrake) ([`ec22063`](https://github.com/agent-of-empires/agent-of-empires/commit/ec2206315fa02db146cccc5a7154e907a6f9ca2d))
- **tui:** Redisplay passphrase when reopening Remote Access dialog in [#687](https://github.com/agent-of-empires/agent-of-empires/pull/687) by [@njbrake](https://github.com/njbrake) ([`ae74a6a`](https://github.com/agent-of-empires/agent-of-empires/commit/ae74a6a511a79ed359d0d4c7301e53420a6ed0ad))


### Features

- **tui:** Press R for remote access over Cloudflare Tunnel in [#683](https://github.com/agent-of-empires/agent-of-empires/pull/683) by [@njbrake](https://github.com/njbrake) ([`eb0f658`](https://github.com/agent-of-empires/agent-of-empires/commit/eb0f658bd2ca3dd80a00fc7e518b6e9bc9e6ebcc))
- SFX Volume Setting  in [#681](https://github.com/agent-of-empires/agent-of-empires/pull/681) by [@metal-gabe](https://github.com/metal-gabe) ([`751ef74`](https://github.com/agent-of-empires/agent-of-empires/commit/751ef746d9dac7cc31f09760ac917bb074a56f71))
- **web:** DX polish — error context, version, security settings, toasts in [#684](https://github.com/agent-of-empires/agent-of-empires/pull/684) by [@njbrake](https://github.com/njbrake) ([`b2e523f`](https://github.com/agent-of-empires/agent-of-empires/commit/b2e523f28e12d585d868c45540cd31d6301c8fd7))


### Other

- Command palette and top app bar (#655) in [#682](https://github.com/agent-of-empires/agent-of-empires/pull/682) by [@njbrake](https://github.com/njbrake) ([`e5e23f7`](https://github.com/agent-of-empires/agent-of-empires/commit/e5e23f76a3d465dad743eae76138051da020f93f))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.3.0...v1.4.0
## [1.3.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.3.0) - 2026-04-14



### Bug Fixes

- **server:** Serve embedded static files from SPA fallback in [#646](https://github.com/agent-of-empires/agent-of-empires/pull/646) by [@njbrake](https://github.com/njbrake) ([`7e6e992`](https://github.com/agent-of-empires/agent-of-empires/commit/7e6e992966f9dd8ae76f906661806e370d1bd87c))
- Prevent env var secret leakage in docker exec argv in [#647](https://github.com/agent-of-empires/agent-of-empires/pull/647) by [@njbrake](https://github.com/njbrake) ([`6acad72`](https://github.com/agent-of-empires/agent-of-empires/commit/6acad725c7b30678a4cf4f1a8858f9018790226e))
- **sandbox:** Seed GH_TOKEN credential helper in .sandbox-gitconfig in [#653](https://github.com/agent-of-empires/agent-of-empires/pull/653) by [@njbrake](https://github.com/njbrake) ([`2196796`](https://github.com/agent-of-empires/agent-of-empires/commit/21967967e022c18578e976f3b107fbfd27412252))


### Features

- **web:** Add passphrase login as second-factor auth for web dashboard in [#641](https://github.com/agent-of-empires/agent-of-empires/pull/641) by [@njbrake](https://github.com/njbrake) ([`f219d9f`](https://github.com/agent-of-empires/agent-of-empires/commit/f219d9ff0d8ae024e5fa3385963a3660c26c6c86))
- **web:** Mobile terminal UX with virtual key toolbar and touch scroll in [#644](https://github.com/agent-of-empires/agent-of-empires/pull/644) by [@njbrake](https://github.com/njbrake) ([`8df87ff`](https://github.com/agent-of-empires/agent-of-empires/commit/8df87ff3667a0e256290d9943683513b06cfb965))
- **tui:** Allow hooks to run in background with session list spinner in [#639](https://github.com/agent-of-empires/agent-of-empires/pull/639) by [@njbrake](https://github.com/njbrake) ([`6a8447b`](https://github.com/agent-of-empires/agent-of-empires/commit/6a8447b7061def6bbbd1d879a294db182709f2d9))
- **tui:** Smarter session display and group-by-project mode in [#649](https://github.com/agent-of-empires/agent-of-empires/pull/649) by [@njbrake](https://github.com/njbrake) ([`99b0f5a`](https://github.com/agent-of-empires/agent-of-empires/commit/99b0f5a28f680d68fd935b0884f08078daa50110))
- **web:** Per-file diff viewer, resizable splits, dashboard redesign in [#652](https://github.com/agent-of-empires/agent-of-empires/pull/652) by [@njbrake](https://github.com/njbrake) ([`d3cfc19`](https://github.com/agent-of-empires/agent-of-empires/commit/d3cfc191610d6b294474ecaf91ac12c2a5531238))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.2.0...v1.3.0
## [1.2.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.2.0) - 2026-04-13



### Bug Fixes

- Replace invalid .npmrc `before=7d` with `min-release-age=7` in [#589](https://github.com/agent-of-empires/agent-of-empires/pull/589) by [@njbrake](https://github.com/njbrake) ([`1bf2505`](https://github.com/agent-of-empires/agent-of-empires/commit/1bf2505235c20c4ad1ba6ca174627bb5e2dd7696))
- Restore dev-release Cargo profile for faster local builds in [#592](https://github.com/agent-of-empires/agent-of-empires/pull/592) by [@njbrake](https://github.com/njbrake) ([`282edf9`](https://github.com/agent-of-empires/agent-of-empires/commit/282edf9858ba0ca64c2803a8c7d0156d32005d27))
- Limit cargo build parallelism to 4 jobs in [#598](https://github.com/agent-of-empires/agent-of-empires/pull/598) by [@njbrake](https://github.com/njbrake) ([`c10a66b`](https://github.com/agent-of-empires/agent-of-empires/commit/c10a66b81e5bf81a7411f0f4e6e5b1339daf2ffe))
- Apply volume_ignores to parent repo mount in worktree sessions in [#599](https://github.com/agent-of-empires/agent-of-empires/pull/599) by [@njbrake](https://github.com/njbrake) ([`967c2b8`](https://github.com/agent-of-empires/agent-of-empires/commit/967c2b8f8ea5ebf26161e9264572ab010f65392c))
- Resolve hooks from original project path in CLI workspace sessions in [#593](https://github.com/agent-of-empires/agent-of-empires/pull/593) by [@njbrake](https://github.com/njbrake) ([`184cdef`](https://github.com/agent-of-empires/agent-of-empires/commit/184cdeff48720d17dd63ccdca7cea1842a583883))
- Delegate input to branch picker when active in worktree config mode in [#600](https://github.com/agent-of-empires/agent-of-empires/pull/600) by [@njbrake](https://github.com/njbrake) ([`5f1d328`](https://github.com/agent-of-empires/agent-of-empires/commit/5f1d3283afdd4fc1ac7d2012d73829b450153ae2))
- Stop misclassifying idle OpenCode sessions as error in [#583](https://github.com/agent-of-empires/agent-of-empires/pull/583) by [@njbrake](https://github.com/njbrake) ([`70afd30`](https://github.com/agent-of-empires/agent-of-empires/commit/70afd30c9195775d1e61b31a588fd1a1a1289e7f))
- Exit cleanly when parent terminal dies instead of busy-looping in [#609](https://github.com/agent-of-empires/agent-of-empires/pull/609) by [@njbrake](https://github.com/njbrake) ([`e5cf622`](https://github.com/agent-of-empires/agent-of-empires/commit/e5cf62257a7d7049dafea0a4c65e01fb14839aa6))
- Prevent env var secrets from leaking into Docker argv in [#610](https://github.com/agent-of-empires/agent-of-empires/pull/610) by [@njbrake](https://github.com/njbrake) ([`ba70912`](https://github.com/agent-of-empires/agent-of-empires/commit/ba7091280317d260697da9aa9ccf20bd1904fe25))
- Prevent raw JSON resize messages from appearing in web terminal in [#616](https://github.com/agent-of-empires/agent-of-empires/pull/616) by [@njbrake](https://github.com/njbrake) ([`153d1f5`](https://github.com/agent-of-empires/agent-of-empires/commit/153d1f58c342c1bca6b432f4db131391f76d3ebd))
- **web:** Unify sidebar toggle behavior and mobile overlay patterns in [#620](https://github.com/agent-of-empires/agent-of-empires/pull/620) by [@njbrake](https://github.com/njbrake) ([`5a9a037`](https://github.com/agent-of-empires/agent-of-empires/commit/5a9a03717f73263d3ade500a62fb845023bb5c47))
- **web:** Offset spinner animations by session start time in [#627](https://github.com/agent-of-empires/agent-of-empires/pull/627) by [@njbrake](https://github.com/njbrake) ([`1f0aa3b`](https://github.com/agent-of-empires/agent-of-empires/commit/1f0aa3b7c610dc1f5268f19e3cf27765880f273c))
- **tui:** Offset spinner animations by session start time in [#629](https://github.com/agent-of-empires/agent-of-empires/pull/629) by [@njbrake](https://github.com/njbrake) ([`4c5201e`](https://github.com/agent-of-empires/agent-of-empires/commit/4c5201eb3ff308ec0d43feb39a9e7cd99eaa37a9))
- **tui:** Check creation results after event handling to prevent starvation in [#634](https://github.com/agent-of-empires/agent-of-empires/pull/634) by [@njbrake](https://github.com/njbrake) ([`68dfe8f`](https://github.com/agent-of-empires/agent-of-empires/commit/68dfe8f328a73eb30581bbc410925ac98d1f7669))
- **docs:** Add web dashboard nav entry and build-time nav validation in [#637](https://github.com/agent-of-empires/agent-of-empires/pull/637) by [@njbrake](https://github.com/njbrake) ([`bc20a87`](https://github.com/agent-of-empires/agent-of-empires/commit/bc20a87113e679b1c7a0da05998d8661bb94b847))
- **tui:** Settle terminal before tmux attach and redact secrets in logs in [#636](https://github.com/agent-of-empires/agent-of-empires/pull/636) by [@njbrake](https://github.com/njbrake) ([`aaff0c4`](https://github.com/agent-of-empires/agent-of-empires/commit/aaff0c4809ef6ca2d16f55e5a4b2efe9bb407eae))


### Features

- Add experimental web dashboard (aoe serve) in [#587](https://github.com/agent-of-empires/agent-of-empires/pull/587) by [@njbrake](https://github.com/njbrake) ([`15fa3a1`](https://github.com/agent-of-empires/agent-of-empires/commit/15fa3a1c279b1bd61196609f9457f06174d3dd8e))
- Web dashboard UI/UX with full TUI feature parity in [#588](https://github.com/agent-of-empires/agent-of-empires/pull/588) by [@njbrake](https://github.com/njbrake) ([`e59448c`](https://github.com/agent-of-empires/agent-of-empires/commit/e59448c2f16437c7c492c99219016cec1819931d))
- Include web dashboard in release binaries in [#590](https://github.com/agent-of-empires/agent-of-empires/pull/590) by [@njbrake](https://github.com/njbrake) ([`3c8db93`](https://github.com/agent-of-empires/agent-of-empires/commit/3c8db9348e9d1523f42e64e948114302085c1666))
- Replace icon with stacked terminal windows design in [#612](https://github.com/agent-of-empires/agent-of-empires/pull/612) by [@njbrake](https://github.com/njbrake) ([`13dca12`](https://github.com/agent-of-empires/agent-of-empires/commit/13dca129d2e9482d5660bb436a5b2487aaf3bd10))
- Redesign web dashboard with workspace-centric layout in [#607](https://github.com/agent-of-empires/agent-of-empires/pull/607) by [@njbrake](https://github.com/njbrake) ([`91d34bb`](https://github.com/agent-of-empires/agent-of-empires/commit/91d34bb9b2086a406d0181ea118c89399c7242ad))
- **web:** Polish dashboard UI with Geist fonts, neutral palette, and design fixes in [#617](https://github.com/agent-of-empires/agent-of-empires/pull/617) by [@njbrake](https://github.com/njbrake) ([`7fe0479`](https://github.com/agent-of-empires/agent-of-empires/commit/7fe04798718ade7331ddd192f9e6aae352305612))
- **web:** Group sidebar sessions by repository in [#619](https://github.com/agent-of-empires/agent-of-empires/pull/619) by [@njbrake](https://github.com/njbrake) ([`cb7ee18`](https://github.com/agent-of-empires/agent-of-empires/commit/cb7ee184b9931d7a0ab8272eca9c67335a810e29))
- Replace static status icons with animated rattles spinners in [#623](https://github.com/agent-of-empires/agent-of-empires/pull/623) by [@njbrake](https://github.com/njbrake) ([`d39be5a`](https://github.com/agent-of-empires/agent-of-empires/commit/d39be5a4a92343295089f941a4e0290ce91ccdb2))
- Harden web auth with Cloudflare Tunnel, rate limiting, and device tracking in [#621](https://github.com/agent-of-empires/agent-of-empires/pull/621) by [@njbrake](https://github.com/njbrake) ([`b47e4fe`](https://github.com/agent-of-empires/agent-of-empires/commit/b47e4fe2bb39c707b3550e17bdca14b30efc7d4a))
- Support user-defined custom agents in config in [#628](https://github.com/agent-of-empires/agent-of-empires/pull/628) by [@njbrake](https://github.com/njbrake) ([`a16acf0`](https://github.com/agent-of-empires/agent-of-empires/commit/a16acf037d91e1acf5acdcc67a1aaffcaf2ae763))
- **web:** Session creation, dashboard, and sidebar redesign in [#630](https://github.com/agent-of-empires/agent-of-empires/pull/630) by [@njbrake](https://github.com/njbrake) ([`5d02264`](https://github.com/agent-of-empires/agent-of-empires/commit/5d022645b0b9be83648bf3ec1688508482e62940))
- **tui:** Allow force-removing sessions stuck in deleting state in [#631](https://github.com/agent-of-empires/agent-of-empires/pull/631) by [@njbrake](https://github.com/njbrake) ([`a4e8690`](https://github.com/agent-of-empires/agent-of-empires/commit/a4e86906ddeacc71bf7aed400a919eb8b3eceada))


### Other

- Update README.md by [@njbrake](https://github.com/njbrake) ([`b5c49f9`](https://github.com/agent-of-empires/agent-of-empires/commit/b5c49f970e8cbdb8d98ad93d69d17d9cf87ecc4f))
- Update AGENTS.md by [@njbrake](https://github.com/njbrake) ([`8af3a28`](https://github.com/agent-of-empires/agent-of-empires/commit/8af3a283d5cafb5044d733b787e7b35e823f5852))
- Update README to improve project description by [@njbrake](https://github.com/njbrake) ([`1f251cc`](https://github.com/agent-of-empires/agent-of-empires/commit/1f251cc4cc725d61fedb3855ca485267b627c33b))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.1.0...v1.2.0
## [1.1.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.1.0) - 2026-04-06



### Bug Fixes

- Skip opencode SQLite database during sandbox config sync in [#584](https://github.com/agent-of-empires/agent-of-empires/pull/584) by [@njbrake](https://github.com/njbrake) ([`ba2614a`](https://github.com/agent-of-empires/agent-of-empires/commit/ba2614ab69b847548ce3036267d486f7b2c07e04))


### Features

- Add scroll indicators to home navigation list in [#579](https://github.com/agent-of-empires/agent-of-empires/pull/579) by [@hansonkim](https://github.com/hansonkim) ([`530b843`](https://github.com/agent-of-empires/agent-of-empires/commit/530b84385a863b0edc7036b5c6797c829d569a17))
- Add settl (Settlers of Catan) as a supported launch in [#581](https://github.com/agent-of-empires/agent-of-empires/pull/581) by [@njbrake](https://github.com/njbrake) ([`c47035d`](https://github.com/agent-of-empires/agent-of-empires/commit/c47035d9627f8da92c72556e9cbea89e64401820))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.0.2...v1.1.0
## [1.0.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.0.2) - 2026-04-03



### Bug Fixes

- Accept string or array for Vec<String> config fields in [#562](https://github.com/agent-of-empires/agent-of-empires/pull/562) by [@njbrake](https://github.com/njbrake) ([`b526cbd`](https://github.com/agent-of-empires/agent-of-empires/commit/b526cbd370ff269041565acaf5431394e2630117))
- Stop misclassifying custom command sessions as Error in [#565](https://github.com/agent-of-empires/agent-of-empires/pull/565) by [@njbrake](https://github.com/njbrake) ([`9522e59`](https://github.com/agent-of-empires/agent-of-empires/commit/9522e59534da0ae52c90837c15e592faee05fc08))
- Rewrite Claude plugin paths in sandbox in [#566](https://github.com/agent-of-empires/agent-of-empires/pull/566) by [@zerone0x](https://github.com/zerone0x) ([`a042df3`](https://github.com/agent-of-empires/agent-of-empires/commit/a042df350fb71dadd527a618f88bb886376d5079))
- Use resolve_config_with_repo so repo-level config overrides are respected in [#569](https://github.com/agent-of-empires/agent-of-empires/pull/569) by [@njbrake](https://github.com/njbrake) ([`925ab5a`](https://github.com/agent-of-empires/agent-of-empires/commit/925ab5a7b5ad0a4b72cdf0f87b0718e17f32218e))


### Features

- Guard against supply chain attacks with cargo-deny in [#563](https://github.com/agent-of-empires/agent-of-empires/pull/563) by [@njbrake](https://github.com/njbrake) ([`33275ca`](https://github.com/agent-of-empires/agent-of-empires/commit/33275ca4a374b433bb1be116b36063ad2ee9f355))
- Rename Group In Place in [#567](https://github.com/agent-of-empires/agent-of-empires/pull/567) by [@metal-gabe](https://github.com/metal-gabe) ([`34ec9c6`](https://github.com/agent-of-empires/agent-of-empires/commit/34ec9c64ab1db12a263ecb9b1b99aec70d77c816))
- Add on_destroy hook for session teardown in [#574](https://github.com/agent-of-empires/agent-of-empires/pull/574) by [@njbrake](https://github.com/njbrake) ([`760bf5a`](https://github.com/agent-of-empires/agent-of-empires/commit/760bf5a3f3b30d03da11e24f4f280f69b97c2a63))


### Other

- Fix links in the documentation section of README in [#570](https://github.com/agent-of-empires/agent-of-empires/pull/570) by [@UnknownPlatypus](https://github.com/UnknownPlatypus) ([`f548ba3`](https://github.com/agent-of-empires/agent-of-empires/commit/f548ba3793a752e93db4b8783c6a1cfda83b41d9))



### New Contributors

- [@UnknownPlatypus](https://github.com/UnknownPlatypus) made their first contribution in [#570](https://github.com/agent-of-empires/agent-of-empires/pull/570)
- [@zerone0x](https://github.com/zerone0x) made their first contribution in [#566](https://github.com/agent-of-empires/agent-of-empires/pull/566)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.0.1...v1.0.2
## [1.0.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.0.1) - 2026-03-31



### Bug Fixes

- Handle SIGHUP/SIGTERM to prevent PTY leak on terminal close in [#543](https://github.com/agent-of-empires/agent-of-empires/pull/543) by [@njbrake](https://github.com/njbrake) ([`f1b5751`](https://github.com/agent-of-empires/agent-of-empires/commit/f1b5751bdd41cb43ddcdccc8865edde3567aa9a6))
- Periodic sandbox credential refresh to prevent mid-session 401s in [#540](https://github.com/agent-of-empires/agent-of-empires/pull/540) by [@fshot](https://github.com/fshot) ([`61f971e`](https://github.com/agent-of-empires/agent-of-empires/commit/61f971ed47244f686efa66439bfd2a77e7695e16))
- Enable bracketed paste for TUI text input dialogs in [#555](https://github.com/agent-of-empires/agent-of-empires/pull/555) by [@njbrake](https://github.com/njbrake) ([`518eee7`](https://github.com/agent-of-empires/agent-of-empires/commit/518eee71f33434992db873be1438c6d378ba1ffc))
- Apply repo-level sandbox config to containers, rename .aoe to .agent-of-empires in [#558](https://github.com/agent-of-empires/agent-of-empires/pull/558) by [@njbrake](https://github.com/njbrake) ([`5e94abf`](https://github.com/agent-of-empires/agent-of-empires/commit/5e94abf47e20b8be22da8f01093080c744564e34))


### Features

- Add agent_status_hooks setting to disable hook installation in [#544](https://github.com/agent-of-empires/agent-of-empires/pull/544) by [@njbrake](https://github.com/njbrake) ([`f62dc7b`](https://github.com/agent-of-empires/agent-of-empires/commit/f62dc7b43fff2ca27ffbfe50638d654dfe76c5ef))
- Add Factory Droid CLI as a supported agent in [#546](https://github.com/agent-of-empires/agent-of-empires/pull/546) by [@njbrake](https://github.com/njbrake) ([`8b7b02b`](https://github.com/agent-of-empires/agent-of-empires/commit/8b7b02b26cd6afcb99783e9fd2b704e481808ad5))
- Custom theme support via TOML files in [#556](https://github.com/agent-of-empires/agent-of-empires/pull/556) by [@njbrake](https://github.com/njbrake) ([`a91846c`](https://github.com/agent-of-empires/agent-of-empires/commit/a91846c763a9450f5bdf2325045dfc13e78b5b62))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v1.0.0...v1.0.1
## [1.0.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.0.0) - 2026-03-26



### Bug Fixes

- Trust hook status over shell detection in attach_session in [#532](https://github.com/agent-of-empires/agent-of-empires/pull/532) by [@fshot](https://github.com/fshot) ([`f9f588e`](https://github.com/agent-of-empires/agent-of-empires/commit/f9f588e6ef5f40402acb7b2b8f54f0313b084aa9))
- Strip ANSI codes before status detection to fix false Running/Idle in [#533](https://github.com/agent-of-empires/agent-of-empires/pull/533) by [@gdw2vs](https://github.com/gdw2vs) ([`5fc7666`](https://github.com/agent-of-empires/agent-of-empires/commit/5fc76666aa8b9ab5baf2f2da0825ec0984b4a734))
- Use single-quote escaping for custom sandbox instructions in [#535](https://github.com/agent-of-empires/agent-of-empires/pull/535) by [@njbrake](https://github.com/njbrake) ([`5e5066a`](https://github.com/agent-of-empires/agent-of-empires/commit/5e5066a0be4bdc6b8831d3dbb253e1f923943aa9))


### Features

- Widen send message popup to 80% of terminal width in [#530](https://github.com/agent-of-empires/agent-of-empires/pull/530) by [@njbrake](https://github.com/njbrake) ([`6e32fbf`](https://github.com/agent-of-empires/agent-of-empires/commit/6e32fbf0bb8ab91690effc68f9a642b79c44c177))
- Add bun and pnpm to dev sandbox image in [#536](https://github.com/agent-of-empires/agent-of-empires/pull/536) by [@fshot](https://github.com/fshot) ([`034972e`](https://github.com/agent-of-empires/agent-of-empires/commit/034972e6d17aeb9423af662ede318677f5bb2cbc))


### Performance

- Optimize status poller with batched metadata and adaptive polling in [#534](https://github.com/agent-of-empires/agent-of-empires/pull/534) by [@njbrake](https://github.com/njbrake) ([`26c61bf`](https://github.com/agent-of-empires/agent-of-empires/commit/26c61bf74388c704c88ff86eba8f5b06ca50464e))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.18.1...v1.0.0
## [0.18.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.18.1) - 2026-03-24



### Bug Fixes

- Update root social preview with new logo in [#512](https://github.com/agent-of-empires/agent-of-empires/pull/512) by [@njbrake](https://github.com/njbrake) ([`382ddeb`](https://github.com/agent-of-empires/agent-of-empires/commit/382ddeb43b80efff886aaca5df07e055354871ec))
- Add light mode override for text-gray-200 in header nav links in [#514](https://github.com/agent-of-empires/agent-of-empires/pull/514) by [@njbrake](https://github.com/njbrake) ([`11c78e5`](https://github.com/agent-of-empires/agent-of-empires/commit/11c78e5a283257b4fe34fbe60c3c54b3f2ec41aa))
- Validate agent override entries in settings TUI in [#516](https://github.com/agent-of-empires/agent-of-empires/pull/516) by [@njbrake](https://github.com/njbrake) ([`aeaf22e`](https://github.com/agent-of-empires/agent-of-empires/commit/aeaf22e4ca3f693247cd8aec6dc909430e609307))
- Status bar respects user-selected theme in [#518](https://github.com/agent-of-empires/agent-of-empires/pull/518) by [@njbrake](https://github.com/njbrake) ([`bbcf4fb`](https://github.com/agent-of-empires/agent-of-empires/commit/bbcf4fbbcf891f5e5da8e85f63e27b561b973146))
- Support Shift+Enter for newlines in send message dialog in [#519](https://github.com/agent-of-empires/agent-of-empires/pull/519) by [@njbrake](https://github.com/njbrake) ([`2fbbdab`](https://github.com/agent-of-empires/agent-of-empires/commit/2fbbdabd107219f81f0346dd4725a9ed35e2d3d9))
- Subscribe to ElicitationResult hook to unstick waiting status in [#524](https://github.com/agent-of-empires/agent-of-empires/pull/524) by [@njbrake](https://github.com/njbrake) ([`031cbe9`](https://github.com/agent-of-empires/agent-of-empires/commit/031cbe9a26273d1311aa715aaf09889486e75d56))
- Prevent 'q' from quitting TUI while search is active in [#529](https://github.com/agent-of-empires/agent-of-empires/pull/529) by [@njbrake](https://github.com/njbrake) ([`c1313f2`](https://github.com/agent-of-empires/agent-of-empires/commit/c1313f20084d8327ec99e4344387c525f20b1e56))


### Features

- Responsive list panel width on small terminals in [#505](https://github.com/agent-of-empires/agent-of-empires/pull/505) by [@njbrake](https://github.com/njbrake) ([`30f5930`](https://github.com/agent-of-empires/agent-of-empires/commit/30f593044a6ee615ebc42dc95ec53ae1e4983bce))
- Empire theme + rounded borders + panel padding in [#510](https://github.com/agent-of-empires/agent-of-empires/pull/510) by [@njbrake](https://github.com/njbrake) ([`7fd5790`](https://github.com/agent-of-empires/agent-of-empires/commit/7fd57902fb7a92d8d46d08aafe22d44bd6644f70))
- Apply design system to website in [#511](https://github.com/agent-of-empires/agent-of-empires/pull/511) by [@njbrake](https://github.com/njbrake) ([`45f6146`](https://github.com/agent-of-empires/agent-of-empires/commit/45f614667065e06f39e228459ced65ba1cfe7964))
- Add Shift+T shortcut to attach terminal from any view in [#517](https://github.com/agent-of-empires/agent-of-empires/pull/517) by [@njbrake](https://github.com/njbrake) ([`6ee9efb`](https://github.com/agent-of-empires/agent-of-empires/commit/6ee9efb3dfd05dec2487e2068c8800fa4c29446c))
- Support group rename from TUI in [#509](https://github.com/agent-of-empires/agent-of-empires/pull/509) by [@hansonkim](https://github.com/hansonkim) ([`25a46ab`](https://github.com/agent-of-empires/agent-of-empires/commit/25a46abea851c9c040c263e412fc0b056213a5d9))
- Embed YouTube channel uploads playlist with subscribe button in [#522](https://github.com/agent-of-empires/agent-of-empires/pull/522) by [@njbrake](https://github.com/njbrake) ([`650c0e1`](https://github.com/agent-of-empires/agent-of-empires/commit/650c0e1615156c49249348211b03a9c1e78023ce))
- Put profile and tool on the same row in Preview pane in [#527](https://github.com/agent-of-empires/agent-of-empires/pull/527) by [@njbrake](https://github.com/njbrake) ([`269c8cc`](https://github.com/agent-of-empires/agent-of-empires/commit/269c8cce5b176260f272223d2181a72a148d4e05))


### Other

- New logo, social preview, guides migration, dark mode readability in [#513](https://github.com/agent-of-empires/agent-of-empires/pull/513) by [@njbrake](https://github.com/njbrake) ([`35c2e5c`](https://github.com/agent-of-empires/agent-of-empires/commit/35c2e5cb37e2fc88d428f3b23906bf9fd2212475))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.18.0...v0.18.1
## [0.18.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.18.0) - 2026-03-21



### Bug Fixes

- Correct terminal preview info_height off-by-one (#485) in [#490](https://github.com/agent-of-empires/agent-of-empires/pull/490) by [@njbrake](https://github.com/njbrake) ([`665282a`](https://github.com/agent-of-empires/agent-of-empires/commit/665282ae510d370730c68724caa83881c74f6d35))
- Target pane 0 explicitly to avoid false-dead detection on split panes in [#489](https://github.com/agent-of-empires/agent-of-empires/pull/489) by [@patjlm](https://github.com/patjlm) ([`c0d7406`](https://github.com/agent-of-empires/agent-of-empires/commit/c0d74060333e2608cafe46571c126566bf426464))


### Features

- Send message to agent from TUI without attaching in [#502](https://github.com/agent-of-empires/agent-of-empires/pull/502) by [@njbrake](https://github.com/njbrake) ([`b23dda2`](https://github.com/agent-of-empires/agent-of-empires/commit/b23dda2c9f698224099aff8b35428c0cb66cb5bf))



### New Contributors

- [@patjlm](https://github.com/patjlm) made their first contribution in [#489](https://github.com/agent-of-empires/agent-of-empires/pull/489)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.17.1...v0.18.0
## [0.17.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.17.1) - 2026-03-20



### Bug Fixes

- Trust hook status over shell detection for wrapper scripts in [#480](https://github.com/agent-of-empires/agent-of-empires/pull/480) by [@njbrake](https://github.com/njbrake) ([`fc3b52f`](https://github.com/agent-of-empires/agent-of-empires/commit/fc3b52f552126b6b4631956c3f3e23a9df0c3ee1))
- Bare repo misidentified when parent has a spurious .git/ directory in [#484](https://github.com/agent-of-empires/agent-of-empires/pull/484) by [@gdw2vs](https://github.com/gdw2vs) ([`64fc6c1`](https://github.com/agent-of-empires/agent-of-empires/commit/64fc6c1a443be2579ab6e00fe04cebbad1eef22d))
- Preserve tmux ANSI colors in preview capture in [#483](https://github.com/agent-of-empires/agent-of-empires/pull/483) by [@SuatBabatan](https://github.com/SuatBabatan) ([`77e509e`](https://github.com/agent-of-empires/agent-of-empires/commit/77e509ea7e4b4182e107810fa261bfc3eb90297d))
- Make OpenCode config dir writable in sandbox containers in [#487](https://github.com/agent-of-empires/agent-of-empires/pull/487) by [@njbrake](https://github.com/njbrake) ([`c0d9aae`](https://github.com/agent-of-empires/agent-of-empires/commit/c0d9aae26ab80bb67777c1a06f2b1a2843bedd53))


### Features

- Multi-repo workspace support in [#455](https://github.com/agent-of-empires/agent-of-empires/pull/455) by [@njbrake](https://github.com/njbrake) ([`6b325d5`](https://github.com/agent-of-empires/agent-of-empires/commit/6b325d523d5ec197a50c6fba4beb7f45d097783b))
- Pre-filled New Session Dialog from selection (N key) in [#481](https://github.com/agent-of-empires/agent-of-empires/pull/481) by [@njbrake](https://github.com/njbrake) ([`beb9427`](https://github.com/agent-of-empires/agent-of-empires/commit/beb9427045f45dee66a6ec1346f1f48a34175576))



### New Contributors

- [@SuatBabatan](https://github.com/SuatBabatan) made their first contribution in [#483](https://github.com/agent-of-empires/agent-of-empires/pull/483)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.17.0...v0.17.1
## [0.17.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.17.0) - 2026-03-18



### Bug Fixes

- Resolve clawhub publish path with space in bun global directory in [#448](https://github.com/agent-of-empires/agent-of-empires/pull/448) by [@njbrake](https://github.com/njbrake) ([`0209fb7`](https://github.com/agent-of-empires/agent-of-empires/commit/0209fb73cc8910bd66295a2c090f7d7113e2fc98))
- Route all HomeView instance mutations through helpers in [#415](https://github.com/agent-of-empires/agent-of-empires/pull/415) by [@jerome-benoit](https://github.com/jerome-benoit) ([`3d92b79`](https://github.com/agent-of-empires/agent-of-empires/commit/3d92b791e5fa05ab1e63a31bad1e5189bed0fff2))
- Avoid blocking Docker call on main thread during sandbox creation in [#451](https://github.com/agent-of-empires/agent-of-empires/pull/451) by [@fshot](https://github.com/fshot) ([`778eb8b`](https://github.com/agent-of-empires/agent-of-empires/commit/778eb8b36b70a535c6d4767f57779bd0645faf6d))
- Handle sandbox worktree deletion on macOS Docker Desktop in [#471](https://github.com/agent-of-empires/agent-of-empires/pull/471) by [@njbrake](https://github.com/njbrake) ([`e759859`](https://github.com/agent-of-empires/agent-of-empires/commit/e7598598592c214739c991419dc78386d0097dd6))
- Remove misleading managed status from session preview in [#475](https://github.com/agent-of-empires/agent-of-empires/pull/475) by [@njbrake](https://github.com/njbrake) ([`6134dd3`](https://github.com/agent-of-empires/agent-of-empires/commit/6134dd30a1edf2f10210f134957495f4aa4ca722))
- Quote env var values in yolo mode to prevent shell expansion in [#478](https://github.com/agent-of-empires/agent-of-empires/pull/478) by [@jerome-benoit](https://github.com/jerome-benoit) ([`1b651e3`](https://github.com/agent-of-empires/agent-of-empires/commit/1b651e3e21346f4eaf42d10689c6280af21b7868))


### Features

- Add ls alias to group list and worktree list subcommands in [#452](https://github.com/agent-of-empires/agent-of-empires/pull/452) by [@roysha1](https://github.com/roysha1) ([`cbeecee`](https://github.com/agent-of-empires/agent-of-empires/commit/cbeecee5970e56d2b4058f19b1b08217eace1824))
- Remove collapsible profile headers in all-profiles view in [#454](https://github.com/agent-of-empires/agent-of-empires/pull/454) by [@njbrake](https://github.com/njbrake) ([`0c103ac`](https://github.com/agent-of-empires/agent-of-empires/commit/0c103acd8be13fa3a56e8d7d5afa9b5bf5e45d0f))



### New Contributors

- [@roysha1](https://github.com/roysha1) made their first contribution in [#452](https://github.com/agent-of-empires/agent-of-empires/pull/452)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.16.1...v0.17.0
## [0.16.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.16.1) - 2026-03-12



### Bug Fixes

- Update oven-sh/setup-bun SHA to valid commit in [#444](https://github.com/agent-of-empires/agent-of-empires/pull/444) by [@njbrake](https://github.com/njbrake) ([`9c9eef4`](https://github.com/agent-of-empires/agent-of-empires/commit/9c9eef4b5bfedb3dafa085a8a995d5cdf54b8d6f))
- Clawhub publish workaround + ClawHub badge in [#445](https://github.com/agent-of-empires/agent-of-empires/pull/445) by [@njbrake](https://github.com/njbrake) ([`b3130e3`](https://github.com/agent-of-empires/agent-of-empires/commit/b3130e3980f3634b124067f7256d87c3164c39f2))
- Use ^ to target first tmux pane regardless of base-index in [#447](https://github.com/agent-of-empires/agent-of-empires/pull/447) by [@gdw2vs](https://github.com/gdw2vs) ([`552db36`](https://github.com/agent-of-empires/agent-of-empires/commit/552db361fa4e244e3f003a78591317f13160a747))


### Features

- Add support for GitHub Copilot CLI in [#434](https://github.com/agent-of-empires/agent-of-empires/pull/434) by [@nakashon](https://github.com/nakashon) ([`ae12d0d`](https://github.com/agent-of-empires/agent-of-empires/commit/ae12d0de596549108e6e426525ce4c9993e2bb24))



### New Contributors

- [@gdw2vs](https://github.com/gdw2vs) made their first contribution in [#447](https://github.com/agent-of-empires/agent-of-empires/pull/447)
- [@nakashon](https://github.com/nakashon) made their first contribution in [#434](https://github.com/agent-of-empires/agent-of-empires/pull/434)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.16.0...v0.16.1
## [0.16.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.16.0) - 2026-03-12



### Features

- Add acknowledgment dialog for first-time agent hook installation in [#441](https://github.com/agent-of-empires/agent-of-empires/pull/441) by [@njbrake](https://github.com/njbrake) ([`ee113c0`](https://github.com/agent-of-empires/agent-of-empires/commit/ee113c0994663bae1ead829ecc5a06cb01789da4))
- Unified all-profiles TUI view in [#427](https://github.com/agent-of-empires/agent-of-empires/pull/427) by [@fshot](https://github.com/fshot) ([`a67cb52`](https://github.com/agent-of-empires/agent-of-empires/commit/a67cb52dad3bd39335684eade904f06b406383e9))
- Add session capture command and OpenClaw skill in [#442](https://github.com/agent-of-empires/agent-of-empires/pull/442) by [@njbrake](https://github.com/njbrake) ([`e09edc3`](https://github.com/agent-of-empires/agent-of-empires/commit/e09edc39bf53291988f52cc486b42b6f5a50771c))
- Add session capture, OpenClaw skill, and ClawHub publish in [#443](https://github.com/agent-of-empires/agent-of-empires/pull/443) by [@njbrake](https://github.com/njbrake) ([`f8233ee`](https://github.com/agent-of-empires/agent-of-empires/commit/f8233eea45f7eed758f1b2a558508bda94e74949))



### New Contributors

- [@fshot](https://github.com/fshot) made their first contribution in [#427](https://github.com/agent-of-empires/agent-of-empires/pull/427)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.15.2...v0.16.0
## [0.15.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.15.2) - 2026-03-12



### Bug Fixes

- Respect default_tool and yolo_mode_default config in aoe add (#408) in [#418](https://github.com/agent-of-empires/agent-of-empires/pull/418) by [@jerome-benoit](https://github.com/jerome-benoit) ([`8cd7804`](https://github.com/agent-of-empires/agent-of-empires/commit/8cd7804a179219d0c2ccd4463543a5641b79308f))
- Use $SHELL instead of hardcoded bash for agent launch and hook execution in [#426](https://github.com/agent-of-empires/agent-of-empires/pull/426) by [@jerome-benoit](https://github.com/jerome-benoit) ([`f064a50`](https://github.com/agent-of-empires/agent-of-empires/commit/f064a503e42b2901093836226b39c3d727d8e154))
- Correct inner_width calculation in profile picker error wrapping in [#416](https://github.com/agent-of-empires/agent-of-empires/pull/416) by [@jerome-benoit](https://github.com/jerome-benoit) ([`4e5a5d5`](https://github.com/agent-of-empires/agent-of-empires/commit/4e5a5d58a727f9f635579fdbe189b64103371732))
- Rename tmux session before mutating instance title in [#432](https://github.com/agent-of-empires/agent-of-empires/pull/432) by [@njbrake](https://github.com/njbrake) ([`4b18f60`](https://github.com/agent-of-empires/agent-of-empires/commit/4b18f60011c826bce3d4a5712fe1963f0c41546b))
- Target window 0 pane 0 in tmux health checks to prevent session kills in [#440](https://github.com/agent-of-empires/agent-of-empires/pull/440) by [@njbrake](https://github.com/njbrake) ([`f8fc8d0`](https://github.com/agent-of-empires/agent-of-empires/commit/f8fc8d0e965f262f6f9d8a658ef70178f067f10f))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.15.0...v0.15.2
## [0.15.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.15.0) - 2026-03-10



### Bug Fixes

- Restart dead panes in [#383](https://github.com/agent-of-empires/agent-of-empires/pull/383) by [@njbrake](https://github.com/njbrake) ([`170ebb7`](https://github.com/agent-of-empires/agent-of-empires/commit/170ebb7ad4ae03499e3b994aa61ba03061d42218))
- Detect shell panes restored by tmux-resurrect and relaunch agent in [#386](https://github.com/agent-of-empires/agent-of-empires/pull/386) by [@jerome-benoit](https://github.com/jerome-benoit) ([`1dd2bcc`](https://github.com/agent-of-empires/agent-of-empires/commit/1dd2bccf2e8a810e873e98c7d52dff05e3ee950d))
- Preserve relative path structure for sibling worktree container mounts in [#395](https://github.com/agent-of-empires/agent-of-empires/pull/395) by [@njbrake](https://github.com/njbrake) ([`208cac0`](https://github.com/agent-of-empires/agent-of-empires/commit/208cac034d96531d1adb1e05e4c42c1207061524))
- Prevent global setting changes from silently clearing profile overrides in [#396](https://github.com/agent-of-empires/agent-of-empires/pull/396) by [@njbrake](https://github.com/njbrake) ([`2206c4b`](https://github.com/agent-of-empires/agent-of-empires/commit/2206c4b4a689f5ad07cf8bc69731902d5f5f33ec))
- Aoe add now respects config-driven agent_extra_args and agent_command_override in [#397](https://github.com/agent-of-empires/agent-of-empires/pull/397) by [@njbrake](https://github.com/njbrake) ([`5b06d36`](https://github.com/agent-of-empires/agent-of-empires/commit/5b06d360657f4d5a7ca466fd70454122437d9cc9))
- Restore absolute gitdir path before worktree removal in [#400](https://github.com/agent-of-empires/agent-of-empires/pull/400) by [@njbrake](https://github.com/njbrake) ([`ee9c485`](https://github.com/agent-of-empires/agent-of-empires/commit/ee9c485a07648bb8290d4dd666f00f3eeef56431))
- Use env to pass inline env vars with exec on macOS bash 3.2 in [#403](https://github.com/agent-of-empires/agent-of-empires/pull/403) by [@alepar](https://github.com/alepar) ([`385d1d9`](https://github.com/agent-of-empires/agent-of-empires/commit/385d1d9e6d1e6402c6d6c395489e83d651a41b45))
- Scope remain-on-exit to pane level to avoid bleeding into non-aoe panes in [#402](https://github.com/agent-of-empires/agent-of-empires/pull/402) by [@alepar](https://github.com/alepar) ([`9e346d3`](https://github.com/agent-of-empires/agent-of-empires/commit/9e346d3b920f658e99e589e2281353d4fab85b6d))
- Delete sandbox worktree contents via container to avoid permission denied in [#405](https://github.com/agent-of-empires/agent-of-empires/pull/405) by [@njbrake](https://github.com/njbrake) ([`c7b97e9`](https://github.com/agent-of-empires/agent-of-empires/commit/c7b97e9afdfee2be603056a10e3dd1a4f6f2f987))
- Hook exits cleanly for non-AoE Claude instances in [#413](https://github.com/agent-of-empires/agent-of-empires/pull/413) by [@njbrake](https://github.com/njbrake) ([`54aa34f`](https://github.com/agent-of-empires/agent-of-empires/commit/54aa34f2bb03df423f483af22cf475c8533ceafc))
- Replace time-based hook staleness with process-aware liveness checks in [#424](https://github.com/agent-of-empires/agent-of-empires/pull/424) by [@njbrake](https://github.com/njbrake) ([`72fccc9`](https://github.com/agent-of-empires/agent-of-empires/commit/72fccc9b42f8d4452036425078d5552420b687b5))


### Features

- Add weekly codebase review workflow in [#388](https://github.com/agent-of-empires/agent-of-empires/pull/388) by [@njbrake](https://github.com/njbrake) ([`95725e9`](https://github.com/agent-of-empires/agent-of-empires/commit/95725e9367ce0570f4e9db68cfb34dc74992d5a4))
- Hook-based status detection for Claude Code and Cursor in [#390](https://github.com/agent-of-empires/agent-of-empires/pull/390) by [@njbrake](https://github.com/njbrake) ([`7e9f36d`](https://github.com/agent-of-empires/agent-of-empires/commit/7e9f36d5c8774c5e80c25545a11a7d71015bede9))
- Profile picker dialog for P key (#365) in [#384](https://github.com/agent-of-empires/agent-of-empires/pull/384) by [@hansonkim](https://github.com/hansonkim) ([`e565423`](https://github.com/agent-of-empires/agent-of-empires/commit/e565423318e04c433f8c4559cce547a9ac540326))
- Only mount active tool's config into sandbox containers in [#398](https://github.com/agent-of-empires/agent-of-empires/pull/398) by [@njbrake](https://github.com/njbrake) ([`782b4be`](https://github.com/agent-of-empires/agent-of-empires/commit/782b4bea4cb95d6f2aecc869aad0fb64d4b33b5b))
- Add pi.dev coding agent support in [#411](https://github.com/agent-of-empires/agent-of-empires/pull/411) by [@nirok80](https://github.com/nirok80) ([`91f4ce4`](https://github.com/agent-of-empires/agent-of-empires/commit/91f4ce4ff1c85362aecf9c1af1298bb274246b52))



### New Contributors

- [@alepar](https://github.com/alepar) made their first contribution in [#402](https://github.com/agent-of-empires/agent-of-empires/pull/402)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.14.0...v0.15.0
## [0.14.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.14.0) - 2026-03-05



### Bug Fixes

- Unify environment and environment_values into single config in [#369](https://github.com/agent-of-empires/agent-of-empires/pull/369) by [@njbrake](https://github.com/njbrake) ([`a66a4cc`](https://github.com/agent-of-empires/agent-of-empires/commit/a66a4cc08575d664430f8e5116a3c2d9d79ac5e2))
- Prevent ancestor git repo from being mounted into container in [#376](https://github.com/agent-of-empires/agent-of-empires/pull/376) by [@njbrake](https://github.com/njbrake) ([`a408bc8`](https://github.com/agent-of-empires/agent-of-empires/commit/a408bc88f7606d5b47a6ef209c4c26acbe82487e))


### Features

- Offer to create non-existent directory on session submit in [#362](https://github.com/agent-of-empires/agent-of-empires/pull/362) by [@njbrake](https://github.com/njbrake) ([`72f0950`](https://github.com/agent-of-empires/agent-of-empires/commit/72f09505ed6fe1a96a84e2cb057e3c6563b7f28d))
- Add group name autocomplete in new session and rename dialogs in [#359](https://github.com/agent-of-empires/agent-of-empires/pull/359) by [@hansonkim](https://github.com/hansonkim) ([`f677811`](https://github.com/agent-of-empires/agent-of-empires/commit/f6778117a088e3bf24223977dc8413bb3ba7b984))
- Add profile picker and collapse sandbox options in new session dialog in [#367](https://github.com/agent-of-empires/agent-of-empires/pull/367) by [@njbrake](https://github.com/njbrake) ([`6f21eef`](https://github.com/agent-of-empires/agent-of-empires/commit/6f21eef010b271df042329a861a24a0e5dc95f7f))
- Remove git lfs in [#370](https://github.com/agent-of-empires/agent-of-empires/pull/370) by [@njbrake](https://github.com/njbrake) ([`b09f23c`](https://github.com/agent-of-empires/agent-of-empires/commit/b09f23c34edc74e5162dae918a64e59b9ef4cafa))
- Settings TUI UX improvements in [#372](https://github.com/agent-of-empires/agent-of-empires/pull/372) by [@njbrake](https://github.com/njbrake) ([`f5980c9`](https://github.com/agent-of-empires/agent-of-empires/commit/f5980c9ddf9e81fc5d3f47e532ab38d03aab573a))
- Resilient session handling for custom commands in [#373](https://github.com/agent-of-empires/agent-of-empires/pull/373) by [@njbrake](https://github.com/njbrake) ([`0d6c34a`](https://github.com/agent-of-empires/agent-of-empires/commit/0d6c34aaf9ebb60b605250608d697d71dcee1df3))


### Other

- Apple container fix in [#377](https://github.com/agent-of-empires/agent-of-empires/pull/377) by [@lgmars](https://github.com/lgmars) ([`cf05548`](https://github.com/agent-of-empires/agent-of-empires/commit/cf055484a1089c3119aac1a4179277bf084606cd))



### New Contributors

- [@lgmars](https://github.com/lgmars) made their first contribution in [#377](https://github.com/agent-of-empires/agent-of-empires/pull/377)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.13.3...v0.14.0
## [0.13.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.13.3) - 2026-03-04



### Bug Fixes

- Handle bare repos where HEAD points to non-existent branch in [#361](https://github.com/agent-of-empires/agent-of-empires/pull/361) by [@njbrake](https://github.com/njbrake) ([`cca49a3`](https://github.com/agent-of-empires/agent-of-empires/commit/cca49a36de2e1dfcb59805864821336fa2e0c9de))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.13.2...v0.13.3
## [0.13.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.13.2) - 2026-03-03



### Bug Fixes

- Update documentation links to use /docs/ and canonical URLs in [#351](https://github.com/agent-of-empires/agent-of-empires/pull/351) by [@gavmor](https://github.com/gavmor) ([`cbd83fa`](https://github.com/agent-of-empires/agent-of-empires/commit/cbd83fa0e45fbe68a42e0625e6d93f78ff6f08ed))
- Mount common parent for non-bare repo worktrees in sandbox in [#357](https://github.com/agent-of-empires/agent-of-empires/pull/357) by [@njbrake](https://github.com/njbrake) ([`6c78656`](https://github.com/agent-of-empires/agent-of-empires/commit/6c786568cda2c9d5c350547d87ee4da7eef0b39a))


### Features

- A sort ordering system for the session list in [#312](https://github.com/agent-of-empires/agent-of-empires/pull/312) by [@metal-gabe](https://github.com/metal-gabe) ([`332bac0`](https://github.com/agent-of-empires/agent-of-empires/commit/332bac01f77d2351051c50dd75a9b4b9a88e3fe2))



### New Contributors

- [@metal-gabe](https://github.com/metal-gabe) made their first contribution in [#312](https://github.com/agent-of-empires/agent-of-empires/pull/312)
- [@gavmor](https://github.com/gavmor) made their first contribution in [#351](https://github.com/agent-of-empires/agent-of-empires/pull/351)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.13.1...v0.13.2
## [0.13.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.13.1) - 2026-03-01



### Bug Fixes

- Show C-p groups hint only when group field is focused in [#323](https://github.com/agent-of-empires/agent-of-empires/pull/323) by [@hansonkim](https://github.com/hansonkim) ([`c2ce2a5`](https://github.com/agent-of-empires/agent-of-empires/commit/c2ce2a51f9c119dcd53ad6848f23cdf376842514))
- Stopped status would never return from that state in [#324](https://github.com/agent-of-empires/agent-of-empires/pull/324) by [@njbrake](https://github.com/njbrake) ([`870807a`](https://github.com/agent-of-empires/agent-of-empires/commit/870807a5769014a848ec4b6765b8900c2bdb1bfd))
- Output pane freeze in [#325](https://github.com/agent-of-empires/agent-of-empires/pull/325) by [@njbrake](https://github.com/njbrake) ([`c941064`](https://github.com/agent-of-empires/agent-of-empires/commit/c941064937e92356c86f0d4ff4d8554250351b12))
- Validate project path exists before creating session in [#327](https://github.com/agent-of-empires/agent-of-empires/pull/327) by [@hansonkim](https://github.com/hansonkim) ([`6d72398`](https://github.com/agent-of-empires/agent-of-empires/commit/6d723980439a0677e5a4a7407b1ea4c664e86e98))
- Seed .sandbox-gitconfig so git works in Claude Code sandboxes in [#336](https://github.com/agent-of-empires/agent-of-empires/pull/336) by [@njbrake](https://github.com/njbrake) ([`a285481`](https://github.com/agent-of-empires/agent-of-empires/commit/a28548163088b27147fd830244b5c7c04e7ba683))
- E2e harness use dedicated tmux socket in [#344](https://github.com/agent-of-empires/agent-of-empires/pull/344) by [@Roberto-XY](https://github.com/Roberto-XY) ([`b0975e8`](https://github.com/agent-of-empires/agent-of-empires/commit/b0975e8462e0ffb14bba95b6c84aa5825274b45a))


### Features

- Add path autocomplete in new session pane in [#329](https://github.com/agent-of-empires/agent-of-empires/pull/329) by [@njbrake](https://github.com/njbrake) ([`6c0bc76`](https://github.com/agent-of-empires/agent-of-empires/commit/6c0bc76adbf6b01dceec1f75c8a5c75b930d770f))
- Add profile rename command in [#334](https://github.com/agent-of-empires/agent-of-empires/pull/334) by [@njbrake](https://github.com/njbrake) ([`aa03032`](https://github.com/agent-of-empires/agent-of-empires/commit/aa03032bae0a16d3269f1f251ba07dab27bfceee))
- Add Dracula theme in [#338](https://github.com/agent-of-empires/agent-of-empires/pull/338) by [@jerome-benoit](https://github.com/jerome-benoit) ([`de858fc`](https://github.com/agent-of-empires/agent-of-empires/commit/de858fc0e7027f1cc965763dbbfd7297ef537162))
- Add e2e test framework with recording support in [#341](https://github.com/agent-of-empires/agent-of-empires/pull/341) by [@njbrake](https://github.com/njbrake) ([`be43dbb`](https://github.com/agent-of-empires/agent-of-empires/commit/be43dbb69cf107f2e98cfae5677ca092684b7792))
- Post e2e recording GIFs inline on PR comments in [#342](https://github.com/agent-of-empires/agent-of-empires/pull/342) by [@njbrake](https://github.com/njbrake) ([`21a55fa`](https://github.com/agent-of-empires/agent-of-empires/commit/21a55fa378fc9615069c5a7c7b2db3600ccdb1d8))
- Add port mapping support for sandbox containers in [#349](https://github.com/agent-of-empires/agent-of-empires/pull/349) by [@pds](https://github.com/pds) ([`e27f1f4`](https://github.com/agent-of-empires/agent-of-empires/commit/e27f1f44c803abca911b9cbff434645eea5a2625))


### Other

- Duplicate mount points bug by [@njbrake](https://github.com/njbrake) ([`92d2e53`](https://github.com/agent-of-empires/agent-of-empires/commit/92d2e5304dad355114519f22a816de3e68aa37e8))



### New Contributors

- [@pds](https://github.com/pds) made their first contribution in [#349](https://github.com/agent-of-empires/agent-of-empires/pull/349)
- [@Roberto-XY](https://github.com/Roberto-XY) made their first contribution in [#344](https://github.com/agent-of-empires/agent-of-empires/pull/344)
- [@hansonkim](https://github.com/hansonkim) made their first contribution in [#327](https://github.com/agent-of-empires/agent-of-empires/pull/327)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.13.0...v0.13.1
## [0.13.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.13.0) - 2026-02-24



### Bug Fixes

- **nix:** Remove deprecated darwin SDK deps and add flake eval to CI in [#316](https://github.com/agent-of-empires/agent-of-empires/pull/316) by [@jerome-benoit](https://github.com/jerome-benoit) ([`3db658f`](https://github.com/agent-of-empires/agent-of-empires/commit/3db658f6c46a56b4014cdc7bc2c09e8c3459bba5))
- Macos keychain overwriting refreshed token in [#318](https://github.com/agent-of-empires/agent-of-empires/pull/318) by [@njbrake](https://github.com/njbrake) ([`767f724`](https://github.com/agent-of-empires/agent-of-empires/commit/767f72474f5b290b9240706e44584871def472a0))
- Cursor jump on search in [#320](https://github.com/agent-of-empires/agent-of-empires/pull/320) by [@njbrake](https://github.com/njbrake) ([`7b064e2`](https://github.com/agent-of-empires/agent-of-empires/commit/7b064e2462fc70f7612831d7522e24617cdc0365))


### Features

- Better search for quick session access in [#319](https://github.com/agent-of-empires/agent-of-empires/pull/319) by [@njbrake](https://github.com/njbrake) ([`ecd8c9c`](https://github.com/agent-of-empires/agent-of-empires/commit/ecd8c9c9e6e3965e3741bd75e72bdbf95e579bf6))
- Add Cursor CLI (agent) support in [#285](https://github.com/agent-of-empires/agent-of-empires/pull/285) by [@covlllp](https://github.com/covlllp) ([`85e9075`](https://github.com/agent-of-empires/agent-of-empires/commit/85e907558169165abf8ec2ef243082903d2d69a3))


### Other

- Enter clears search by [@njbrake](https://github.com/njbrake) ([`cb0bc04`](https://github.com/agent-of-empires/agent-of-empires/commit/cb0bc04db68d0e84de54882301e9364f4d4eadf1))



### New Contributors

- [@covlllp](https://github.com/covlllp) made their first contribution in [#285](https://github.com/agent-of-empires/agent-of-empires/pull/285)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.12.5...v0.13.0
## [0.12.5](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.12.5) - 2026-02-23



### Bug Fixes

- **website:** Broken brew link in [#307](https://github.com/agent-of-empires/agent-of-empires/pull/307) by [@njbrake](https://github.com/njbrake) ([`81e78f2`](https://github.com/agent-of-empires/agent-of-empires/commit/81e78f2aadfa8bf544dcc4e6af29511354f6e98b))
- Dirpicker scroll offscreen and unintuitive UX(#313) in [#313](https://github.com/agent-of-empires/agent-of-empires/pull/313) by [@njbrake](https://github.com/njbrake) ([`909f61d`](https://github.com/agent-of-empires/agent-of-empires/commit/909f61d411a8a1b5ce30550ddbdda16cbf9860c7))


### Features

- **tui:** Add theme system with 3 built-in themes in [#299](https://github.com/agent-of-empires/agent-of-empires/pull/299) by [@jerome-benoit](https://github.com/jerome-benoit) ([`684397e`](https://github.com/agent-of-empires/agent-of-empires/commit/684397ea3f2a12c5202da6a2f52f600d2f480685))
- Ability to stop container in [#310](https://github.com/agent-of-empires/agent-of-empires/pull/310) by [@njbrake](https://github.com/njbrake) ([`25aaf86`](https://github.com/agent-of-empires/agent-of-empires/commit/25aaf861d6242ff4f64077ef385308ad1b070025))
- **nix:** Add shell completions and enriched meta to flake in [#314](https://github.com/agent-of-empires/agent-of-empires/pull/314) by [@jerome-benoit](https://github.com/jerome-benoit) ([`f3613b6`](https://github.com/agent-of-empires/agent-of-empires/commit/f3613b69e620315c0351a7a8ce61e37a666e3b2a))


### Other

- Worktrees dos by [@njbrake](https://github.com/njbrake) ([`7103d7d`](https://github.com/agent-of-empires/agent-of-empires/commit/7103d7dd481471cf94d6f4919845916b5d5abbc4))
- Agents by [@njbrake](https://github.com/njbrake) ([`dd9bbad`](https://github.com/agent-of-empires/agent-of-empires/commit/dd9bbad090e8b0f8809d3413c2a932ec0f5d9b2a))
- Add Nix flake for building the project in [#309](https://github.com/agent-of-empires/agent-of-empires/pull/309) by [@neunenak](https://github.com/neunenak) ([`57c9d84`](https://github.com/agent-of-empires/agent-of-empires/commit/57c9d84027e84f5122472985ff1ec9ffcceb36cd))



### New Contributors

- [@neunenak](https://github.com/neunenak) made their first contribution in [#309](https://github.com/agent-of-empires/agent-of-empires/pull/309)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.12.4...v0.12.5
## [0.12.4](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.12.4) - 2026-02-19



### Bug Fixes

- Docs view on mobile in [#283](https://github.com/agent-of-empires/agent-of-empires/pull/283) by [@njbrake](https://github.com/njbrake) ([`5db302d`](https://github.com/agent-of-empires/agent-of-empires/commit/5db302ded98c7605262d0f1f9324db40e1444d64))
- Dependabot action(#286) in [#286](https://github.com/agent-of-empires/agent-of-empires/pull/286) by [@njbrake](https://github.com/njbrake) ([`bfe3702`](https://github.com/agent-of-empires/agent-of-empires/commit/bfe3702207edfe3314356f948cc0e4bff652d0f0))


### Features

- Force delete option for git worktrees in [#304](https://github.com/agent-of-empires/agent-of-empires/pull/304) by [@njbrake](https://github.com/njbrake) ([`abe7d82`](https://github.com/agent-of-empires/agent-of-empires/commit/abe7d82e2b89d29d82b3defa92847a8bdaedc10b))
- Allow yolo outside of aoe sandbox in [#305](https://github.com/agent-of-empires/agent-of-empires/pull/305) by [@njbrake](https://github.com/njbrake) ([`648ecb0`](https://github.com/agent-of-empires/agent-of-empires/commit/648ecb0f3674c864f7af194c542293d6558eb3e6))


### Other

- Brew in [#279](https://github.com/agent-of-empires/agent-of-empires/pull/279) by [@njbrake](https://github.com/njbrake) ([`4470fd4`](https://github.com/agent-of-empires/agent-of-empires/commit/4470fd45c54a24b05ba0c4827835fc339dfd6200))
- Update README.md by [@njbrake](https://github.com/njbrake) ([`240b25f`](https://github.com/agent-of-empires/agent-of-empires/commit/240b25f630819b3e67e67c2ecb914b194fa09026))
- Revert "chore(deps): bump @astrojs/sitemap from 3.1.6 to 3.7.0 in /website (#…" in [#294](https://github.com/agent-of-empires/agent-of-empires/pull/294) by [@njbrake](https://github.com/njbrake) ([`c53bafa`](https://github.com/agent-of-empires/agent-of-empires/commit/c53bafaff3efeecd41471daeb75dd2431a229baa))
- Fix worktree repo resolution in [#296](https://github.com/agent-of-empires/agent-of-empires/pull/296) by [@sbillig](https://github.com/sbillig) ([`8471f0e`](https://github.com/agent-of-empires/agent-of-empires/commit/8471f0e09e4f09771bea28a316369ac874916828))
- Pr temp check in [#301](https://github.com/agent-of-empires/agent-of-empires/pull/301) by [@njbrake](https://github.com/njbrake) ([`e98cf6d`](https://github.com/agent-of-empires/agent-of-empires/commit/e98cf6d75464911caaff13ffa2c72062300eba24))
- Dependabot fixing in [#302](https://github.com/agent-of-empires/agent-of-empires/pull/302) by [@njbrake](https://github.com/njbrake) ([`c2c42f1`](https://github.com/agent-of-empires/agent-of-empires/commit/c2c42f19830abfb6474beb82dc08b68654db6a67))



### New Contributors

- [@sbillig](https://github.com/sbillig) made their first contribution in [#296](https://github.com/agent-of-empires/agent-of-empires/pull/296)
- [@reneleonhardt](https://github.com/reneleonhardt) made their first contribution in [#281](https://github.com/agent-of-empires/agent-of-empires/pull/281)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.12.3...v0.12.4
## [0.12.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.12.3) - 2026-02-18



### Other

- Change token to use RELEASE_TOKEN for checkout by [@njbrake](https://github.com/njbrake) ([`ef4d854`](https://github.com/agent-of-empires/agent-of-empires/commit/ef4d85413c218092ffd3e6015cd6495abe79faa1))
- Update release.yml to include HOMEBREW_NO_INSTALL_FROM_API by [@njbrake](https://github.com/njbrake) ([`d95a6c4`](https://github.com/agent-of-empires/agent-of-empires/commit/d95a6c4f9ef1a2c8593cdc7283e9ef44bb08b239))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.12.2...v0.12.3
## [0.12.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.12.2) - 2026-02-18



### Other

- Release action workflow in [#276](https://github.com/agent-of-empires/agent-of-empires/pull/276) by [@njbrake](https://github.com/njbrake) ([`43953b3`](https://github.com/agent-of-empires/agent-of-empires/commit/43953b30cd0fcd8ba9a2b2eb1d1fda057610da80))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.12.1...v0.12.2
## [0.12.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.12.1) - 2026-02-17



### Bug Fixes

- Skip migrations for completion command in [#275](https://github.com/agent-of-empires/agent-of-empires/pull/275) by [@jerome-benoit](https://github.com/jerome-benoit) ([`681b0b1`](https://github.com/agent-of-empires/agent-of-empires/commit/681b0b1e7a698fd40d14e149da969435b80aff81))


### Features

- Add shell completion support in [#261](https://github.com/agent-of-empires/agent-of-empires/pull/261) by [@jerome-benoit](https://github.com/jerome-benoit) ([`1e548cf`](https://github.com/agent-of-empires/agent-of-empires/commit/1e548cf55d72e7b043553c4a9733cadd71253f26))


### Other

- Review pr skill update ([`3d9c6cf`](https://github.com/agent-of-empires/agent-of-empires/commit/3d9c6cfe8b7f7cf5ce2bc0780a8ead102c8e5498))
- Version ([`79dc8f4`](https://github.com/agent-of-empires/agent-of-empires/commit/79dc8f44a24049eb389e7c052a8b4900ebd515b6))



### New Contributors

- [@jerome-benoit](https://github.com/jerome-benoit) made their first contribution in [#275](https://github.com/agent-of-empires/agent-of-empires/pull/275)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.12.0...v0.12.1
## [0.12.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.12.0) - 2026-02-17



### Bug Fixes

- Contribution page render in [#240](https://github.com/agent-of-empires/agent-of-empires/pull/240) by [@njbrake](https://github.com/njbrake) ([`a646f64`](https://github.com/agent-of-empires/agent-of-empires/commit/a646f64579a9a9963c18354c1cac8f0a79e1c415))
- Multiline custom sandbox instructions break sandbox launch in [#263](https://github.com/agent-of-empires/agent-of-empires/pull/263) by [@njbrake](https://github.com/njbrake) ([`14ec0b4`](https://github.com/agent-of-empires/agent-of-empires/commit/14ec0b4239cf29631e0093f0d5db5375b916a5c5))
- Handle dead tmux sessions in [#264](https://github.com/agent-of-empires/agent-of-empires/pull/264) by [@njbrake](https://github.com/njbrake) ([`59be3d7`](https://github.com/agent-of-empires/agent-of-empires/commit/59be3d7aed6bd3ed5afea14a7a58f6a584036054))
- Remove unnamed (anon) volumes in [#271](https://github.com/agent-of-empires/agent-of-empires/pull/271) by [@njbrake](https://github.com/njbrake) ([`ecb5e3b`](https://github.com/agent-of-empires/agent-of-empires/commit/ecb5e3b861b6ff4f5d4628894a89cee3ad926451))


### Features

- Add `session rename` CLI command in [#242](https://github.com/agent-of-empires/agent-of-empires/pull/242) by [@lazyoft](https://github.com/lazyoft) ([`18120f1`](https://github.com/agent-of-empires/agent-of-empires/commit/18120f13a568033f7b8ce97e04cb9b276ee83735))
- Custom Instructions for sandbox Claude/Codex Agents in [#244](https://github.com/agent-of-empires/agent-of-empires/pull/244) by [@njbrake](https://github.com/njbrake) ([`7c307cc`](https://github.com/agent-of-empires/agent-of-empires/commit/7c307cc93758379e1303191721a018fe5115b41c))
- Better custom sandbox instructions edit in [#258](https://github.com/agent-of-empires/agent-of-empires/pull/258) by [@njbrake](https://github.com/njbrake) ([`c2cc324`](https://github.com/agent-of-empires/agent-of-empires/commit/c2cc324ab86a520229d55e1fce4e7d66ed34c0ee))
- Initial support for Apple containers in [#248](https://github.com/agent-of-empires/agent-of-empires/pull/248) by [@njbrake](https://github.com/njbrake) ([`f6841b3`](https://github.com/agent-of-empires/agent-of-empires/commit/f6841b3b24d3e26f93773611fc66041e84824de4))
- Use shared sandbox directories for agent auth instead of docker volumes(#246) in [#246](https://github.com/agent-of-empires/agent-of-empires/pull/246) by [@peteski22](https://github.com/peteski22) ([`457b6c6`](https://github.com/agent-of-empires/agent-of-empires/commit/457b6c6a038000af64420d3bbdf79248b8e67f24))


### Other

- Dir pick in [#243](https://github.com/agent-of-empires/agent-of-empires/pull/243) by [@njbrake](https://github.com/njbrake) ([`adc89f4`](https://github.com/agent-of-empires/agent-of-empires/commit/adc89f41add58e217ebbccd0be8d1262f1e78d36))
- Add Star History section to README by [@njbrake](https://github.com/njbrake) ([`bb3c508`](https://github.com/agent-of-empires/agent-of-empires/commit/bb3c50886b6fc9106e6f0726095d55efd011d864))
- Jq in [#249](https://github.com/agent-of-empires/agent-of-empires/pull/249) by [@njbrake](https://github.com/njbrake) ([`9b2ca69`](https://github.com/agent-of-empires/agent-of-empires/commit/9b2ca699d6564470ed8789273a52be2c58140e4a))
- Add checkbox for AI agent in PR template by [@njbrake](https://github.com/njbrake) ([`3f4e7de`](https://github.com/agent-of-empires/agent-of-empires/commit/3f4e7debdcf4350643e3ccd5a007e77e7ae867e2))
- Version bump ([`ebad8e3`](https://github.com/agent-of-empires/agent-of-empires/commit/ebad8e331383e0ba6f699432120c916e6ee4aca6))



### New Contributors

- [@](https://github.com/) made their first contribution in [#](https://github.com/agent-of-empires/agent-of-empires/pull/)
- [@peteski22](https://github.com/peteski22) made their first contribution in [#246](https://github.com/agent-of-empires/agent-of-empires/pull/246)
- [@dependabot[bot]](https://github.com/dependabot[bot]) made their first contribution in [#255](https://github.com/agent-of-empires/agent-of-empires/pull/255)
- [@lazyoft](https://github.com/lazyoft) made their first contribution in [#242](https://github.com/agent-of-empires/agent-of-empires/pull/242)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.11.2...v0.12.0
## [0.11.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.11.2) - 2026-02-09



### Bug Fixes

- **sandbox:** Apply extra_volumes config when creating containers in [#237](https://github.com/agent-of-empires/agent-of-empires/pull/237) by [@nirok80](https://github.com/nirok80) ([`3a4b112`](https://github.com/agent-of-empires/agent-of-empires/commit/3a4b112629ed1185ff59072c94868eac643c888c))
- Action PR format in [#239](https://github.com/agent-of-empires/agent-of-empires/pull/239) by [@njbrake](https://github.com/njbrake) ([`65ddfd4`](https://github.com/agent-of-empires/agent-of-empires/commit/65ddfd45e2dbfed8a3f04a22f4b2db166e4cdbb4))


### Other

- Bump version to 0.11.2 by [@njbrake](https://github.com/njbrake) ([`0fa377c`](https://github.com/agent-of-empires/agent-of-empires/commit/0fa377c874dab673eb43541c94357aab24d68303))



### New Contributors

- [@nirok80](https://github.com/nirok80) made their first contribution in [#237](https://github.com/agent-of-empires/agent-of-empires/pull/237)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.11.1...v0.11.2
## [0.11.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.11.1) - 2026-02-03



### Bug Fixes

- Don't hang when offline in [#219](https://github.com/agent-of-empires/agent-of-empires/pull/219) by [@njbrake](https://github.com/njbrake) ([`066d6ae`](https://github.com/agent-of-empires/agent-of-empires/commit/066d6aeb97cf02c0bba07b54093ca5f1831aeb38))


### Features

- Filter and select group and branch in new session popup in [#220](https://github.com/agent-of-empires/agent-of-empires/pull/220) by [@njbrake](https://github.com/njbrake) ([`5c6f4e5`](https://github.com/agent-of-empires/agent-of-empires/commit/5c6f4e50d7620c4edfc3e55f3be5da64713dad62))
- Unifying docs style with splash page in [#221](https://github.com/agent-of-empires/agent-of-empires/pull/221) by [@njbrake](https://github.com/njbrake) ([`483f7c3`](https://github.com/agent-of-empires/agent-of-empires/commit/483f7c3649618f00a97888e4d758ab1d675c8129))
- Display ver in TUI in [#224](https://github.com/agent-of-empires/agent-of-empires/pull/224) by [@njbrake](https://github.com/njbrake) ([`aa79d86`](https://github.com/agent-of-empires/agent-of-empires/commit/aa79d86ae99cca5499ac9b95fbbf4788aaa4a5bf))
- Add dynamic contributor count badge to README in [#225](https://github.com/agent-of-empires/agent-of-empires/pull/225) by [@njbrake](https://github.com/njbrake) ([`60ae32b`](https://github.com/agent-of-empires/agent-of-empires/commit/60ae32b30cbe95a9a61cda73814eb1a5352e42bf))
- Docker configure men cpu limits in [#226](https://github.com/agent-of-empires/agent-of-empires/pull/226) by [@njbrake](https://github.com/njbrake) ([`4386a96`](https://github.com/agent-of-empires/agent-of-empires/commit/4386a9633ae6105f961729ac016f87bf364df8ec))
- Optional ssh mount in [#227](https://github.com/agent-of-empires/agent-of-empires/pull/227) by [@njbrake](https://github.com/njbrake) ([`c84c03e`](https://github.com/agent-of-empires/agent-of-empires/commit/c84c03e743f4d4a2dacf5397e535b8223ac54c80))
- Editable hooks and repo level settings tab in [#231](https://github.com/agent-of-empires/agent-of-empires/pull/231) by [@njbrake](https://github.com/njbrake) ([`647cbc6`](https://github.com/agent-of-empires/agent-of-empires/commit/647cbc6fb69c2c186f4bdb418031ca6ec9fce381))
- Better file picker in [#232](https://github.com/agent-of-empires/agent-of-empires/pull/232) by [@njbrake](https://github.com/njbrake) ([`d981457`](https://github.com/agent-of-empires/agent-of-empires/commit/d9814573366fc9314714d1d7d5e951153b0aef91))


### Other

- Update SUMMARY.md by [@njbrake](https://github.com/njbrake) ([`d9324fb`](https://github.com/agent-of-empires/agent-of-empires/commit/d9324fb06f371f544407bf9931d39ee6360b808f))
- Update sounds.md by [@njbrake](https://github.com/njbrake) ([`5211e63`](https://github.com/agent-of-empires/agent-of-empires/commit/5211e63f825dcf8a7f36e3066fee01c52dd75362))
- Leaderboard in [#212](https://github.com/agent-of-empires/agent-of-empires/pull/212) by [@njbrake](https://github.com/njbrake) ([`d55f369`](https://github.com/agent-of-empires/agent-of-empires/commit/d55f369a0a5f6ab9386e56c0c7e3ea57fbb39ff0))
- Credit in [#214](https://github.com/agent-of-empires/agent-of-empires/pull/214) by [@njbrake](https://github.com/njbrake) ([`8efcbe1`](https://github.com/agent-of-empires/agent-of-empires/commit/8efcbe1891030475ef345e062879b0178921a9dc))
- Update credits.yml by [@njbrake](https://github.com/njbrake) ([`7f91f2c`](https://github.com/agent-of-empires/agent-of-empires/commit/7f91f2cd86c178a29d9c3fc3b9a8e5ed4db32ab1))
- Credit in [#217](https://github.com/agent-of-empires/agent-of-empires/pull/217) by [@njbrake](https://github.com/njbrake) ([`98f758a`](https://github.com/agent-of-empires/agent-of-empires/commit/98f758a261e4617f17dc9a1617be01826180e993))
- Merge in [#218](https://github.com/agent-of-empires/agent-of-empires/pull/218) by [@njbrake](https://github.com/njbrake) ([`820f827`](https://github.com/agent-of-empires/agent-of-empires/commit/820f82744328e7fce35e78d0f4e4fe537875b56a))
- Yt in [#228](https://github.com/agent-of-empires/agent-of-empires/pull/228) by [@njbrake](https://github.com/njbrake) ([`54980db`](https://github.com/agent-of-empires/agent-of-empires/commit/54980db0a0de5209c02c03669fd0b77413b9bf4c))
- Bump version from 0.11.0 to 0.11.1 by [@njbrake](https://github.com/njbrake) ([`06294d6`](https://github.com/agent-of-empires/agent-of-empires/commit/06294d628a7ce063fb8e7de7a6ab8a6273147bbe))



### New Contributors

- [@github-actions[bot]](https://github.com/github-actions[bot]) made their first contribution in [#](https://github.com/agent-of-empires/agent-of-empires/pull/)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.11.0...v0.11.1
## [0.11.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.11.0) - 2026-02-02



### Bug Fixes

- Diff shows merge view like github in [#207](https://github.com/agent-of-empires/agent-of-empires/pull/207) by [@njbrake](https://github.com/njbrake) ([`d77d19b`](https://github.com/agent-of-empires/agent-of-empires/commit/d77d19bf68ba5d402c6751dd5f7e2bec9ddcdfaa))


### Features

- Optional sounds! in [#211](https://github.com/agent-of-empires/agent-of-empires/pull/211) by [@njbrake](https://github.com/njbrake) ([`c297272`](https://github.com/agent-of-empires/agent-of-empires/commit/c297272780abcbd9a273ddf8d6ce7709bee88c44))


### Other

- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`cb30c99`](https://github.com/agent-of-empires/agent-of-empires/commit/cb30c99265e1535f7ce52877ee290afcf2bc1b1e))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.10.1...v0.11.0
## [0.10.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.10.1) - 2026-02-01



### Bug Fixes

- Profile should override global in [#190](https://github.com/agent-of-empires/agent-of-empires/pull/190) by [@njbrake](https://github.com/njbrake) ([`dafe02c`](https://github.com/agent-of-empires/agent-of-empires/commit/dafe02c8eca41d13cc712de754c86273ca5e7c77))
- Race condition for tmux resizing on sandbox creation in [#201](https://github.com/agent-of-empires/agent-of-empires/pull/201) by [@njbrake](https://github.com/njbrake) ([`16739fb`](https://github.com/agent-of-empires/agent-of-empires/commit/16739fbdf8d3fa7711e80b15b1ff6dd8e81dfd61))


### Features

- Configureable dir ignores between sandbox and host in [#188](https://github.com/agent-of-empires/agent-of-empires/pull/188) by [@njbrake](https://github.com/njbrake) ([`ade917c`](https://github.com/agent-of-empires/agent-of-empires/commit/ade917cb917a706a7d9a4c4379a8dcdb9ad855fe))
- Pass key=val env vars through in [#191](https://github.com/agent-of-empires/agent-of-empires/pull/191) by [@njbrake](https://github.com/njbrake) ([`f21bf25`](https://github.com/agent-of-empires/agent-of-empires/commit/f21bf25dfd67a5de12e0efc73855d1c867d67218))
- `.aoe` per-repo config in [#200](https://github.com/agent-of-empires/agent-of-empires/pull/200) by [@njbrake](https://github.com/njbrake) ([`6843f2a`](https://github.com/agent-of-empires/agent-of-empires/commit/6843f2ac9413cdbe6022998eedd3732493b9af9d))


### Other

- Resize diff view columns in [#187](https://github.com/agent-of-empires/agent-of-empires/pull/187) by [@njbrake](https://github.com/njbrake) ([`2a27d06`](https://github.com/agent-of-empires/agent-of-empires/commit/2a27d0665ba8619729ddac0da1837f22f69e19af))
- Resizeable in [#195](https://github.com/agent-of-empires/agent-of-empires/pull/195) by [@njbrake](https://github.com/njbrake) ([`eda3243`](https://github.com/agent-of-empires/agent-of-empires/commit/eda32431a65c44345dd7d3dbec0d2095175ccaf4))
- Website pages for usage guides in [#196](https://github.com/agent-of-empires/agent-of-empires/pull/196) by [@njbrake](https://github.com/njbrake) ([`d1076bd`](https://github.com/agent-of-empires/agent-of-empires/commit/d1076bdefbee45364d5ac9383f53af6071b77570))
- Prune stale worktrees and log errors in [#197](https://github.com/agent-of-empires/agent-of-empires/pull/197) by [@njbrake](https://github.com/njbrake) ([`03d469e`](https://github.com/agent-of-empires/agent-of-empires/commit/03d469e0485a5968949f5c787ce3e2a9bd3a3ec4))
- If branch isn't local, look for remote in [#198](https://github.com/agent-of-empires/agent-of-empires/pull/198) by [@njbrake](https://github.com/njbrake) ([`1db0d3d`](https://github.com/agent-of-empires/agent-of-empires/commit/1db0d3ddb11d521adfa68742a131ddbbe02317b9))
- Remember resize in [#199](https://github.com/agent-of-empires/agent-of-empires/pull/199) by [@njbrake](https://github.com/njbrake) ([`7047d43`](https://github.com/agent-of-empires/agent-of-empires/commit/7047d43cc8efba1f90474e93db9ff473e100e4db))
- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`cb56362`](https://github.com/agent-of-empires/agent-of-empires/commit/cb563622ec07d9f7d69d1eb236b0b9887738de57))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.10.0...v0.10.1
## [0.10.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.10.0) - 2026-01-29



### Features

- Move website to astro in [#183](https://github.com/agent-of-empires/agent-of-empires/pull/183) by [@njbrake](https://github.com/njbrake) ([`dd64998`](https://github.com/agent-of-empires/agent-of-empires/commit/dd649988a09a5a2ea50f5b4a05aefb7df4cb444f))
- View and edit the diff in the TUI! in [#186](https://github.com/agent-of-empires/agent-of-empires/pull/186) by [@njbrake](https://github.com/njbrake) ([`53c8ec3`](https://github.com/agent-of-empires/agent-of-empires/commit/53c8ec31fdfbad81aefc50741f9349bb7b42b2a4))


### Other

- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`5ed849f`](https://github.com/agent-of-empires/agent-of-empires/commit/5ed849f25ab946c42e7e9ee9e727ecff516d6a10))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.9.0...v0.10.0
## [0.9.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.9.0) - 2026-01-28



### Features

- Map in the whole git repo, set work dir to worktree in [#181](https://github.com/agent-of-empires/agent-of-empires/pull/181) by [@njbrake](https://github.com/njbrake) ([`82b4468`](https://github.com/agent-of-empires/agent-of-empires/commit/82b4468cde4fe6c052fc5bb6007b145462e31aeb))
- Support Gemini CLI in [#182](https://github.com/agent-of-empires/agent-of-empires/pull/182) by [@njbrake](https://github.com/njbrake) ([`05479e3`](https://github.com/agent-of-empires/agent-of-empires/commit/05479e3eb0bc67417f5a12342d0f00b98d4f5458))


### Other

- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`1b4f59f`](https://github.com/agent-of-empires/agent-of-empires/commit/1b4f59f2b2011be4546989b61ef1ab74dfae7a8a))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.8.3...v0.9.0
## [0.8.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.8.3) - 2026-01-28



### Bug Fixes

- Sitemap url in [#173](https://github.com/agent-of-empires/agent-of-empires/pull/173) by [@njbrake](https://github.com/njbrake) ([`10acab4`](https://github.com/agent-of-empires/agent-of-empires/commit/10acab43d3e1323dc2f4239c0f010d540e9f7b23))
- Correctly detect bare repos when running from worktree directory in [#174](https://github.com/agent-of-empires/agent-of-empires/pull/174) by [@njbrake](https://github.com/njbrake) ([`87fc666`](https://github.com/agent-of-empires/agent-of-empires/commit/87fc6663ae3b02aaa2d6ed193acd5e996c0f1e8e))
- Site build script in [#176](https://github.com/agent-of-empires/agent-of-empires/pull/176) by [@njbrake](https://github.com/njbrake) ([`9adc15e`](https://github.com/agent-of-empires/agent-of-empires/commit/9adc15eee38a5e748a8f3995a952e7c89b5b21ce))


### Features

- Ability to move session to different profile in [#177](https://github.com/agent-of-empires/agent-of-empires/pull/177) by [@njbrake](https://github.com/njbrake) ([`daed053`](https://github.com/agent-of-empires/agent-of-empires/commit/daed0539def7d2b04080192d95b75fe12d0d0c87))
- Ability to add extra env vars to single container in [#178](https://github.com/agent-of-empires/agent-of-empires/pull/178) by [@njbrake](https://github.com/njbrake) ([`fd6a685`](https://github.com/agent-of-empires/agent-of-empires/commit/fd6a685b082398fda765d48a28cabf354c41dfa1))
- Terminal can connect to either host or sandbox in [#180](https://github.com/agent-of-empires/agent-of-empires/pull/180) by [@njbrake](https://github.com/njbrake) ([`048a775`](https://github.com/agent-of-empires/agent-of-empires/commit/048a775d0c6356510936a3165538ea6a53483a2d))


### Other

- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`bff6731`](https://github.com/agent-of-empires/agent-of-empires/commit/bff67315d9814cd2a0def0468ccb02392701f2be))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.8.2...v0.8.3
## [0.8.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.8.2) - 2026-01-27



### Features

- Option to delete branch when deleting worktree in [#170](https://github.com/agent-of-empires/agent-of-empires/pull/170) by [@njbrake](https://github.com/njbrake) ([`9a0a76a`](https://github.com/agent-of-empires/agent-of-empires/commit/9a0a76ac53e6d25a5c207f83c029ec94e59c8af6))


### Other

- Merge branch 'main' of github.com:njbrake/agent-of-empires by [@njbrake](https://github.com/njbrake) ([`9d3986f`](https://github.com/agent-of-empires/agent-of-empires/commit/9d3986f4e8381703f37dc0211007a19636576428))
- Patches for mistral sandboxing and new gif by [@njbrake](https://github.com/njbrake) ([`e16a94e`](https://github.com/agent-of-empires/agent-of-empires/commit/e16a94e8fe89e54cd0d35a05d205a4595469b207))
- Website links by [@njbrake](https://github.com/njbrake) ([`e3c6ee4`](https://github.com/agent-of-empires/agent-of-empires/commit/e3c6ee422ccd16d9b57931db43eb7959451d9c02))
- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`4920859`](https://github.com/agent-of-empires/agent-of-empires/commit/4920859249648dbd58483c946a67cc988e541a0d))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.8.1...v0.8.2
## [0.8.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.8.1) - 2026-01-27



### Other

- Mistral sandbox by [@njbrake](https://github.com/njbrake) ([`fc827d5`](https://github.com/agent-of-empires/agent-of-empires/commit/fc827d54ddc4f25fd1b7f85367ab7895e9c26fb5))
- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`ade5d53`](https://github.com/agent-of-empires/agent-of-empires/commit/ade5d531a18f7606bb7d2f06cd911e4e0760ed8f))
- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`c8625d1`](https://github.com/agent-of-empires/agent-of-empires/commit/c8625d1c56697493d20b2e8f7c36b9c19e1347f3))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.8.0...v0.8.1
## [0.8.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.8.0) - 2026-01-27



### Bug Fixes

- Fix error wrappng and vol mount by [@njbrake](https://github.com/njbrake) ([`1641b24`](https://github.com/agent-of-empires/agent-of-empires/commit/1641b241a9cffb2ebc5e15d397dcf0cdea477b13))


### Features

- Splashpage for website in [#165](https://github.com/agent-of-empires/agent-of-empires/pull/165) by [@njbrake](https://github.com/njbrake) ([`bbeff5e`](https://github.com/agent-of-empires/agent-of-empires/commit/bbeff5ef1188ec60e8f5fe3aaadc7759f39827cb))
- Support mistral vibe in [#168](https://github.com/agent-of-empires/agent-of-empires/pull/168) by [@njbrake](https://github.com/njbrake) ([`b1f3c90`](https://github.com/agent-of-empires/agent-of-empires/commit/b1f3c90b57a8cf69d186eaedad74f69de910fe88))


### Other

- Update index.html by [@njbrake](https://github.com/njbrake) ([`f3eee6a`](https://github.com/agent-of-empires/agent-of-empires/commit/f3eee6a362c916f615b065518fb6c0cc07c1ce94))
- Website by [@njbrake](https://github.com/njbrake) ([`79bc812`](https://github.com/agent-of-empires/agent-of-empires/commit/79bc812cfb30144f7f205c16de41f62c029ec430))
- Scripts by [@njbrake](https://github.com/njbrake) ([`5bec588`](https://github.com/agent-of-empires/agent-of-empires/commit/5bec58870575eff4dfdbf3dfaa84682c8e94bb6d))
- Cleanup by [@njbrake](https://github.com/njbrake) ([`eda9bca`](https://github.com/agent-of-empires/agent-of-empires/commit/eda9bca35972256ff1beecaf989e87780c0cc411))
- Chmod by [@njbrake](https://github.com/njbrake) ([`ae25fb7`](https://github.com/agent-of-empires/agent-of-empires/commit/ae25fb721ae32dfc3f1006b3e9ff6e895130976b))
- Update installation.md by [@njbrake](https://github.com/njbrake) ([`272cd99`](https://github.com/agent-of-empires/agent-of-empires/commit/272cd99c2d24f6a8746326fe663586ba5ebdc30e))
- Update index.html by [@njbrake](https://github.com/njbrake) ([`4e2fe49`](https://github.com/agent-of-empires/agent-of-empires/commit/4e2fe4976106ebcf740483b4e22a5a98a9bfc7a7))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.7.0...v0.8.0
## [0.7.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.7.0) - 2026-01-26



### Bug Fixes

- **docker:** Git lfs in sandbox in [#162](https://github.com/agent-of-empires/agent-of-empires/pull/162) by [@njbrake](https://github.com/njbrake) ([`f2f2f80`](https://github.com/agent-of-empires/agent-of-empires/commit/f2f2f808730dbebd0d21295df1042e2b83a5b5e8))


### Features

- **tui:** Ability to rename group in [#163](https://github.com/agent-of-empires/agent-of-empires/pull/163) by [@njbrake](https://github.com/njbrake) ([`234cb62`](https://github.com/agent-of-empires/agent-of-empires/commit/234cb62498aa779282a3d85b5a0384a4b427cddd))
- Mouse mode as an option in [#164](https://github.com/agent-of-empires/agent-of-empires/pull/164) by [@njbrake](https://github.com/njbrake) ([`5646863`](https://github.com/agent-of-empires/agent-of-empires/commit/5646863dddc2629556af511e985322d7c50966ab))


### Other

- Ai statement in [#153](https://github.com/agent-of-empires/agent-of-empires/pull/153) by [@njbrake](https://github.com/njbrake) ([`47ee69d`](https://github.com/agent-of-empires/agent-of-empires/commit/47ee69d19defe5b318d875a30cd6138fa4bf70b1))
- Update config.yml by [@njbrake](https://github.com/njbrake) ([`eb43e91`](https://github.com/agent-of-empires/agent-of-empires/commit/eb43e9182c25b030eab8e51deab06e7f9840e262))
- Remove emdashes in [#158](https://github.com/agent-of-empires/agent-of-empires/pull/158) by [@njbrake](https://github.com/njbrake) ([`f126bd3`](https://github.com/agent-of-empires/agent-of-empires/commit/f126bd3706e2be55a8385e9614f099825e186cd5))
- Update README.md by [@njbrake](https://github.com/njbrake) ([`685f2ba`](https://github.com/agent-of-empires/agent-of-empires/commit/685f2ba100c98350f4a768b4bdabd294c254d5af))
- Experimental settings page in TUI in [#155](https://github.com/agent-of-empires/agent-of-empires/pull/155) by [@njbrake](https://github.com/njbrake) ([`484fbe9`](https://github.com/agent-of-empires/agent-of-empires/commit/484fbe9b6b98b538a51e029b63f92906bea707d4))
- Update AGENTS.md by [@njbrake](https://github.com/njbrake) ([`3252c8e`](https://github.com/agent-of-empires/agent-of-empires/commit/3252c8e0993d55b021eb2259d96f2f47bfc4cd2f))
- Settings TUI cleanup in [#161](https://github.com/agent-of-empires/agent-of-empires/pull/161) by [@njbrake](https://github.com/njbrake) ([`3c611c8`](https://github.com/agent-of-empires/agent-of-empires/commit/3c611c8acbddefb71ec5266648546980d18cdf46))
- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`5e011e8`](https://github.com/agent-of-empires/agent-of-empires/commit/5e011e8d72707cd6db4ee593c88d67d5510fba1d))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.6.2...v0.7.0
## [0.6.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.2) - 2026-01-23



### Bug Fixes

- Suspending of agent with no way to recover in [#152](https://github.com/agent-of-empires/agent-of-empires/pull/152) by [@njbrake](https://github.com/njbrake) ([`86eadce`](https://github.com/agent-of-empires/agent-of-empires/commit/86eadcec67a93c6b25ad4ed5da9c922776e3350c))


### Other

- Not all processes killed when closing session in [#151](https://github.com/agent-of-empires/agent-of-empires/pull/151) by [@njbrake](https://github.com/njbrake) ([`e378b63`](https://github.com/agent-of-empires/agent-of-empires/commit/e378b63bcb4fe8fde6da6f9f205be43f44b3d6c1))
- Bump version to 0.6.2 by [@njbrake](https://github.com/njbrake) ([`cc4b257`](https://github.com/agent-of-empires/agent-of-empires/commit/cc4b257706c0c8cd7632d117b57fe4ff79946140))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.6.1...v0.6.2
## [0.6.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.1) - 2026-01-23



### Other

- Bump version from 0.5.7 to 0.6.1 by [@njbrake](https://github.com/njbrake) ([`b4233f8`](https://github.com/agent-of-empires/agent-of-empires/commit/b4233f8c251f4d739410b0222980748fe9c95c4f))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.6.0...v0.6.1
## [0.6.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0) - 2026-01-23



### Bug Fixes

- Fix metrics reporting in [#137](https://github.com/agent-of-empires/agent-of-empires/pull/137) by [@njbrake](https://github.com/njbrake) ([`3e37f2c`](https://github.com/agent-of-empires/agent-of-empires/commit/3e37f2cd0b288d6ab3c0f4162cc39122a4a4c02b))


### Features

- Better message for image pull in [#146](https://github.com/agent-of-empires/agent-of-empires/pull/146) by [@njbrake](https://github.com/njbrake) ([`043dad9`](https://github.com/agent-of-empires/agent-of-empires/commit/043dad9c8f019075e0dcf5f427f9fcd3d0ad9af2))
- Trim whitespace for args when creating new session in [#148](https://github.com/agent-of-empires/agent-of-empires/pull/148) by [@njbrake](https://github.com/njbrake) ([`083787f`](https://github.com/agent-of-empires/agent-of-empires/commit/083787f54a2f544ec3650e466c09ac003335f2cc))
- Support git bare repos in [#147](https://github.com/agent-of-empires/agent-of-empires/pull/147) by [@njbrake](https://github.com/njbrake) ([`e1a3caa`](https://github.com/agent-of-empires/agent-of-empires/commit/e1a3caa837fe9a7c1c8df1968fab788d6dd570c4))


### Other

- Correct capitalization of 'AoE' in README by [@njbrake](https://github.com/njbrake) ([`73775ce`](https://github.com/agent-of-empires/agent-of-empires/commit/73775ce26aa9d4b47730046958a2f0f40fe539cd))
- Support codex in [#149](https://github.com/agent-of-empires/agent-of-empires/pull/149) by [@njbrake](https://github.com/njbrake) ([`741450c`](https://github.com/agent-of-empires/agent-of-empires/commit/741450c2a1559e6b5548cb5f1b651a6998f182a8))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.5.7...v0.6.0
## [0.5.7](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.5.7) - 2026-01-21



### Bug Fixes

- Custom sandbox images were ignored in [#136](https://github.com/agent-of-empires/agent-of-empires/pull/136) by [@njbrake](https://github.com/njbrake) ([`88578b5`](https://github.com/agent-of-empires/agent-of-empires/commit/88578b513def46edf60b0fa9b19c113de78019db))


### Other

- Cargo version check in [#133](https://github.com/agent-of-empires/agent-of-empires/pull/133) by [@njbrake](https://github.com/njbrake) ([`a99196a`](https://github.com/agent-of-empires/agent-of-empires/commit/a99196a35bfac0e80b67fea41a28f07786ca2690))
- Update guidelines for backwards compatibility and comments by [@njbrake](https://github.com/njbrake) ([`0eabbde`](https://github.com/agent-of-empires/agent-of-empires/commit/0eabbdedf985380d9a9be1b4f85673cb1ae1366b))
- Bump version from 0.5.6 to 0.5.7 by [@njbrake](https://github.com/njbrake) ([`e006e16`](https://github.com/agent-of-empires/agent-of-empires/commit/e006e161fed8c82c7ff34ddda28bf30c1b0de52f))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.5.6...v0.5.7
## [0.5.6](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.5.6) - 2026-01-21



### Bug Fixes

- **sandbox:** Tool PATH and allow local only image in [#128](https://github.com/agent-of-empires/agent-of-empires/pull/128) by [@njbrake](https://github.com/njbrake) ([`2a31842`](https://github.com/agent-of-empires/agent-of-empires/commit/2a31842e30db3150d446c7c1d771c742e3cfcea1))
- **tui:** Conditional rendering of attach tooltip hint in [#125](https://github.com/agent-of-empires/agent-of-empires/pull/125) by [@jlamberts](https://github.com/jlamberts) ([`459ee7c`](https://github.com/agent-of-empires/agent-of-empires/commit/459ee7c96d73086034ce17c360361d5bc24b0e36))
- Group deletion should not keep group container in [#129](https://github.com/agent-of-empires/agent-of-empires/pull/129) by [@njbrake](https://github.com/njbrake) ([`8bfcee2`](https://github.com/agent-of-empires/agent-of-empires/commit/8bfcee2e8be0e2211bbb985fc26ec0c989f90cfd))
- Re-expanding groups in [#132](https://github.com/agent-of-empires/agent-of-empires/pull/132) by [@njbrake](https://github.com/njbrake) ([`4ddd39a`](https://github.com/agent-of-empires/agent-of-empires/commit/4ddd39a99baa4eaff3e8065391447b354f48cc38))


### Other

- Update pull request template for clarity and AI usage by [@njbrake](https://github.com/njbrake) ([`fa5eca1`](https://github.com/agent-of-empires/agent-of-empires/commit/fa5eca12b621672f99c86c003f33a0db7ea14765))
- Bump version from 0.5.5 to 0.5.6 by [@njbrake](https://github.com/njbrake) ([`350e301`](https://github.com/agent-of-empires/agent-of-empires/commit/350e30100fa530ae69a1f2562c972edc2df8ca4e))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.5.5...v0.5.6
## [0.5.5](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.5.5) - 2026-01-20



### Bug Fixes

- Fix license file by [@njbrake](https://github.com/njbrake) ([`521e5c2`](https://github.com/agent-of-empires/agent-of-empires/commit/521e5c21ab3e9f7b401e36dbe965dbf98787690f))
- **tui:** Don't render delete option if no sessions in [#107](https://github.com/agent-of-empires/agent-of-empires/pull/107) by [@jlamberts](https://github.com/jlamberts) ([`0208171`](https://github.com/agent-of-empires/agent-of-empires/commit/0208171bee39b19d38dc337299ca71dddc402d2a))
- **sandbox:** Lazily patch volume mount permissions in [#113](https://github.com/agent-of-empires/agent-of-empires/pull/113) by [@njbrake](https://github.com/njbrake) ([`3ae0d4a`](https://github.com/agent-of-empires/agent-of-empires/commit/3ae0d4ab56423504b006120adb4cd0e5b13fc423))
- **sandbox:** Tmux window sizing race condition in [#114](https://github.com/agent-of-empires/agent-of-empires/pull/114) by [@njbrake](https://github.com/njbrake) ([`f03f560`](https://github.com/agent-of-empires/agent-of-empires/commit/f03f560bca6ee928211902990b098d548b05bb57))
- **tui:** Improve startup time in [#117](https://github.com/agent-of-empires/agent-of-empires/pull/117) by [@njbrake](https://github.com/njbrake) ([`e82cc44`](https://github.com/agent-of-empires/agent-of-empires/commit/e82cc444557bf6eb5b541f345c7486ad7ee48f1f))


### Features

- **tui:** Color running terminal status different from running agent in [#112](https://github.com/agent-of-empires/agent-of-empires/pull/112) by [@njbrake](https://github.com/njbrake) ([`f602155`](https://github.com/agent-of-empires/agent-of-empires/commit/f6021556709b7c5483a2b9230aeefd350ea19f60))
- **tui:** Use loading spinner page when launching sandbox in [#106](https://github.com/agent-of-empires/agent-of-empires/pull/106) by [@njbrake](https://github.com/njbrake) ([`beb87ce`](https://github.com/agent-of-empires/agent-of-empires/commit/beb87ce0b2c169d866f66975f718790c41d2d413))
- Add favicon and logo to documentation in [#119](https://github.com/agent-of-empires/agent-of-empires/pull/119) by [@njbrake](https://github.com/njbrake) ([`737bf19`](https://github.com/agent-of-empires/agent-of-empires/commit/737bf195bb2e33e74304ad31acf45fa95029b291))
- **tui:** Little tmux helper message at bottom of session toolbar in [#121](https://github.com/agent-of-empires/agent-of-empires/pull/121) by [@njbrake](https://github.com/njbrake) ([`be938be`](https://github.com/agent-of-empires/agent-of-empires/commit/be938bee6e667a266ebc01c518f3739bf614d38c))


### Other

- Bump version from 0.5.4 to 0.5.5 by [@njbrake](https://github.com/njbrake) ([`bc0b567`](https://github.com/agent-of-empires/agent-of-empires/commit/bc0b5677470f93a7c73c9d42a5a00cdb9ab6eda9))



### New Contributors

- [@jlamberts](https://github.com/jlamberts) made their first contribution in [#107](https://github.com/agent-of-empires/agent-of-empires/pull/107)

**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.5.4...v0.5.5
## [0.5.4](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.5.4) - 2026-01-19



### Bug Fixes

- **sandbox:** Always pull Docker image before creating container in [#104](https://github.com/agent-of-empires/agent-of-empires/pull/104) by [@njbrake](https://github.com/njbrake) ([`f051e0b`](https://github.com/agent-of-empires/agent-of-empires/commit/f051e0b53e449b63ca8de6bfc6b6cab8cf4b66eb))


### Other

- Bump package version to 0.5.4 by [@njbrake](https://github.com/njbrake) ([`2e5948e`](https://github.com/agent-of-empires/agent-of-empires/commit/2e5948edad27078561c6ad5399637fec03a53dd2))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.5.3...v0.5.4
## [0.5.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.5.3) - 2026-01-19



### Bug Fixes

- **tui:** Docker image row not being selected correctly in [#101](https://github.com/agent-of-empires/agent-of-empires/pull/101) by [@njbrake](https://github.com/njbrake) ([`ef839a9`](https://github.com/agent-of-empires/agent-of-empires/commit/ef839a93fc96b98111d821fe2b458935a69e9594))
- **sandbox,linux:** Use root user in dockerfile in [#102](https://github.com/agent-of-empires/agent-of-empires/pull/102) by [@njbrake](https://github.com/njbrake) ([`bb51051`](https://github.com/agent-of-empires/agent-of-empires/commit/bb5105135a6152c005c1d840bcb86e5bad12f41a))


### Other

- Bump package version to 0.5.3 by [@njbrake](https://github.com/njbrake) ([`4034eae`](https://github.com/agent-of-empires/agent-of-empires/commit/4034eae5817848881717c4c2eb0e7191db3849fc))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.5.2...v0.5.3
## [0.5.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.5.2) - 2026-01-19



### Other

- Bump package version to 0.5.2 by [@njbrake](https://github.com/njbrake) ([`0e28251`](https://github.com/agent-of-empires/agent-of-empires/commit/0e282515d9fa12f805d96fb514224495c6b657d0))
- Tmux styling in [#100](https://github.com/agent-of-empires/agent-of-empires/pull/100) by [@njbrake](https://github.com/njbrake) ([`35e9538`](https://github.com/agent-of-empires/agent-of-empires/commit/35e95386e68e2d399e3a54b9a3df7eab18206ccc))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.5.1...v0.5.2
## [0.5.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.5.1) - 2026-01-19



### Features

- Support XDG Base Dir in [#94](https://github.com/agent-of-empires/agent-of-empires/pull/94) by [@njbrake](https://github.com/njbrake) ([`e9d730e`](https://github.com/agent-of-empires/agent-of-empires/commit/e9d730ef95a3d2721cbaa24ad3aa666dd507ae29))
- **tui:** Make terminal coloring distinct in [#97](https://github.com/agent-of-empires/agent-of-empires/pull/97) by [@njbrake](https://github.com/njbrake) ([`c9b3388`](https://github.com/agent-of-empires/agent-of-empires/commit/c9b338867adfbbc3423eea57ddcc0c1351ef9bd8))
- **tui:** Cleaner display and viewing of release notes in [#98](https://github.com/agent-of-empires/agent-of-empires/pull/98) by [@njbrake](https://github.com/njbrake) ([`9aebc44`](https://github.com/agent-of-empires/agent-of-empires/commit/9aebc442d4438479feee5a5fb73835b99caa7ad6))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.5.0...v0.5.1
## [0.5.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.5.0) - 2026-01-18



### Bug Fixes

- **linux+docker:** Use UID 1000 for sandbox user to match host user permissions in [#83](https://github.com/agent-of-empires/agent-of-empires/pull/83) by [@njbrake](https://github.com/njbrake) ([`465cca3`](https://github.com/agent-of-empires/agent-of-empires/commit/465cca382bc650757c965f77d35abd893723b5ea))


### Features

- **TUI:** Terminal view via `t`! Paired terminal sessions for each agent in [#85](https://github.com/agent-of-empires/agent-of-empires/pull/85) by [@njbrake](https://github.com/njbrake) ([`1ef6611`](https://github.com/agent-of-empires/agent-of-empires/commit/1ef6611b9e664e01283aebd01b825a3428c05d90))


### Other

- Revise README description for clarity by [@njbrake](https://github.com/njbrake) ([`60599a1`](https://github.com/agent-of-empires/agent-of-empires/commit/60599a15b9689b90bd799fcd393426d5a27622e9))
- Terminal in [#89](https://github.com/agent-of-empires/agent-of-empires/pull/89) by [@njbrake](https://github.com/njbrake) ([`0a9a995`](https://github.com/agent-of-empires/agent-of-empires/commit/0a9a9952dadf814c0d010cca568ac8277f47fad0))
- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`3d2c74c`](https://github.com/agent-of-empires/agent-of-empires/commit/3d2c74c95b698905ef3dbfed9fc1670277a9b901))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.4.5...v0.5.0
## [0.4.5](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.4.5) - 2026-01-17



### Other

- Bump version from 0.4.3 to 0.4.5 by [@njbrake](https://github.com/njbrake) ([`9b1784c`](https://github.com/agent-of-empires/agent-of-empires/commit/9b1784c498b41dc9b6bcc0ad2821bf637ee39934))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.4.4...v0.4.5
## [0.4.4](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.4.4) - 2026-01-16



### Bug Fixes

- Fall back to attach-session when switch-client fails by [@njbrake](https://github.com/njbrake) ([`aafd218`](https://github.com/agent-of-empires/agent-of-empires/commit/aafd218087f175a8a71583bd8de272bded986119))
- **TUI:** Hang while docker container is spinning down and deleting  in [#73](https://github.com/agent-of-empires/agent-of-empires/pull/73) by [@njbrake](https://github.com/njbrake) ([`ff42af9`](https://github.com/agent-of-empires/agent-of-empires/commit/ff42af99ac5c8cad7ef16d3b2388cc62e19ef399))
- **tui:** Better handling of keyboard commands when deleting  in [#76](https://github.com/agent-of-empires/agent-of-empires/pull/76) by [@njbrake](https://github.com/njbrake) ([`48abf15`](https://github.com/agent-of-empires/agent-of-empires/commit/48abf15c4dc31ac82188dd379886181700395b1e))
- Delete container option should be wired into cli in [#78](https://github.com/agent-of-empires/agent-of-empires/pull/78) by [@njbrake](https://github.com/njbrake) ([`c07a3a9`](https://github.com/agent-of-empires/agent-of-empires/commit/c07a3a9651037bd932a48c36244bd1ddef7dfab9))


### Features

- **tui:** Welcome splash screen and 'whats changed' splash in [#74](https://github.com/agent-of-empires/agent-of-empires/pull/74) by [@njbrake](https://github.com/njbrake) ([`8466db6`](https://github.com/agent-of-empires/agent-of-empires/commit/8466db6b9103f7976401ffbf1a25f510c5a150fa))


### Other

- Dev images in [#67](https://github.com/agent-of-empires/agent-of-empires/pull/67) by [@njbrake](https://github.com/njbrake) ([`9af2573`](https://github.com/agent-of-empires/agent-of-empires/commit/9af25737969bac50952e7727800536de84767800))
- Options when deleting group; in [#75](https://github.com/agent-of-empires/agent-of-empires/pull/75) by [@njbrake](https://github.com/njbrake) ([`e5a3376`](https://github.com/agent-of-empires/agent-of-empires/commit/e5a3376352ca88e2aeeff16d989346b554505698))
- Metric reporting in [#77](https://github.com/agent-of-empires/agent-of-empires/pull/77) by [@njbrake](https://github.com/njbrake) ([`cfa9b24`](https://github.com/agent-of-empires/agent-of-empires/commit/cfa9b245f87103c33b77ae7b5fba9e897e682ca0))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.4.3...v0.4.4
## [0.4.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.4.3) - 2026-01-15



### Features

- **tui:** Toggle profiles with 'P' in [#63](https://github.com/agent-of-empires/agent-of-empires/pull/63) by [@njbrake](https://github.com/njbrake) ([`4f812eb`](https://github.com/agent-of-empires/agent-of-empires/commit/4f812ebaa0b49c6d8fa452bc636284db17fa026a))


### Other

- Faq in [#64](https://github.com/agent-of-empires/agent-of-empires/pull/64) by [@njbrake](https://github.com/njbrake) ([`24f670c`](https://github.com/agent-of-empires/agent-of-empires/commit/24f670c0ef4ee089ead8b0aa0cb3276f9961f22a))
- Bump version to 0.4.3 by [@njbrake](https://github.com/njbrake) ([`2b854cc`](https://github.com/agent-of-empires/agent-of-empires/commit/2b854cc0fcddac804a32215ada87f1d4bf03ef68))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.4.2...v0.4.3
## [0.4.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.4.2) - 2026-01-14



### Bug Fixes

- TUI show when. update available in [#62](https://github.com/agent-of-empires/agent-of-empires/pull/62) by [@njbrake](https://github.com/njbrake) ([`554eac9`](https://github.com/agent-of-empires/agent-of-empires/commit/554eac9c57de2a8a5c75a5c885e873580b501e89))


### Other

- Update AGENTS.md with commenting and testing guidelines by [@njbrake](https://github.com/njbrake) ([`4b9d646`](https://github.com/agent-of-empires/agent-of-empires/commit/4b9d646625dec4571de5499755beab143fe323ca))
- Bump version from 0.4.1 to 0.4.2 by [@njbrake](https://github.com/njbrake) ([`58feffd`](https://github.com/agent-of-empires/agent-of-empires/commit/58feffd0c60deb38ae18fc12b0a6b6459e2b4fb1))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.4.1...v0.4.2
## [0.4.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.4.1) - 2026-01-14



### Bug Fixes

- Longer help messages were cut off in [#57](https://github.com/agent-of-empires/agent-of-empires/pull/57) by [@njbrake](https://github.com/njbrake) ([`2247245`](https://github.com/agent-of-empires/agent-of-empires/commit/2247245fa44e19f4e45ffd9e25000b8e61993ab5))


### Features

- TUI sandbox has YOLO mode toggle in [#58](https://github.com/agent-of-empires/agent-of-empires/pull/58) by [@njbrake](https://github.com/njbrake) ([`ca0092f`](https://github.com/agent-of-empires/agent-of-empires/commit/ca0092f6ae361ce07af5825ba0460ee5bd5b8b53))
- Update demo script for Docker compatibility and improve demo tape timing in [#60](https://github.com/agent-of-empires/agent-of-empires/pull/60) by [@njbrake](https://github.com/njbrake) ([`217f267`](https://github.com/agent-of-empires/agent-of-empires/commit/217f26795f6f59e0599c95c1b1310b60fdf63ec0))


### Other

- Bump version to 0.4.1 by [@njbrake](https://github.com/njbrake) ([`a6f15f5`](https://github.com/agent-of-empires/agent-of-empires/commit/a6f15f5de82c7ac4b137999b0d001f57a4813d07))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.4.0...v0.4.1
## [0.4.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.4.0) - 2026-01-14



### Bug Fixes

- Tui slowness in [#52](https://github.com/agent-of-empires/agent-of-empires/pull/52) by [@njbrake](https://github.com/njbrake) ([`0d86120`](https://github.com/agent-of-empires/agent-of-empires/commit/0d861208baa0b0c888437673f60903d0dadbbed2))


### Features

- Include relative dir name when launching sandbox in [#47](https://github.com/agent-of-empires/agent-of-empires/pull/47) by [@njbrake](https://github.com/njbrake) ([`85b088d`](https://github.com/agent-of-empires/agent-of-empires/commit/85b088df3e312869f6b0b4baa0d9e952d2158f21))
- When you detach, cursor is set to that session in [#54](https://github.com/agent-of-empires/agent-of-empires/pull/54) by [@njbrake](https://github.com/njbrake) ([`99de9ce`](https://github.com/agent-of-empires/agent-of-empires/commit/99de9ceb935dae2314f42ecd4e7adefdf1f44bb9))
- Option to attach to existing worktree/branch in [#56](https://github.com/agent-of-empires/agent-of-empires/pull/56) by [@njbrake](https://github.com/njbrake) ([`a34ba02`](https://github.com/agent-of-empires/agent-of-empires/commit/a34ba02d49a9944ced94721b4104cb356f79188e))


### Other

- Badges in [#44](https://github.com/agent-of-empires/agent-of-empires/pull/44) by [@njbrake](https://github.com/njbrake) ([`002327e`](https://github.com/agent-of-empires/agent-of-empires/commit/002327e56b7d1e445fed8e98f7c21a5997706109))
- Sandbox_options in [#53](https://github.com/agent-of-empires/agent-of-empires/pull/53) by [@njbrake](https://github.com/njbrake) ([`2f8c1cc`](https://github.com/agent-of-empires/agent-of-empires/commit/2f8c1ccc97b6320227430e9012eb3b3498c3c1ad))
- Bump version to 0.4.0 by [@njbrake](https://github.com/njbrake) ([`9eb4457`](https://github.com/agent-of-empires/agent-of-empires/commit/9eb4457702203b0d466aa60e14ae530b0a7e8217))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.3.4...v0.4.0
## [0.3.4](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.3.4) - 2026-01-13



### Bug Fixes

- Doc deployment hang in [#42](https://github.com/agent-of-empires/agent-of-empires/pull/42) by [@njbrake](https://github.com/njbrake) ([`b3fcf1a`](https://github.com/agent-of-empires/agent-of-empires/commit/b3fcf1af9b600dea5803cc11688b3a11fab9d24a))


### Other

- Docs for launching parallel agents to plan out fixes for all issues in a repo in [#43](https://github.com/agent-of-empires/agent-of-empires/pull/43) by [@njbrake](https://github.com/njbrake) ([`2e65197`](https://github.com/agent-of-empires/agent-of-empires/commit/2e651975117aa0f10ec1baa9c05adbae7d46f5e2))
- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`772ad78`](https://github.com/agent-of-empires/agent-of-empires/commit/772ad78d85a0c4ce7f794ee67608c86b7e6ff025))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.3.3...v0.3.4
## [0.3.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.3.3) - 2026-01-13



### Bug Fixes

- Docker image don't be root in [#40](https://github.com/agent-of-empires/agent-of-empires/pull/40) by [@njbrake](https://github.com/njbrake) ([`76d8dec`](https://github.com/agent-of-empires/agent-of-empires/commit/76d8dece3efdb737e9d041c54d75652cf49910cc))


### Other

- Bump version from 0.3.2 to 0.3.3 by [@njbrake](https://github.com/njbrake) ([`cb4d67b`](https://github.com/agent-of-empires/agent-of-empires/commit/cb4d67ba804f2ae7be55175c83484c04a1341f93))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.3.2...v0.3.3
## [0.3.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.3.2) - 2026-01-13



### Other

- Github by [@njbrake](https://github.com/njbrake) ([`16a1735`](https://github.com/agent-of-empires/agent-of-empires/commit/16a1735d1940ad29b500ca36f9f3c6fc329d7caf))
- Github by [@njbrake](https://github.com/njbrake) ([`5dc83bb`](https://github.com/agent-of-empires/agent-of-empires/commit/5dc83bbc0479eb8b74c4064d173ef5032ab8c686))
- Delete assets/tui.png by [@njbrake](https://github.com/njbrake) ([`ba938b4`](https://github.com/agent-of-empires/agent-of-empires/commit/ba938b45eefc25e18c0f1c64956ec6d96388d73c))
- Enhance GIF generation script and demo assets in [#38](https://github.com/agent-of-empires/agent-of-empires/pull/38) by [@njbrake](https://github.com/njbrake) ([`c70012d`](https://github.com/agent-of-empires/agent-of-empires/commit/c70012d2e35f781645dcd585d74f10bff27d56d4))
- Update README with new features and installation info by [@njbrake](https://github.com/njbrake) ([`b5e9950`](https://github.com/agent-of-empires/agent-of-empires/commit/b5e995057d248b1972b174a2efcfbca54d323395))
- Bump version from 0.3.1 to 0.3.2 by [@njbrake](https://github.com/njbrake) ([`89d4b4c`](https://github.com/agent-of-empires/agent-of-empires/commit/89d4b4c9fc79322565d0ff5a456ec622529c766d))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.3.1...v0.3.2
## [0.3.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.3.1) - 2026-01-13



### Other

- Bump version to 0.3.1 by [@njbrake](https://github.com/njbrake) ([`f5891df`](https://github.com/agent-of-empires/agent-of-empires/commit/f5891df924f0fce68eb37b359f5c5a22afb5061e))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.3.0...v0.3.1
## [0.3.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.3.0) - 2026-01-13



### Features

- Dev release for faster compile in [#30](https://github.com/agent-of-empires/agent-of-empires/pull/30) by [@njbrake](https://github.com/njbrake) ([`044750e`](https://github.com/agent-of-empires/agent-of-empires/commit/044750e2bd5f63ecced4a77a0ca782bbe3687085))
- Docker sandboxing in [#32](https://github.com/agent-of-empires/agent-of-empires/pull/32) by [@njbrake](https://github.com/njbrake) ([`77e32fc`](https://github.com/agent-of-empires/agent-of-empires/commit/77e32fc560195960df42efe7b74da6e9e657197a))


### Other

- Update README with demo GIF and add script for GIF generation; introduce development documentation in [#28](https://github.com/agent-of-empires/agent-of-empires/pull/28) by [@njbrake](https://github.com/njbrake) ([`6c6ddeb`](https://github.com/agent-of-empires/agent-of-empires/commit/6c6ddeb20489d1694b433ac1b816dcb32bbe9475))
- Usage in [#29](https://github.com/agent-of-empires/agent-of-empires/pull/29) by [@njbrake](https://github.com/njbrake) ([`ab71474`](https://github.com/agent-of-empires/agent-of-empires/commit/ab714746305c09601619ddd9a9b59551c566320b))
- Help in [#34](https://github.com/agent-of-empires/agent-of-empires/pull/34) by [@njbrake](https://github.com/njbrake) ([`ccafa7e`](https://github.com/agent-of-empires/agent-of-empires/commit/ccafa7ec1676f93393361ef37840375cdc87a2e8))
- Extra details by [@njbrake](https://github.com/njbrake) ([`30b6b36`](https://github.com/agent-of-empires/agent-of-empires/commit/30b6b361210684363bcc0af0d347923ed59c2301))
- Agents update in [#36](https://github.com/agent-of-empires/agent-of-empires/pull/36) by [@njbrake](https://github.com/njbrake) ([`f54f088`](https://github.com/agent-of-empires/agent-of-empires/commit/f54f0886172b2d44596efcf7546b4fbee013e5d4))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.2.2...v0.3.0
## [0.2.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.2.2) - 2026-01-13



### Other

- Clean canonical path in [#24](https://github.com/agent-of-empires/agent-of-empires/pull/24) by [@njbrake](https://github.com/njbrake) ([`006c09f`](https://github.com/agent-of-empires/agent-of-empires/commit/006c09f544c2e36cbcc948c3ee909f7ad4c44ef0))
- Livecheck in [#25](https://github.com/agent-of-empires/agent-of-empires/pull/25) by [@njbrake](https://github.com/njbrake) ([`681b2c6`](https://github.com/agent-of-empires/agent-of-empires/commit/681b2c6ad184c327f670f801859e837ed8992e9b))
- File descriptor desync fix(#27) in [#27](https://github.com/agent-of-empires/agent-of-empires/pull/27) by [@njbrake](https://github.com/njbrake) ([`7970b01`](https://github.com/agent-of-empires/agent-of-empires/commit/7970b016c847836fb5ff0e118d8226b61add651b))
- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`b65b7cb`](https://github.com/agent-of-empires/agent-of-empires/commit/b65b7cb6a2356f3c9be2482490052a3f3814f810))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.2.1...v0.2.2
## [0.2.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.2.1) - 2026-01-13



### Other

- Bump version to 0.2.1 by [@njbrake](https://github.com/njbrake) ([`5d06ac8`](https://github.com/agent-of-empires/agent-of-empires/commit/5d06ac85ae80a40c6c2b8bd6930ce3d58cc5269c))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.2.0...v0.2.1
## [0.2.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.2.0) - 2026-01-13



### Features

- Evaluate worktree path when displaying in [#20](https://github.com/agent-of-empires/agent-of-empires/pull/20) by [@njbrake](https://github.com/njbrake) ([`0f4b157`](https://github.com/agent-of-empires/agent-of-empires/commit/0f4b157d869cae046308a4ce8e8c126c53e1bdad))


### Other

- Implement asynchronous update check and enhance UI for update notifications in [#21](https://github.com/agent-of-empires/agent-of-empires/pull/21) by [@njbrake](https://github.com/njbrake) ([`18e25ec`](https://github.com/agent-of-empires/agent-of-empires/commit/18e25ece9080725037ed2a87c6a25058bb489ac5))
- Badge in [#22](https://github.com/agent-of-empires/agent-of-empires/pull/22) by [@njbrake](https://github.com/njbrake) ([`16f7f1b`](https://github.com/agent-of-empires/agent-of-empires/commit/16f7f1bb97202824cfd54a41b3f845490359bc83))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.1.2...v0.2.0
## [0.1.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.1.2) - 2026-01-12



### Other

- Patch by [@njbrake](https://github.com/njbrake) ([`20a1c5d`](https://github.com/agent-of-empires/agent-of-empires/commit/20a1c5dcb586322f8def84d7821cf30ec7a92802))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.1.1...v0.1.2
## [0.1.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.1.1) - 2026-01-12



### Other

- Update dependencies and bump version to 0.1.1 by [@njbrake](https://github.com/njbrake) ([`12dc193`](https://github.com/agent-of-empires/agent-of-empires/commit/12dc193ed4b5db6f003b27366f4fe0a7e54b7fbe))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.1.0...v0.1.1
## [0.1.0](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.1.0) - 2026-01-12



### Bug Fixes

- Fix flicker by [@njbrake](https://github.com/njbrake) ([`85ddfe2`](https://github.com/agent-of-empires/agent-of-empires/commit/85ddfe218446aa681dd03f0a1bad07ec0574404c))


### Features

- Git worktrees for parallel agents in same git project in [#14](https://github.com/agent-of-empires/agent-of-empires/pull/14) by [@njbrake](https://github.com/njbrake) ([`ffd6244`](https://github.com/agent-of-empires/agent-of-empires/commit/ffd624446cabb8c1a9a6046ec7e6e505b4352789))


### Other

- Update README.md by [@njbrake](https://github.com/njbrake) ([`e0350d7`](https://github.com/agent-of-empires/agent-of-empires/commit/e0350d73b0ab2d6dd3cc41d5e6352ca33fd798e1))
- Opt to move faster in [#13](https://github.com/agent-of-empires/agent-of-empires/pull/13) by [@njbrake](https://github.com/njbrake) ([`14b112a`](https://github.com/agent-of-empires/agent-of-empires/commit/14b112ab34770ca88116ca4aacce1352083c19c0))
- Bump version to 0.1.0 by [@njbrake](https://github.com/njbrake) ([`9547961`](https://github.com/agent-of-empires/agent-of-empires/commit/9547961a1ab087f35f059ead8cfb06b0478e3a0d))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.12...v0.1.0
## [0.0.12](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.12) - 2026-01-12



### Features

- Status checker only looks at last 30 lines, not entire window in [#6](https://github.com/agent-of-empires/agent-of-empires/pull/6) by [@njbrake](https://github.com/njbrake) ([`085a9e3`](https://github.com/agent-of-empires/agent-of-empires/commit/085a9e355b58579942f5fbf9fcaae6f0afdf81c8))
- Implement session renaming functionality with a dedicated dialog in [#9](https://github.com/agent-of-empires/agent-of-empires/pull/9) by [@njbrake](https://github.com/njbrake) ([`3bea21f`](https://github.com/agent-of-empires/agent-of-empires/commit/3bea21f10461b6557def0a7d37051de66078b60e))


### Other

- Update tui.png asset with new design by [@njbrake](https://github.com/njbrake) ([`b6549e5`](https://github.com/agent-of-empires/agent-of-empires/commit/b6549e5f84e0fe4d441cd75cae8e8c244a2ca692))
- Bugs in [#11](https://github.com/agent-of-empires/agent-of-empires/pull/11) by [@njbrake](https://github.com/njbrake) ([`acd68ab`](https://github.com/agent-of-empires/agent-of-empires/commit/acd68ab0ddcad5d6cf0d3d99d7c0a2a76c333a11))
- Update Cargo.toml by [@njbrake](https://github.com/njbrake) ([`6d7d4ad`](https://github.com/agent-of-empires/agent-of-empires/commit/6d7d4ad26dc739f21c7e48f9cac291538d3f6d49))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.11...v0.0.12
## [0.0.11](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.11) - 2026-01-11



### Other

- Setup to be able to add more captures if issues with state are found by [@njbrake](https://github.com/njbrake) ([`d1d5eec`](https://github.com/agent-of-empires/agent-of-empires/commit/d1d5eec85ad4cb0c281980c701868514693e09d2))
- Symlink a CLAUDE.md by [@njbrake](https://github.com/njbrake) ([`847a03e`](https://github.com/agent-of-empires/agent-of-empires/commit/847a03eda946d5aa943c45c769a011a00e4c0786))
- Better styling for session selector by [@njbrake](https://github.com/njbrake) ([`9f2b8c7`](https://github.com/agent-of-empires/agent-of-empires/commit/9f2b8c7367ff3e4f7c4ab79e0bd137e52891dc34))
- Enhance styling in NewSessionDialog for improved focus indication and tool selection display by [@njbrake](https://github.com/njbrake) ([`2c3f157`](https://github.com/agent-of-empires/agent-of-empires/commit/2c3f15775f8635b35238b723ba551ec888bc2987))
- Bump version from 0.0.10 to 0.0.11 in Cargo.toml by [@njbrake](https://github.com/njbrake) ([`beb0c8a`](https://github.com/agent-of-empires/agent-of-empires/commit/beb0c8a4fc3c9cd43abf7028d4a6c3112c8c649b))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.10...v0.0.11
## [0.0.10](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.10) - 2026-01-10



### Features

- Add debug logging to preview rendering and remove unused window resizing functionality by [@njbrake](https://github.com/njbrake) ([`5f80f62`](https://github.com/agent-of-empires/agent-of-empires/commit/5f80f62fe70d7bc06eb5b276bf90dbf48ce9b7ae))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.9...v0.0.10
## [0.0.9](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.9) - 2026-01-10



### Features

- Add terminal fixture capture script and implement status detection tests for Claude Code and OpenCode by [@njbrake](https://github.com/njbrake) ([`09a1471`](https://github.com/agent-of-empires/agent-of-empires/commit/09a147188b104992395addeef52d16aaf59d6f0b))
- Update README for clarity and installation instructions, and add install script for easier setup by [@njbrake](https://github.com/njbrake) ([`1ba7bf0`](https://github.com/agent-of-empires/agent-of-empires/commit/1ba7bf0d2b33455ab69e12851afa3efcd59bf24b))


### Other

- Bump version from 0.0.8 to 0.0.9 by [@njbrake](https://github.com/njbrake) ([`d995d42`](https://github.com/agent-of-empires/agent-of-empires/commit/d995d42368b98fe7c90f945251292777d5cfee5c))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.8...v0.0.9
## [0.0.8](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.8) - 2026-01-10



### Features

- Update TUI image asset to improve visual representation by [@njbrake](https://github.com/njbrake) ([`f516e46`](https://github.com/agent-of-empires/agent-of-empires/commit/f516e460bbbb224303a392661e43cc7d27b74602))
- Bump version to 0.0.8, add random title generation using Age of Empires civilizations, and enhance session management with logging improvements by [@njbrake](https://github.com/njbrake) ([`b3850c3`](https://github.com/agent-of-empires/agent-of-empires/commit/b3850c394c3a6b68f232c1b0d7a9db7a6f6bfb0b))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.7...v0.0.8
## [0.0.7](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.7) - 2026-01-10



### Features

- Add support for tool availability detection, enhance session management with error handling, and improve README with mobile SSH client instructions by [@njbrake](https://github.com/njbrake) ([`7ae3e7e`](https://github.com/agent-of-empires/agent-of-empires/commit/7ae3e7ec36909b1ea6e148d27dd5e1620e012e28))
- Add cargo-husky for pre-commit hooks and improve ConfirmDialog with comprehensive unit tests by [@njbrake](https://github.com/njbrake) ([`b74dc08`](https://github.com/agent-of-empires/agent-of-empires/commit/b74dc088968a321ec91404d6000d03c32528e3c6))
- Implement comprehensive unit tests for session management, group handling, and UI interactions in TUI components by [@njbrake](https://github.com/njbrake) ([`f240bd0`](https://github.com/agent-of-empires/agent-of-empires/commit/f240bd0d653fecb4ab2517aefa9201541217e292))


### Other

- Image by [@njbrake](https://github.com/njbrake) ([`f16ba2b`](https://github.com/agent-of-empires/agent-of-empires/commit/f16ba2b801dc1ccc810ead68c363d3dd49ba9b66))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.6...v0.0.7
## [0.0.6](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.6) - 2026-01-09



### Other

- Version 0.0.5 by [@njbrake](https://github.com/njbrake) ([`b5caa6a`](https://github.com/agent-of-empires/agent-of-empires/commit/b5caa6a59335ad1349dd0c93b93020838760a895))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.5...v0.0.6
## [0.0.5](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.5) - 2026-01-09



### Other

- Fmt by [@njbrake](https://github.com/njbrake) ([`ad49990`](https://github.com/agent-of-empires/agent-of-empires/commit/ad499909de84947e9be603e32a0ad2a0076684de))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.4...v0.0.5
## [0.0.4](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.4) - 2026-01-09



### Bug Fixes

- Correct GitHub username to njbrake in all URLs by [@njbrake](https://github.com/njbrake) ([`87c95b6`](https://github.com/agent-of-empires/agent-of-empires/commit/87c95b6d078d10227727066c3db931569e7d5f9a))


### Features

- Enhance README with tmux usage instructions, update default tool to 'claude', and improve command detection logic for empty commands. Add new content detection for 'claude' in session management. by [@njbrake](https://github.com/njbrake) ([`99f2bfc`](https://github.com/agent-of-empires/agent-of-empires/commit/99f2bfc76cee5a00c85038c20f6485f8e39fdf49))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.3...v0.0.4
## [0.0.3](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.3) - 2026-01-09



### Bug Fixes

- Release workflow artifact handling, bump to 0.0.3 by [@njbrake](https://github.com/njbrake) ([`dd2cb86`](https://github.com/agent-of-empires/agent-of-empires/commit/dd2cb8638668bb422dd721421e87789278ae72ff))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.2...v0.0.3
## [0.0.2](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.2) - 2026-01-09



### Other

- Format by [@njbrake](https://github.com/njbrake) ([`17e2fa1`](https://github.com/agent-of-empires/agent-of-empires/commit/17e2fa11742dbc1e4f26d607af0993deae6e3645))
- Refactor update-formula.sh to simplify SHA256 hash retrieval and update output format for Homebrew formula. Improve CLI output formatting in list.rs and mcp.rs, and enhance path shortening logic in multiple files. Remove unnecessary logging in tmux/session.rs and streamline session management in tui components. by [@njbrake](https://github.com/njbrake) ([`2afbb5e`](https://github.com/agent-of-empires/agent-of-empires/commit/2afbb5e9275c4ca1307452a1a31a989e98a7562c))


**Full Changelog**: https://github.com/agent-of-empires/agent-of-empires/compare/v0.0.1...v0.0.2
## [0.0.1](https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.0.1) - 2026-01-09



### Features

- Add CI/CD and release workflows by [@njbrake](https://github.com/njbrake) ([`e55b82e`](https://github.com/agent-of-empires/agent-of-empires/commit/e55b82e193702905e7f88e39dd1e537feecee744))


### Other

- Initial commit by [@njbrake](https://github.com/njbrake) ([`dfdb22d`](https://github.com/agent-of-empires/agent-of-empires/commit/dfdb22d60a4e0339a81d9bab838e5cec97421d0f))
- Claude status by [@njbrake](https://github.com/njbrake) ([`c1e2293`](https://github.com/agent-of-empires/agent-of-empires/commit/c1e229367432d11211cd18f7bad45af27b1eab6e))
- Add repository guidelines, update binary name to 'aoe', and remove deprecated files. Implement process state detection for Linux and macOS, and enhance session management features. Update documentation and tests accordingly. by [@njbrake](https://github.com/njbrake) ([`48ccc07`](https://github.com/agent-of-empires/agent-of-empires/commit/48ccc079a60d3617ef3016a98865238d509a0799))



### New Contributors

- [@njbrake](https://github.com/njbrake) made their first contribution in [#](https://github.com/agent-of-empires/agent-of-empires/pull/)



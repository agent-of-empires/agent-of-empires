# Agent of Empires OMP integration

This directory contains two separately installed components: the AoE plugin declared by `aoe-plugin.toml` and the OMP runtime extension under `omp-extension/`.

The AoE host API and runtime support for provider-native OMP status and explicit prompt replies are introduced in [upstream PR #4666](https://github.com/agent-of-empires/agent-of-empires/pull/4666). The host requires AoE API 18; the extension requires OMP 18.11.1 or newer. Installing the integration does not approve prompts automatically.
{
  description = "Terminal session manager for AI coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
  };

  outputs = inputs @ { flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];

      perSystem = { config, self', inputs', pkgs, system, ... }:
        let
          craneLib = inputs.crane.mkLib pkgs;

          # git2 uses vendored-openssl (needs perl to build OpenSSL)
          # and libgit2-sys vendors libgit2 (needs cmake to build it)
          nativeBuildInputs = with pkgs; [
            pkg-config
            perl
            cmake
            installShellFiles
          ];

          buildInputs = with pkgs; [
            zlib # required by vendored libgit2
          ];

          commonArgs = {
            src = craneLib.cleanCargoSource ./.;
            strictDeps = true;
            inherit nativeBuildInputs buildInputs;
            # Default cargo features now include `serve` (web dashboard),
            # whose build.rs step wants npm or AOE_WEB_DIST. The base
            # derivation and checks build TUI-only so they stay hermetic
            # without the frontend; the web-enabled build (the flake's
            # default package) opts back in below via AOE_WEB_DIST.
            cargoExtraArgs = "--no-default-features";
          };

          # Build only workspace dependencies first (for caching)
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          aoe = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            cargoExtraArgs = "--package agent-of-empires --no-default-features";
            doCheck = false;
            postInstall = ''
              installShellCompletion --cmd aoe \
                --bash <($out/bin/aoe completion bash) \
                --fish <($out/bin/aoe completion fish) \
                --zsh <($out/bin/aoe completion zsh)
            '';

            meta = with pkgs.lib; {
              description = "Terminal session manager for AI coding agents";
              longDescription = ''
                Agent of Empires (AoE) is a terminal session manager for AI coding
                agents on Linux and macOS. Built on tmux, it allows running multiple
                AI agents in parallel across different branches of your codebase,
                each in its own isolated session with optional Docker sandboxing.

                Supports Claude Code, OpenCode, Mistral Vibe, Codex CLI, and Gemini CLI.
              '';
              homepage = "https://github.com/agent-of-empires/agent-of-empires";
              license = licenses.mit;
              platforms = platforms.unix;
              mainProgram = "aoe";
            };
          });

          # Build the React frontend as a standalone derivation (Forgejo/ntfy-sh pattern).
          # This separates the npm build from the Rust build cleanly.
          #
          # Update npmDepsHash whenever web/package-lock.json changes:
          #   nix-update aoe-with-web
          # or manually: set npmDepsHash to lib.fakeHash, build, copy the got: hash.
          webFrontend = pkgs.buildNpmPackage {
            pname = "agent-of-empires-web";
            version = "0";
            src = ./web;
            npmDepsHash = "sha256-/K8QQ9Gj/mCttVMgi5B2JxdQ9igBVK9BgjxzFy3QgpY=";
            # tsc -b && vite build; output goes to web/dist
            installPhase = ''
              mkdir $out
              cp -r dist $out/
            '';
          };

          # Base args for the web-enabled build. No npm tooling needed here since
          # build.rs respects AOE_WEB_DIST to use the pre-built frontend.
          # buildDepsOnly uses a dummy crate source so AOE_WEB_DIST is irrelevant there.
          commonArgsWithWeb = commonArgs // {
            # Default features include `serve`, so no explicit flag needed.
            cargoExtraArgs = "--package agent-of-empires";
          };

          # Rust dep cache compiled with the serve deps (no npm involved).
          cargoArtifactsWithWeb = craneLib.buildDepsOnly commonArgsWithWeb;

          aoeWithWeb = craneLib.buildPackage (commonArgsWithWeb // {
            cargoArtifacts = cargoArtifactsWithWeb;
            doCheck = false;
            # Point build.rs at the pre-built frontend; it will copy dist/ into
            # place and skip running npm entirely (see build.rs AOE_WEB_DIST handling).
            AOE_WEB_DIST = "${webFrontend}/dist";
            postInstall = ''
              installShellCompletion --cmd aoe \
                --bash <($out/bin/aoe completion bash) \
                --fish <($out/bin/aoe completion fish) \
                --zsh <($out/bin/aoe completion zsh)
            '';
            meta = aoe.meta;
            # Expose npmDeps so `nix-update` can automatically recompute the
            # npmDepsHash in webFrontend when web/package-lock.json changes.
            passthru.npmDeps = webFrontend.npmDeps;
          });
        in
        {
          # The default package matches the cargo default: web dashboard
          # included (frontend injected hermetically via AOE_WEB_DIST).
          packages.default = aoeWithWeb;
          # TUI-only binary, no web assets, smaller closure.
          packages.aoe-tui = aoe;
          # Back-compat alias; same derivation as the default package.
          packages.aoe-with-web = aoeWithWeb;
          # Just the npm + vite build. Exposed so the PR-CI Nix Build
          # Web job can validate npmDepsHash + frontend build in ~1-2
          # min instead of rebuilding the full Rust workspace.
          packages.aoe-web-frontend = webFrontend;
          # Exposed so the nix-npm-hash bots and the local manual
          # update procedure use the same nixpkgs revision as
          # `buildNpmPackage` above.
          packages.prefetch-npm-deps = pkgs.prefetch-npm-deps;

          checks = {
            # Build the packages as checks too
            inherit aoe;
            inherit aoeWithWeb;

            aoe-clippy = craneLib.cargoClippy (commonArgs // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--package agent-of-empires --all-targets -- --deny warnings";
            });

            aoe-fmt = craneLib.cargoFmt {
              inherit (commonArgs) src;
            };

            aoe-test = craneLib.cargoTest (commonArgs // {
              inherit cargoArtifacts;
              cargoTestExtraArgs = "--package agent-of-empires";
              # Some git:: unit tests invoke the git binary directly
              nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.git ];
            });
          };

          devShells.default = craneLib.devShell {
            checks = self'.checks;
            packages = with pkgs; [
              rust-analyzer
              tmux
              nodejs # for web frontend development (default build includes the dashboard)
            ];
          };
        };
    };
}

{
  description = "Terminal session manager for AI coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ { flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];

      perSystem = { config, self', inputs', system, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ (import inputs.rust-overlay) ];
          };

          # Pin an explicit stable rustc instead of inheriting whatever rustc
          # the pinned nixpkgs happens to ship. nixpkgs lags stable, which
          # broke the nix build when libsqlite3-sys 0.38.1 started using the
          # cfg_select! macro (stable in rustc >= 1.93) while nixpkgs still
          # shipped 1.92.0. Tracking latest stable here keeps the nix
          # toolchain in step with the rustup-stable used by the regular CI.
          rustToolchain = pkgs.rust-bin.stable.latest.default;
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

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
          };

          # Build only workspace dependencies first (for caching)
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          aoe = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            cargoExtraArgs = "--package agent-of-empires";
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
            npmDepsHash = "sha256-5xBa6GocCfQCucIFA78t+LAV9gZXdn+uEzKUSqRlvI4=";
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
            cargoExtraArgs = "--package agent-of-empires --features serve";
          };

          # Rust dep cache compiled with --features serve (no npm involved).
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
          packages.default = aoe;
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
              nodejs # for web frontend development (--features serve)
            ];
          };
        };
    };
}

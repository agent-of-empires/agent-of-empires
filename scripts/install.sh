#!/bin/bash
set -e

REPO="agent-of-empires/agent-of-empires"
DEFAULT_INSTALL_DIR="$HOME/.local/bin"
INSTALL_DIR="${INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"
BINARY_NAME="aoe"

# Matches the manylinux_2_28 container the release workflow builds in
# (.github/workflows/release.yml, env.LINUX_GLIBC_FLOOR). Bump together.
MIN_GLIBC="2.28"

info() { printf "\033[34m[info]\033[0m %s\n" "$1"; }
success() { printf "\033[32m[ok]\033[0m %s\n" "$1"; }
warn() { printf "\033[33m[warn]\033[0m %s\n" "$1"; }
error() { printf "\033[31m[error]\033[0m %s\n" "$1" >&2; exit 1; }

# Fail fast if the system's glibc is older than the released binary needs.
# Without this, the download "succeeds" and the user only finds out at the
# first `aoe` invocation that libc.so.6 doesn't have GLIBC_2.xx. Skips
# silently when we can't tell (non-Linux, no ldd, unparseable version).
check_glibc() {
    [ "$(uname -s)" = "Linux" ] || return 0
    command -v ldd >/dev/null 2>&1 || return 0
    local sys_glibc
    sys_glibc=$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' | head -1)
    [ -n "$sys_glibc" ] || return 0
    if [ "$(printf '%s\n%s\n' "$MIN_GLIBC" "$sys_glibc" | sort -V | head -1)" != "$MIN_GLIBC" ]; then
        error "Your system's glibc ($sys_glibc) is older than this binary requires ($MIN_GLIBC).
Options:
  - Use a newer distro (Ubuntu 20.04+, Debian 11+, RHEL 8+, Amazon Linux 2/2023)
  - Install via Homebrew: brew install aoe
  - Build from source: https://www.agent-of-empires.com/docs/installation/#build-from-source"
    fi
}

detect_platform() {
    local os arch
    os=$(uname -s | tr '[:upper:]' '[:lower:]')
    arch=$(uname -m)

    case "$os" in
        linux) os="linux" ;;
        darwin) os="darwin" ;;
        *) error "Unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64) arch="amd64" ;;
        aarch64|arm64) arch="arm64" ;;
        *) error "Unsupported architecture: $arch" ;;
    esac

    echo "${os}-${arch}"
}

get_latest_version() {
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | sed -E 's/.*"([^"]+)".*/\1/'
}

# Pick the right shell rc file for PATH guidance. Mirrors what most
# install scripts (rustup, bun, deno) do.
detect_shell_rc() {
    local shell_name
    shell_name=$(basename "${SHELL:-}")
    case "$shell_name" in
        zsh)  echo "$HOME/.zshrc" ;;
        bash)
            if [ -f "$HOME/.bashrc" ]; then echo "$HOME/.bashrc"
            else echo "$HOME/.bash_profile"
            fi
            ;;
        fish) echo "$HOME/.config/fish/config.fish" ;;
        *)    echo "" ;;
    esac
}

main() {
    info "Detecting platform..."
    platform=$(detect_platform)
    success "Platform: $platform"

    check_glibc

    info "Fetching latest version..."
    version=$(get_latest_version)
    if [ -z "$version" ]; then
        error "Failed to fetch latest version"
    fi
    success "Latest version: $version"

    download_url="https://github.com/${REPO}/releases/download/${version}/aoe-${platform}.tar.gz"
    info "Downloading from: $download_url"

    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT

    curl -fsSL "$download_url" -o "$tmp_dir/aoe.tar.gz" || error "Download failed"
    success "Downloaded successfully"

    info "Extracting..."
    tar xzf "$tmp_dir/aoe.tar.gz" -C "$tmp_dir"

    mkdir -p "$INSTALL_DIR" 2>/dev/null || true

    info "Installing to $INSTALL_DIR..."
    if [ -w "$INSTALL_DIR" ] || [ ! -e "$INSTALL_DIR" ]; then
        mv "$tmp_dir/aoe-${platform}" "$INSTALL_DIR/$BINARY_NAME"
    else
        warn "$INSTALL_DIR is not user-writable; falling back to sudo."
        warn "Consider re-running with INSTALL_DIR=\$HOME/.local/bin to avoid this."
        sudo mv "$tmp_dir/aoe-${platform}" "$INSTALL_DIR/$BINARY_NAME"
    fi
    chmod +x "$INSTALL_DIR/$BINARY_NAME"

    success "Installed $BINARY_NAME $version to $INSTALL_DIR/$BINARY_NAME"

    # Detect a leftover binary at the legacy /usr/local/bin location. Older
    # versions of this script defaulted there; if a user re-runs install.sh
    # with the new ~/.local/bin default they end up with two binaries and
    # whichever directory comes first on PATH wins, which is confusing.
    LEGACY_PATH=/usr/local/bin/aoe
    if [ -e "$LEGACY_PATH" ] && [ "$INSTALL_DIR/$BINARY_NAME" != "$LEGACY_PATH" ]; then
        echo ""
        warn "Found a previous install at $LEGACY_PATH."
        warn "PATH order will decide which one runs. Remove the old copy with:"
        warn "  sudo rm $LEGACY_PATH"
    fi

    if ! command -v "$BINARY_NAME" >/dev/null 2>&1; then
        echo ""
        warn "$INSTALL_DIR is not on your PATH."
        rc_file=$(detect_shell_rc)
        if [ -n "$rc_file" ]; then
            info "Add it by running:"
            info "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> $rc_file"
            info "Then restart your shell, or run: source $rc_file"
        else
            info "Add this line to your shell rc file:"
            info "  export PATH=\"$INSTALL_DIR:\$PATH\""
        fi
    fi

    if ! command -v tmux &> /dev/null; then
        info ""
        info "Note: tmux is required but not installed."
        info "Install it with:"
        info "  Debian/Ubuntu: sudo apt install tmux"
        info "  Fedora/RHEL:   sudo dnf install tmux"
        info "  Arch:          sudo pacman -S tmux"
        info "  macOS:         brew install tmux"
    fi

    echo ""
    success "Run 'aoe' to get started!"
    echo ""
    info "For shell completions, see: https://www.agent-of-empires.com/guides/shell-completions/"
}

main "$@"

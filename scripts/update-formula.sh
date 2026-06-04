#!/bin/bash
set -e

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version>"
    echo "Example: $0 0.1.0"
    exit 1
fi

REPO="agent-of-empires/agent-of-empires"
BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"

echo "Fetching sha256 hashes for v${VERSION}..."
echo ""

for ARTIFACT in aoe-darwin-arm64 aoe-darwin-amd64 aoe-linux-arm64 aoe-linux-amd64; do
    URL="${BASE_URL}/${ARTIFACT}.tar.gz"
    echo "Downloading ${ARTIFACT}..."
    SHA=$(curl -sL "${URL}" | shasum -a 256 | cut -d' ' -f1)
    echo "  ${ARTIFACT}: ${SHA}"

    eval "SHA_${ARTIFACT//-/_}=${SHA}"
done

echo ""
echo "=== Update Formula/aoe.rb with these values ==="
echo ""
cat << EOF
  on_macos do
    on_arm do
      url "https://github.com/${REPO}/releases/download/v${VERSION}/aoe-darwin-arm64.tar.gz"
      sha256 "${SHA_aoe_darwin_arm64}"
    end
    on_intel do
      url "https://github.com/${REPO}/releases/download/v${VERSION}/aoe-darwin-amd64.tar.gz"
      sha256 "${SHA_aoe_darwin_amd64}"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/${REPO}/releases/download/v${VERSION}/aoe-linux-arm64.tar.gz"
      sha256 "${SHA_aoe_linux_arm64}"
    end
    on_intel do
      url "https://github.com/${REPO}/releases/download/v${VERSION}/aoe-linux-amd64.tar.gz"
      sha256 "${SHA_aoe_linux_amd64}"
    end
  end
EOF

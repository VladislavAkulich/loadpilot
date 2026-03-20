#!/bin/sh
# LoadPilot agent install script
# Usage: curl -fsSL https://raw.githubusercontent.com/VladislavAkulich/loadpilot/main/install.sh | sh
#
# Options (env vars):
#   INSTALL_DIR   — where to install the binary (default: /usr/local/bin)
#   LOADPILOT_VERSION — specific version tag to install (default: latest)

set -e

REPO="VladislavAkulich/loadpilot"
BIN_NAME="loadpilot-agent"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
VERSION="${LOADPILOT_VERSION:-latest}"

# ── Detect OS ────────────────────────────────────────────────────────────────
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$OS" in
  linux)  OS="linux"  ;;
  darwin) OS="darwin" ;;
  *)
    echo "Unsupported OS: $OS"
    exit 1
    ;;
esac

# ── Detect architecture ───────────────────────────────────────────────────────
ARCH=$(uname -m)
case "$ARCH" in
  x86_64|amd64)   ARCH="x86_64"  ;;
  aarch64|arm64)  ARCH="arm64"   ;;
  *)
    echo "Unsupported architecture: $ARCH"
    exit 1
    ;;
esac

# macOS arm64 uses "arm64" but our asset name uses "arm64" too — consistent.
ASSET="agent-${OS}-${ARCH}"

# ── Resolve download URL ──────────────────────────────────────────────────────
if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
else
  URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
fi

# ── Download ──────────────────────────────────────────────────────────────────
DEST="${INSTALL_DIR}/${BIN_NAME}"

echo "Installing loadpilot-agent (${OS}/${ARCH})..."
echo "  from: ${URL}"
echo "  to:   ${DEST}"
echo ""

if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$URL" -o "$DEST"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$DEST" "$URL"
else
  echo "Error: curl or wget is required"
  exit 1
fi

chmod +x "$DEST"

echo "Done! loadpilot-agent installed to $DEST"
echo ""
echo "Usage:"
echo "  $BIN_NAME --coordinator <nats-host:port> --run-id <run-id> --agent-id <agent-id>"
echo ""
echo "Example (connect to coordinator at 10.0.0.1:4222):"
echo "  $BIN_NAME --coordinator 10.0.0.1:4222 --run-id my-run --agent-id agent-1"

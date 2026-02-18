#!/usr/bin/env bash
set -euo pipefail

VERSION="${WASI_SDK_VERSION:-27.0}"
TAG="wasi-sdk-${VERSION%%.*}"
ROOT="${WASI_SDK_ROOT:-$PWD/.wasi-sdk}"
ARCHIVE="wasi-sdk-${VERSION}-arm64-macos.tar.gz"
URL="https://github.com/WebAssembly/wasi-sdk/releases/download/${TAG}/${ARCHIVE}"

mkdir -p "$ROOT"
cd "$ROOT"

if find "$ROOT" -maxdepth 1 -type d -name "wasi-sdk-${VERSION}*" | grep -q .; then
  EXISTING=$(find "$ROOT" -maxdepth 1 -type d -name "wasi-sdk-${VERSION}*" | head -n1)
  echo "WASI SDK already installed at $EXISTING"
  exit 0
fi

if [[ ! -f "$ARCHIVE" ]]; then
  echo "Downloading $URL"
  curl -L "$URL" -o "$ARCHIVE"
fi

tar -xzf "$ARCHIVE"

INSTALLED=$(find "$ROOT" -maxdepth 1 -type d -name "wasi-sdk-${VERSION}*" | head -n1)
if [[ -z "$INSTALLED" ]]; then
  echo "Failed to locate extracted WASI SDK directory"
  exit 1
fi

echo "Installed WASI SDK under: $INSTALLED"
echo "Use: export WASI_SDK_PATH=$INSTALLED"

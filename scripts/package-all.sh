#!/bin/sh
set -eu
ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
VERSION=${VERSION:-0.2.0}
echo "Worktable — full release v$VERSION (native + web)"
echo "  native: scripts/package-macos.sh"
echo "  web   : scripts/package-web.sh"
echo ""
"$ROOT_DIR/scripts/package-macos.sh"
echo ""
"$ROOT_DIR/scripts/package-web.sh"
echo ""
echo "→ Zipping web for release…"
cd "$ROOT_DIR/dist"
if [ -d web ]; then
  zip -r -y "Worktable-web-v${VERSION}.zip" web -q
  ls -lh "Worktable-web-v${VERSION}.zip"
  shasum -a 256 "Worktable-web-v${VERSION}.zip" | head -n 1
fi
if [ -d Worktable.app ]; then
  # re-zip native with versioned name if not already
  if [ ! -f "Worktable-v${VERSION}-macos-arm64.zip" ]; then
    zip -r -y "Worktable-v${VERSION}-macos-arm64.zip" Worktable.app -q
  fi
  ls -lh Worktable*.zip Worktable*.dmg 2>&1 | head -n 20
fi
echo ""
echo "Artifacts:"
ls -lh "$ROOT_DIR/dist" | head -n 30

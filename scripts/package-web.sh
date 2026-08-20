#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
CRATE_DIR="$ROOT_DIR/crates/worktable-web"
DIST_DIR="$ROOT_DIR/dist/web"
PKG_DIR="$DIST_DIR/pkg"
VERSION=${VERSION:-0.1.0}

echo "Worktable Web — WASM/WebGPU + WebView"
echo "  crate: $CRATE_DIR"
echo "  dist : $DIST_DIR"

# Ensure wasm target
if ! rustup target list --installed 2>/dev/null | grep -q "wasm32-unknown-unknown"; then
  echo "Installing wasm32-unknown-unknown…"
  rustup target add wasm32-unknown-unknown
fi

# Ensure wasm-pack / wasm-bindgen
if ! command -v wasm-pack >/dev/null 2>&1; then
  echo "wasm-pack not found — installing via cargo install wasm-pack"
  cargo install wasm-pack
fi
if ! command -v wasm-bindgen >/dev/null 2>&1; then
  echo "Installing wasm-bindgen-cli…"
  cargo install wasm-bindgen-cli
fi

cd "$ROOT_DIR"

echo ""
echo "→ Building WASM (release, wasm32-unknown-unknown)…"
# wasm-pack handles bindgen + optimization; fallback to cargo + wasm-bindgen if it fails
if wasm-pack build crates/worktable-web --target web --out-dir "$PKG_DIR" --out-name worktable_web ${WASM_PACK_ARGS:-}; then
  echo "wasm-pack succeeded"
else
  echo "wasm-pack failed, falling back to cargo + wasm-bindgen…"
  cargo build -p worktable-web --target wasm32-unknown-unknown --release
  mkdir -p "$PKG_DIR"
  wasm-bindgen --target web --out-dir "$PKG_DIR" --out-name worktable_web target/wasm32-unknown-unknown/release/worktable_web.wasm
  # wasm-opt if available
  if command -v wasm-opt >/dev/null 2>&1; then
    wasm-opt "$PKG_DIR/worktable_web_bg.wasm" -Oz -o "$PKG_DIR/worktable_web_bg.wasm.opt" && mv "$PKG_DIR/worktable_web_bg.wasm.opt" "$PKG_DIR/worktable_web_bg.wasm" || true
  fi
fi

echo ""
echo "→ Staging web assets…"
mkdir -p "$DIST_DIR"
cp -f "$CRATE_DIR/index.html" "$DIST_DIR/index.html"
# pkg already at $PKG_DIR, verify
ls -lh "$PKG_DIR" | head -n 20
if [ ! -f "$PKG_DIR/worktable_web_bg.wasm" ]; then
  echo "ERROR: $PKG_DIR/worktable_web_bg.wasm missing" >&2
  exit 1
fi
if [ ! -f "$PKG_DIR/worktable_web.js" ]; then
  echo "ERROR: $PKG_DIR/worktable_web.js missing" >&2
  exit 1
fi

# Optional: native WebView wrapper (wry/tao) — builds a desktop binary that hosts the same web build
if [ "${BUILD_WEBVIEW:-1}" = "1" ]; then
  echo ""
  echo "→ Building native WebView wrapper (worktable-webview)…"
  if cargo build -p worktable-web --bin worktable-webview --features webview --release; then
    mkdir -p "$DIST_DIR/../Worktable-WebView.app/Contents/MacOS" 2>/dev/null || true
    if [ -f "$ROOT_DIR/target/release/worktable-webview" ]; then
      cp "$ROOT_DIR/target/release/worktable-webview" "$DIST_DIR/worktable-webview" || true
      echo "  webview binary: $DIST_DIR/worktable-webview ($(du -sh "$DIST_DIR/worktable-webview" | cut -f1))"
    fi
  else
    echo "  webview build skipped/failed (requires wry/tao deps, macOS/WebKit)"
  fi
fi

echo ""
echo "→ Web artifacts ready:"
du -sh "$DIST_DIR" "$PKG_DIR" 2>/dev/null | head -n 10
ls -lh "$DIST_DIR" | head -n 30
echo ""
echo "Run locally:"
echo "  python3 -m http.server --directory $DIST_DIR 8000"
echo "  open http://localhost:8000/"
echo ""
echo "WebView (if built):"
echo "  ./dist/web/worktable-webview  # or cargo run -p worktable-web --bin worktable-webview --features webview"
echo ""
# wasm size
if command -v wasm-bindgen >/dev/null 2>&1; then
  echo "WASM sizes:"
  ls -lh "$PKG_DIR"/*.wasm 2>/dev/null | head -n 5
  ls -lh "$PKG_DIR"/*.js 2>/dev/null | head -n 5
fi

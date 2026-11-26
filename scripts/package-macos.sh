#!/usr/bin/env bash
# Builds a self-contained dist/Accio.app and dist/Accio_<version>_aarch64.dmg:
#   - the Tauri app,
#   - accio-mcp (Contents/MacOS/accio-mcp) for coding agents,
#   - the Python helper project (Contents/Resources/ingest), which the app runs with
#     uv on the user's Mac; uv keeps its environment in Accio's data folder.
# The bundle is ad-hoc signed, not notarized.
#
#   scripts/package-macos.sh            # app and DMG
#   scripts/package-macos.sh --no-dmg   # app only (local installs)
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(python3 -c 'import json; print(json.load(open("app/src-tauri/tauri.conf.json"))["version"])')

(cd app/ui && npm ci --silent)
cargo build --release -p accio-mcp
(cd app/src-tauri && cargo tauri build --bundles app)

rm -rf dist
mkdir -p dist
app=dist/Accio.app
cp -R target/release/bundle/macos/Accio.app "$app"
cp target/release/accio-mcp "$app/Contents/MacOS/accio-mcp"
ingest="$app/Contents/Resources/ingest"
mkdir -p "$ingest"
cp ingest/pyproject.toml ingest/uv.lock "$ingest/"
rsync -a --exclude __pycache__ --exclude '*.pyc' ingest/src "$ingest/"
# Adding files invalidated the bundle's signature; Apple Silicon needs a valid one.
codesign --force --deep --sign - "$app"

if [[ "${1:-}" != "--no-dmg" ]]; then
  staging=$(mktemp -d)
  cp -R "$app" "$staging/"
  ln -s /Applications "$staging/Applications"
  hdiutil create -quiet -volname Accio -srcfolder "$staging" -ov -format UDZO \
    "dist/Accio_${version}_aarch64.dmg"
  rm -rf "$staging"
  echo "built dist/Accio_${version}_aarch64.dmg"
fi
echo "built $app"

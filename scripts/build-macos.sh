#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
smoke_config=
if [[ "${1:-}" == --smoke-test ]]; then
  shift
  # WebKit's Keychain item is scoped to the bundle ID and trusts its code signature.
  smoke_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  smoke_config="\"identifier\":\"dev.orca-tauri.smoke.$smoke_id\",\"productName\":\"Orca-Tauri Smoke\","
fi

core_dir=dist-core/macos-arm64-bun-plugins
python3 scripts/extract-macos-core.py "$@" --output "$core_dir"

config="{${smoke_config}\"bundle\":{\"macOS\":{\"minimumSystemVersion\":\"13.5\",\"signingIdentity\":\"-\"},\"resources\":{\"../$core_dir/bootstrap-core.zip\":\"bootstrap-core.zip\",\"../$core_dir/bootstrap-core.json\":\"bootstrap-core.json\"}}}"
npm exec --yes --package=@tauri-apps/cli@2.12.6 -- tauri build \
  --target aarch64-apple-darwin --bundles app --config "$config" -- --locked

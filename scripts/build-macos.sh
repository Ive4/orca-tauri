#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
runtime=bun
case "${1:-}" in
  --runtime) runtime="${2:?Expected bun or node after --runtime}"; shift 2 ;;
  --runtime=*) runtime="${1#*=}"; shift ;;
esac

case "$runtime" in
  bun) core_dir=dist-core/macos-arm64-bun-plugins ;;
  node) core_dir=dist-core/macos-arm64-node-plugins ;;
  *) printf 'Unknown runtime: %s\n' "$runtime" >&2; exit 2 ;;
esac
python3 scripts/extract-macos-core.py "$@" --runtime "$runtime" --output "$core_dir"

config="{\"bundle\":{\"macOS\":{\"minimumSystemVersion\":\"13.5\",\"signingIdentity\":\"-\"},\"resources\":{\"../$core_dir/bootstrap-core.zip\":\"bootstrap-core.zip\",\"../$core_dir/bootstrap-core.json\":\"bootstrap-core.json\"}}}"
npm exec --yes --package=@tauri-apps/cli@2.12.6 -- tauri build \
  --target aarch64-apple-darwin --bundles app --config "$config" -- --locked

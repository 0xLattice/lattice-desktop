#!/usr/bin/env bash
# Local preparation only: never uploads, notarizes, or creates credentials.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" = Darwin && "$(uname -m)" = arm64 ]]
[[ -z "${ALLOW_OTHER_COMMIT:-}" ]]
output="${1:?Specify a NEW release output directory}"
[[ ! -e "$output" ]]
[[ -z "$(git status --porcelain)" ]] || { echo 'Commit and review the source before a release build.' >&2; exit 1; }
if git submodule status --recursive | grep -q '^[+U-]'; then
  echo 'Initialize submodules at their pinned commits before a release build.' >&2
  exit 1
fi
export LATTICE_RELEASE=1 ZERO_AR_DATE=1 TZ=UTC LC_ALL=C
export LATTICE_NODE_SOURCE="${LATTICE_NODE_SOURCE:-/tmp/lattice-release-node}"
export SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)"
export RUSTFLAGS="--remap-path-prefix=$PWD=/src/lattice-desktop --remap-path-prefix=$HOME=/build-home"
export CFLAGS="-ffile-prefix-map=$PWD=/src/lattice-desktop -ffile-prefix-map=$HOME=/build-home"
export CXXFLAGS="$CFLAGS"
unset APPLE_SIGNING_IDENTITY APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID
# Swift embeds some literal source paths; serialize the canonical build root.
lock=/tmp/lattice-release-build.lock
mkdir "$lock" || { echo 'Another release build holds the canonical build root.' >&2; exit 1; }
trap 'rmdir "$lock"' EXIT
scripts/build-node-binaries.sh
npm run typecheck
npm run build
npx --no-install tauri build --bundles app --no-sign -- --locked
app=src-tauri/target/release/bundle/macos/Lattice.app
python3 scripts/bundle-swift-runtime.py "$app"
python3 scripts/stage-swift-resources.py "$app"
mkdir -p "$app/Contents/Resources"
python3 scripts/collect-release-notices.py "$app/Contents/Resources/THIRD-PARTY-NOTICES.txt"
for name in lattice lattice-node lattice-mining-coordinator lattice-miner lattice-miner-gpu lattice-desktop; do
  codesign --force --sign - --timestamp=none "$app/Contents/MacOS/$name"
done
codesign --force --sign - --timestamp=none --entitlements src-tauri/Entitlements.plist "$app"
python3 scripts/release-macos.py package "$app" "$output"
python3 scripts/release-macos.py verify "$output"

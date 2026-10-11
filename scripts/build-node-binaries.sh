#!/usr/bin/env bash
# Build the bundled node binaries (`lattice`, `lattice-node`, and the miner's
# `lattice-mining-coordinator` and `lattice-miner`) from
# adalinxx/lattice-node at the commit in NODE_COMMIT and stage them as Tauri
# sidecars in src-tauri/binaries/<name>-<target-triple>, with the GPU mining
# worker from adalinxx/lattice-miner-gpu at MINER_GPU_COMMIT.
#
# DEBUG configuration on purpose: Swift 6.3's -O miscompiles the node on
# macOS (Task.sleep loops), so the Mac node is built unoptimized, exactly
# as the Mac Nexus node is today. macOS binaries must be built on macOS.
#
# Usage: scripts/build-node-binaries.sh [path-to-lattice-node-checkout]
#   Without a path, clones adalinxx/lattice-node into .build-node/.
#   The checkout must be clean and at NODE_COMMIT (override: ALLOW_OTHER_COMMIT=1).
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
commit="$(tr -d '[:space:]' < "$here/NODE_COMMIT")"
src="${1:-$here/.build-node}"
if [ "${LATTICE_RELEASE:-0}" = 1 ]; then
  src="${LATTICE_NODE_SOURCE:-/tmp/lattice-release-node}"
  case "$src" in /tmp/lattice-release-node|/private/tmp/lattice-release-node) ;; *) echo 'Release node source must use the canonical /tmp/lattice-release-node path' >&2; exit 1;; esac
fi

if [ ! -d "$src/.git" ] && [ ! -f "$src/.git" ]; then
  git clone --quiet https://github.com/adalinxx/lattice-node.git "$src"
fi
if [ -z "${1:-}" ] || [ "${LATTICE_RELEASE:-0}" = 1 ]; then
  git -C "$src" fetch --quiet origin "$commit"
  git -C "$src" checkout --quiet --detach "$commit"
fi
src="$(cd "$src" && pwd -P)"
head="$(git -C "$src" rev-parse HEAD)"
if [ "$head" != "$commit" ] && [ "${ALLOW_OTHER_COMMIT:-0}" != 1 ]; then
  echo "lattice-node checkout is at $head, not NODE_COMMIT $commit" >&2
  exit 1
fi
if [ -n "$(git -C "$src" status --porcelain --untracked-files=normal)" ]; then
  echo "lattice-node checkout has local changes; refusing to bundle it" >&2
  exit 1
fi

# SwiftPM honours only the last --product flag, so build each one.
products="lattice lattice-node lattice-mining-coordinator lattice-miner"
flags=(--force-resolved-versions)
if [ "${LATTICE_RELEASE:-0}" = 1 ]; then
  flags+=(--disable-index-store -Xlinker -reproducible
    -Xswiftc -file-prefix-map -Xswiftc "$src=/src/lattice-node"
    -Xswiftc -debug-prefix-map -Xswiftc "$src=/src/lattice-node"
    -Xswiftc -prefix-serialized-debugging-options
    -Xcc "-ffile-prefix-map=$src=/src/lattice-node"
    -Xcxx "-ffile-prefix-map=$src=/src/lattice-node")
fi
for product in $products; do
  (cd "$src" && swift build -c debug "${flags[@]}" --product "$product")
done

triple="$(rustc -vV | sed -n 's/^host: //p')"
out="$here/src-tauri/binaries"
mkdir -p "$out"
for name in $products; do
  install -m 0755 "$src/.build/debug/$name" "$out/$name-$triple"
done
printf '%s\n' "$head" > "$out/COMMIT"

# The GPU mining worker (Metal on macOS), from adalinxx/lattice-miner-gpu at the
# commit in MINER_GPU_COMMIT. A plain compute program with no node code in it,
# so it is built optimized.
gpu_commit="$(tr -d '[:space:]' < "$here/MINER_GPU_COMMIT")"
gpu_src="$here/.build-miner-gpu"
if [ ! -d "$gpu_src/.git" ]; then
  git clone --quiet https://github.com/adalinxx/lattice-miner-gpu.git "$gpu_src"
fi
git -C "$gpu_src" fetch --quiet origin "$gpu_commit"
git -C "$gpu_src" checkout --quiet --detach "$gpu_commit"
if [ -n "$(git -C "$gpu_src" status --porcelain --untracked-files=normal)" ]; then
  echo "lattice-miner-gpu checkout has local changes; refusing to bundle it" >&2
  exit 1
fi
if [ "${LATTICE_RELEASE:-0}" = 1 ]; then
  (cd "$gpu_src" && RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-reproducible" cargo build --release --locked)
else
  (cd "$gpu_src" && cargo build --release --locked)
fi
install -m 0755 "$gpu_src/target/release/lattice-miner-gpu" "$out/lattice-miner-gpu-$triple"
printf '%s\n' "$gpu_commit" > "$out/MINER_GPU_COMMIT"
if [ "${LATTICE_RELEASE:-0}" = 1 ]; then
  for name in $products lattice-miner-gpu; do
    strip -S "$out/$name-$triple"
  done
fi
echo "staged $products ($head, debug, $triple) and lattice-miner-gpu ($gpu_commit, release) in $out"

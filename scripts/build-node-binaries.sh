#!/usr/bin/env bash
# Build the bundled node binaries (`lattice`, `lattice-node`) from
# adalinxx/lattice-node at the commit in NODE_COMMIT and stage them as Tauri
# sidecars in src-tauri/binaries/<name>-<target-triple>.
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

if [ ! -d "$src/.git" ] && [ ! -f "$src/.git" ]; then
  git clone --quiet https://github.com/adalinxx/lattice-node.git "$src"
fi
if [ -z "${1:-}" ]; then
  git -C "$src" fetch --quiet origin "$commit"
  git -C "$src" checkout --quiet --detach "$commit"
fi
head="$(git -C "$src" rev-parse HEAD)"
if [ "$head" != "$commit" ] && [ "${ALLOW_OTHER_COMMIT:-0}" != 1 ]; then
  echo "lattice-node checkout is at $head, not NODE_COMMIT $commit" >&2
  exit 1
fi
if [ -n "$(git -C "$src" status --porcelain --untracked-files=no)" ]; then
  echo "lattice-node checkout has local changes; refusing to bundle it" >&2
  exit 1
fi

# SwiftPM honours only the last --product flag, so build each one.
for product in lattice lattice-node; do
  (cd "$src" && swift build -c debug --product "$product")
done

triple="$(rustc -vV | sed -n 's/^host: //p')"
out="$here/src-tauri/binaries"
mkdir -p "$out"
for name in lattice lattice-node; do
  install -m 0755 "$src/.build/debug/$name" "$out/$name-$triple"
done
printf '%s\n' "$head" > "$out/COMMIT"
echo "staged lattice + lattice-node ($head, debug, $triple) in $out"

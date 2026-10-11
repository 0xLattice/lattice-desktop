#!/usr/bin/env python3
"""Preserve dependency license/notice files without copying build metadata."""
import json
import os
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parent.parent
records = ['\n## Swift compatibility runtime — Swift 6.3.3 LICENSE.txt\n\n' + (root / 'docs/licenses/Swift-LICENSE.txt').read_text()]
missing = []

def collect(label, package, recursive=False):
    matches = []
    candidates = package.rglob('*') if recursive else package.iterdir()
    for path in sorted(candidates):
        if not path.is_file() or path.is_symlink() or path.stat().st_size > 2_000_000:
            continue
        name = path.name.lower()
        if name.startswith(('license', 'licence', 'notice', 'copying', 'copyright')):
            matches.append((path.relative_to(package).as_posix(), path.read_text(errors='replace')))
    if not matches:
        fallback = root / 'docs/licenses' / (package.name + '.txt')
        if fallback.is_file():
            matches.append(('upstream license', fallback.read_text()))
        else:
            missing.append(label)
    for name, text in matches:
        records.append(f'\n## {label} — {name}\n\n{text}\n')

# Include all installed npm package notices (a conservative superset).
for base in [root, root / 'vendor/nexus-wallet', root / 'vendor/nexus-wallet/vendor/lattice-sdk']:
    lock = json.loads((base / 'package-lock.json').read_text())['packages']
    for manifest in sorted((base / 'node_modules').glob('*/package.json')) + sorted((base / 'node_modules').glob('@*/*/package.json')):
        if lock.get(manifest.parent.relative_to(base).as_posix(), {}).get('dev', False):
            continue
        package = json.loads(manifest.read_text())
        collect(f"npm {package['name']} {package.get('version', '')}", manifest.parent)

for package in sorted((Path(os.environ.get('LATTICE_NODE_SOURCE', '/tmp/lattice-release-node')) / '.build/checkouts').iterdir()):
    if package.is_dir():
        # Nested notices include vendored BoringSSL and other embedded sources.
        collect('Swift ' + package.name, package, recursive=True)

for manifest in [root / 'src-tauri/Cargo.toml', root / '.build-miner-gpu/Cargo.toml']:
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version', '1', '--filter-platform', 'aarch64-apple-darwin', '--manifest-path', str(manifest)]))
    for package in metadata['packages']:
        if package['source']:
            collect(f"Cargo {package['name']} {package['version']}", Path(package['manifest_path']).parent, recursive=True)

# Multiple graphs can refer to the same package. Preserve each distinct notice once.
text = '# Third-party notices\n\nThis includes dependencies used to build Lattice.\n' + ''.join(sorted(set(records)))
Path(sys.argv[1]).write_text(text)
if missing:
    print('License files unavailable for: ' + ', '.join(sorted(set(missing))), file=sys.stderr)
    sys.exit(1)

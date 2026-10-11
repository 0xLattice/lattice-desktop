#!/usr/bin/env python3
"""Include Swift dependency privacy/resource bundles in the app resources."""
from pathlib import Path
import os
import shutil
import sys

source = Path(os.environ.get('LATTICE_NODE_SOURCE', '/tmp/lattice-release-node')) / '.build/debug'
destination = Path(sys.argv[1]) / 'Contents/Resources'
destination.mkdir(exist_ok=True)
for bundle in sorted(source.glob('*.bundle')):
    shutil.copytree(bundle, destination / bundle.name, dirs_exist_ok=True)
    for path in (destination / bundle.name).rglob('*'):
        path.chmod(0o755 if path.is_dir() else 0o644)

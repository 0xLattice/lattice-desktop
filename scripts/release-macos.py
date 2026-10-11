#!/usr/bin/env python3
"""Build-output verification, canonical packaging, and byte-level comparison."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import stat
import subprocess
import sys
import zipfile

BINARIES = {'lattice-desktop', 'lattice', 'lattice-node', 'lattice-mining-coordinator', 'lattice-miner', 'lattice-miner-gpu'}
PATTERNS = [rb'/Users/[^/\s\x00]+', rb'/home/[^/\s\x00]+', rb'/private/var/folders/', rb'-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----']

def run(*args):
    return subprocess.check_output(args, stderr=subprocess.STDOUT).decode().strip()

def sha(data):
    return hashlib.sha256(data).hexdigest()

def inventory(root):
    result = {}
    for path in sorted(root.rglob('*')):
        if path.is_symlink():
            raise ValueError('symlinks are not supported in this release layout')
        if path.is_file():
            result[path.relative_to(root).as_posix()] = {'sha256': sha(path.read_bytes()), 'mode': oct(stat.S_IMODE(path.stat().st_mode))}
    return result

def audit(app):
    if app.name != 'Lattice.app':
        raise ValueError('expected Lattice.app')
    info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
    if info.get('LSMinimumSystemVersion') != '15.0':
        raise ValueError('expected macOS 15 minimum')
    if info['CFBundleIdentifier'] != 'build.lattice.desktop':
        raise ValueError('unexpected bundle identity')
    if {p.name for p in (app / 'Contents/MacOS').iterdir()} != BINARIES:
        raise ValueError('unexpected executable inventory')
    patterns = PATTERNS[:]
    # Optional private denylist stays outside the source and release outputs.
    if os.environ.get('LATTICE_AUDIT_DENYLIST'):
        patterns += [re.escape(s) for s in Path(os.environ['LATTICE_AUDIT_DENYLIST']).read_bytes().splitlines() if s]
    for path in sorted(app.rglob('*')):
        relative = path.relative_to(app).as_posix()
        if path.is_symlink():
            raise ValueError('unexpected symbolic link')
        if path.name.startswith('.') or path.suffix in {'.map', '.dSYM', '.key', '.pem'}:
            raise ValueError('unexpected hidden, debug, or credential file')
        data = relative.encode() + (path.read_bytes() if path.is_file() else b'')
        if any(re.search(pattern, data, re.I) for pattern in patterns):
            # Never echo the matched identifier into publishable logs.
            raise ValueError('privacy scan failed; inspect locally')
        if path.is_file() and run('file', '-b', str(path)).startswith('Mach-O'):
            if run('lipo', '-archs', str(path)) != 'arm64':
                raise ValueError('expected Apple Silicon only')
            run('codesign', '--verify', '--strict', str(path))
            signature = run('codesign', '-dvv', str(path))
            if 'Signature=adhoc' not in signature or 'TeamIdentifier=not set' not in signature:
                raise ValueError('expected anonymous ad-hoc signature')
            commands = run('otool', '-l', str(path))
            if 'cmd LC_UUID' not in commands:
                raise ValueError('native binary lacks a loader-required UUID command')
            rpaths = re.findall(r'cmd LC_RPATH\n.*?path (.*?) \(offset', commands, re.S)
            if any(p not in {'/usr/lib/swift', '@loader_path/../Frameworks', '@loader_path'} for p in rpaths):
                raise ValueError('unexpected runtime search path')
            minimums = re.findall(r'minos ([0-9.]+)', run('vtool', '-show-build', str(path)))
            if any(tuple(map(int, v.split('.'))) > (15, 0) for v in minimums):
                raise ValueError('binary requires a newer OS than advertised')
            dependencies = run('otool', '-L', str(path)).splitlines()[1:]
            for dependency in dependencies:
                name = dependency.strip().split(' (')[0]
                if not name.startswith(('/usr/lib/', '/System/Library/')) and not (name.startswith('@rpath/') and (app / 'Contents/Frameworks' / name.removeprefix('@rpath/')).is_file()):
                    raise ValueError('non-system dynamic dependency requires review')
    run('codesign', '--verify', '--deep', '--strict', str(app))
    return info

def package(app, output):
    info = audit(app)
    version = info['CFBundleShortVersionString']
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?', version):
        raise ValueError('unsafe version')
    output.mkdir(parents=True, exist_ok=False)
    archive = output / f'Lattice-{version}-macos-arm64.zip'
    # Fixed zip fields avoid usernames, extended attributes, and local times.
    with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for path in sorted([app, *app.rglob('*')]):
            name = path.relative_to(app.parent).as_posix() + ('/' if path.is_dir() else '')
            entry = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
            entry.create_system = 3
            mode = 0o755 if path.is_dir() or path.stat().st_mode & 0o111 else 0o644
            entry.external_attr = ((stat.S_IFDIR if path.is_dir() else stat.S_IFREG) | mode) << 16
            entry.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(entry, b'' if path.is_dir() else path.read_bytes(), compresslevel=9)
    manifest = inventory(app)
    for entry in manifest.values():
        entry['mode'] = '0o755' if int(entry['mode'], 8) & 0o111 else '0o644'
    (output / 'files.json').write_text(json.dumps(manifest, indent=2, sort_keys=True) + '\n')
    names = [archive.name, 'files.json']
    (output / 'SHA256SUMS').write_text(''.join(f'{sha((output / n).read_bytes())}  {n}\n' for n in names))
    print(archive.name)

def verify(directory):
    lines = (directory / 'SHA256SUMS').read_text().splitlines()
    expected = {'files.json'}
    archives = list(directory.glob('Lattice-*-macos-arm64.zip'))
    if len(archives) != 1:
        raise ValueError('expected one release archive')
    expected.add(archives[0].name)
    seen = set()
    for line in lines:
        digest, name = line.split('  ', 1)
        if name not in expected or name in seen or not re.fullmatch('[0-9a-f]{64}', digest):
            raise ValueError('invalid checksum manifest')
        seen.add(name)
        if sha((directory / name).read_bytes()) != digest:
            raise ValueError('checksum mismatch')
    if seen != expected:
        raise ValueError('incomplete checksum manifest')
    with zipfile.ZipFile(archives[0]) as z:
        actual = {}
        for entry in z.infolist():
            if stat.S_IFMT(entry.external_attr >> 16) not in {stat.S_IFDIR, stat.S_IFREG}:
                raise ValueError('unsafe archive member type')
            if not entry.filename.startswith('Lattice.app/') or '..' in Path(entry.filename).parts or entry.extra or entry.comment:
                raise ValueError('unsafe archive metadata')
            if not entry.is_dir():
                name = entry.filename.removeprefix('Lattice.app/')
                if name in actual:
                    raise ValueError('duplicate archive member')
                actual[name] = {'sha256': sha(z.read(entry)), 'mode': oct(stat.S_IMODE(entry.external_attr >> 16))}
        if actual != json.loads((directory / 'files.json').read_text()):
            raise ValueError('archive content mismatch')
    print('Checksums and archive contents verified; publisher authentication requires a trusted project signature.')

def compare(first, second):
    a, b = inventory(first), inventory(second)
    changed = [p for p in sorted(a.keys() | b.keys()) if a.get(p) != b.get(p)]
    print(json.dumps({'identical': not changed, 'files_compared': len(a.keys() | b.keys()), 'changed_files': changed}, indent=2))
    return bool(changed)

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['audit', 'package', 'verify', 'compare'])
    parser.add_argument('path', type=Path)
    parser.add_argument('other', type=Path, nargs='?')
    args = parser.parse_args()
    try:
        if args.action == 'audit':
            audit(args.path)
            print('Bundle signatures, architecture, dependencies, and privacy scan passed.')
        elif args.action == 'package':
            package(args.path, args.other)
        elif args.action == 'verify':
            verify(args.path)
        else:
            sys.exit(compare(args.path, args.other))
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f'Verification failed ({type(error).__name__}); inspect locally.', file=sys.stderr)
        sys.exit(1)

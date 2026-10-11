#!/usr/bin/env python3
"""Bundle the toolchain-selected compatibility runtime and remove build rpaths."""
from pathlib import Path
import re
import subprocess
import sys

app = Path(sys.argv[1])
macos = app / 'Contents/MacOS'
frameworks = app / 'Contents/Frameworks'
frameworks.mkdir(exist_ok=True)
libraries = subprocess.check_output(['xcrun', 'swift-stdlib-tool', '--print', '--platform', 'macosx', '--scan-folder', str(macos)], text=True).splitlines()
for library in libraries:
    source = Path(library)
    subprocess.run(['lipo', str(source), '-thin', 'arm64', '-output', str(frameworks / source.name)], check=True)
for binary in [*macos.iterdir(), *frameworks.iterdir()]:
    commands = subprocess.check_output(['otool', '-l', str(binary)], text=True)
    rpaths = re.findall(r'cmd LC_RPATH\n.*?path (.*?) \(offset', commands, re.S)
    for path in rpaths:
        if path != '/usr/lib/swift':
            subprocess.run(['install_name_tool', '-delete_rpath', path, str(binary)], check=True)
    if binary.parent == macos and any('@rpath/' in line for line in subprocess.check_output(['otool', '-L', str(binary)], text=True).splitlines()[1:]):
        subprocess.run(['install_name_tool', '-add_rpath', '@loader_path/../Frameworks', str(binary)], check=True)
    subprocess.run(['codesign', '--force', '--sign', '-', '--timestamp=none', str(binary)], check=True)

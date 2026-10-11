#!/usr/bin/env python3
import importlib.util
import json
import plistlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('release', Path(__file__).with_name('release-macos.py'))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)

notice_spec = importlib.util.spec_from_file_location('notices', Path(__file__).with_name('collect-release-notices.py'))
notices = importlib.util.module_from_spec(notice_spec)
notice_spec.loader.exec_module(notices)

class ReleaseTests(unittest.TestCase):
    def test_source_grant_requires_exact_clean_revision(self):
        revision = notices.grants['Ivy']['revision']
        with patch.object(notices.subprocess, 'check_output', side_effect=[revision + '\n', '']):
            text = notices.source_grant(Path('Ivy'))
            self.assertIn(revision, text)
            self.assertIn('Copyright (c) Lattice contributors', text)
        for result in [('0' * 40, ''), (revision, ' M Sources/example.swift')]:
            with patch.object(notices.subprocess, 'check_output', side_effect=result):
                with self.assertRaisesRegex(ValueError, 'exact clean pinned revision'):
                    notices.source_grant(Path('Ivy'))
        self.assertIsNone(notices.source_grant(Path('unlisted-component')))

    def test_source_grant_preserves_existing_notices(self):
        with tempfile.TemporaryDirectory() as directory:
            package = Path(directory) / 'Ivy'
            package.mkdir()
            (package / 'LICENSE.third-party').write_text('Existing third-party terms')
            with patch.object(notices, 'records', []), patch.object(notices, 'source_grant', return_value='Scoped component grant'):
                notices.collect('Swift Ivy', package, recursive=True)
                combined = ''.join(notices.records)
                self.assertIn('Existing third-party terms', combined)
                self.assertIn('Scoped component grant', combined)

    def test_archive_is_canonical_and_detects_tampering(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            app = root / 'Lattice.app'
            (app / 'Contents/MacOS').mkdir(parents=True)
            binary = app / 'Contents/MacOS/test'
            binary.write_bytes(b'fixture')
            binary.chmod(0o755)
            resource = app / 'Contents/read-only.txt'
            resource.write_text('resource')
            resource.chmod(0o444)
            with patch.object(release, 'audit', return_value={'CFBundleShortVersionString': '0.1.0'}):
                release.package(app, root / 'a')
                release.package(app, root / 'b')
            a = root / 'a/Lattice-0.1.0-macos-arm64.zip'
            b = root / 'b/Lattice-0.1.0-macos-arm64.zip'
            self.assertEqual(a.read_bytes(), b.read_bytes())
            release.verify(root / 'a')
            a.write_bytes(a.read_bytes() + b'tampered')
            with self.assertRaises(ValueError):
                release.verify(root / 'a')

    def test_comparison_checks_bytes_and_modes(self):
        with tempfile.TemporaryDirectory() as directory:
            a, b = Path(directory) / 'a', Path(directory) / 'b'
            a.mkdir(); b.mkdir()
            (a / 'binary').write_bytes(b'a')
            (b / 'binary').write_bytes(b'b')
            self.assertTrue(release.compare(a, b))
            (b / 'binary').write_bytes(b'a')
            self.assertFalse(release.compare(a, b))
            (b / 'binary').chmod(0o755)
            self.assertTrue(release.compare(a, b))

    def test_bundle_rejects_private_paths_and_missing_helpers(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory) / 'Lattice.app'
            macos = app / 'Contents/MacOS'
            macos.mkdir(parents=True)
            (app / 'Contents/Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier': 'build.lattice.desktop', 'LSMinimumSystemVersion': '15.0'}))
            for name in release.BINARIES:
                (macos / name).write_bytes(b'fixture')
            with patch.object(release, 'run', return_value='ASCII text'):
                release.audit(app)
                (macos / 'lattice').write_bytes(b'/Users/example/source')
                with self.assertRaisesRegex(ValueError, 'privacy'):
                    release.audit(app)
                (macos / 'lattice').unlink()
                with self.assertRaisesRegex(ValueError, 'inventory'):
                    release.audit(app)

    def test_bundle_rejects_missing_native_uuid(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory) / 'Lattice.app'
            macos = app / 'Contents/MacOS'
            macos.mkdir(parents=True)
            (app / 'Contents/Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier': 'build.lattice.desktop', 'LSMinimumSystemVersion': '15.0'}))
            for name in release.BINARIES:
                (macos / name).write_bytes(b'fixture')
            def output(*args):
                if args[0] == 'file': return 'Mach-O 64-bit executable arm64'
                if args[0] == 'lipo': return 'arm64'
                if args[0] == 'codesign': return 'Signature=adhoc\nTeamIdentifier=not set'
                return ''
            with patch.object(release, 'run', side_effect=output):
                with self.assertRaisesRegex(ValueError, 'UUID'):
                    release.audit(app)

    def test_manifest_cannot_escape_download_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'Lattice-0.1.0-macos-arm64.zip').write_bytes(b'')
            (root / 'SHA256SUMS').write_text('0' * 64 + '  ../outside\n')
            with self.assertRaisesRegex(ValueError, 'manifest'):
                release.verify(root)

    def test_symlink_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'link').symlink_to('/etc/passwd')
            with self.assertRaises(ValueError):
                release.inventory(root)

if __name__ == '__main__':
    unittest.main()

import fnmatch
import hashlib
import importlib.util
import os
import re
import subprocess
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('rootshell', 'scripts/rootshell-manifest.py')
rootshell = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rootshell)


class RootshellReleaseTests(unittest.TestCase):
    def test_release_tags_have_one_publishing_workflow(self):
        workflows = [
            Path('.github/workflows/rootshell-release.yml'),
            Path('.github/workflows/arm64-sync-and-release.yml'),
        ]
        patterns = {}
        for path in workflows:
            if not path.exists():
                continue
            tags = re.search(
                r'(?m)^  push:\n    tags:\n((?:      - .*\n)+)', path.read_text()
            )
            self.assertIsNotNone(tags, str(path))
            patterns[path.name] = re.findall(r'"([^"]+)"', tags[1])
        for tag, expected in [
            ('rootshell-v0.1.14', ['rootshell-release.yml']),
            ('v0.9.5-rootshell.0.1.14', ['rootshell-release.yml']),
            ('v0.9.5-arm64.0.1.14', []),
        ]:
            with self.subTest(tag=tag):
                publishers = [
                    name for name, globs in patterns.items()
                    if any(fnmatch.fnmatchcase(tag, glob) for glob in globs)
                ]
                self.assertEqual(publishers, expected)

    def test_channel_publish_failure_fails_the_workflow_step(self):
        workflow = Path('.github/workflows/rootshell-release.yml').read_text()
        step = workflow.split('      - name: Publish fork update channel\n', 1)[1]
        run = step.split('        run: |\n', 1)[1]
        command = '\n'.join(re.findall(r'(?m)^          (.*)$', run))
        self.assertTrue(command.strip())
        with tempfile.TemporaryDirectory() as folder:
            python = Path(folder) / 'python3'
            python.write_text('#!/bin/sh\nexit 42\n', encoding='utf-8', newline='\n')
            python.chmod(0o755)
            result = subprocess.run(
                ['sh', '-e', '-c', 'PATH="$PWD:/usr/bin:/bin"\nexport PATH\n' + command],
                cwd=folder,
                capture_output=True,
            )
            self.assertEqual(result.returncode, 42)

    def test_channel_promotion_never_regresses(self):
        self.assertTrue(rootshell.should_promote({'version': '0.1.9'}, {'version': '0.1.10'}))
        self.assertFalse(rootshell.should_promote({'version': '0.2.0'}, {'version': '0.1.10'}))
        self.assertFalse(rootshell.should_promote({'version': '0.1.0'}, {'version': '0.1.0'}))
        with self.assertRaises(ValueError):
            rootshell.should_promote({'version': '0.1.0', 'notes': 'old'}, {'version': '0.1.0', 'notes': 'new'})

    def test_install_and_checksum_failure(self):
        installer = Path('scripts/rootshell-install.sh').resolve()
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / 'installer.sh').write_text(
                installer.read_text(encoding='utf-8'), encoding='utf-8', newline='\n'
            )
            mocks = root / 'mocks'
            mocks.mkdir()
            curl = mocks / 'curl'
            curl.write_text('''#!/bin/sh
while [ "$1" != -o ]; do shift; done
case "$3" in
  */rootshell.json) printf '{\n  "version": "0.1.0"\n}\n' > "$2" ;;
  *.sha256) asset=${3##*/}; printf '%s  %s\\n' "$TEST_DIGEST" "${asset%.sha256}" > "$2" ;;
  *) printf test-binary > "$2" ;;
esac
''', encoding='utf-8', newline='\n')
            curl.chmod(0o755)
            uname = mocks / 'uname'
            uname.write_text('''#!/bin/sh
case "$1" in
  -s) printf 'Linux\\n' ;;
  -m) printf 'x86_64\\n' ;;
  *) exit 1 ;;
esac
''', encoding='utf-8', newline='\n')
            uname.chmod(0o755)
            command = [
                'sh', '-c',
                'HOME="$PWD" PATH="$PWD/mocks:/usr/bin:/bin" exec sh ./installer.sh',
            ]
            env = dict(os.environ, SHELL='/bin/bash',
                       TEST_DIGEST=hashlib.sha256(b'test-binary').hexdigest())
            for _ in range(2):
                result = subprocess.run(command, cwd=root, env=env, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            dest = root / '.local/opt/herdr-rootshell/bin/herdr'
            self.assertEqual(dest.read_bytes(), b'test-binary')
            self.assertEqual((root / '.bashrc').read_text().count('# rootshell-herdr'), 1)
            env['TEST_DIGEST'] = '0' * 64
            result = subprocess.run(command, cwd=root, env=env, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(dest.read_bytes(), b'test-binary')

    def test_assets_and_tag_validation(self):
        with tempfile.TemporaryDirectory() as folder:
            assets = Path(folder)
            for target in ('linux-x86_64', 'linux-aarch64', 'macos-x86_64', 'macos-aarch64', 'windows-x86_64.zip'):
                (assets / f'herdr-{target}').write_bytes(b'binary')
            result = rootshell.manifest('rootshell-v0.1.2', 'abc', assets_dir=assets)
            self.assertEqual(result['version'], '0.1.2')
            self.assertEqual(result['assets']['windows-x86_64']['format'], 'zip')
            self.assertEqual(result['assets']['macos-aarch64']['sha256'], hashlib.sha256(b'binary').hexdigest())
            with self.assertRaises(ValueError):
                rootshell.manifest('v0.1.2', 'abc', assets_dir=assets)

    def test_manifest_reads_utf8_sources_with_an_ansi_default_encoding(self):
        original_read_text = Path.read_text

        def read_with_windows_default(path, encoding=None, errors=None, **kwargs):
            return original_read_text(
                path, encoding=encoding or 'cp1252', errors=errors, **kwargs
            )

        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            sources = {
                'src/protocol/wire.rs': '// UTF-8 marker: \u2701\npub const PROTOCOL_VERSION: u32 = 7;\n',
                'src/protocol/endpoint.rs': 'pub const ENDPOINT_PROTOCOL_GENERATION: u32 = 2;\n',
                'Cargo.toml': '[package]\nversion = "0.9.5"\n',
            }
            for name, contents in sources.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(contents, encoding='utf-8')
            # Model a Windows ANSI locale without changing the process-wide locale.
            with mock.patch.object(Path, 'read_text', read_with_windows_default):
                result = rootshell.manifest(
                    'rootshell-v0.1.2', 'abc', root=root, assets_dir=root / 'assets'
                )
            self.assertEqual(result['protocol'], 7)
            self.assertEqual(result['endpoint_generation'], 2)
            self.assertEqual(result['base_version'], '0.9.5')

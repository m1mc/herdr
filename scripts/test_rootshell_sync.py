import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from scripts.rootshell_sync import next_build_id


SCRIPT = Path('scripts/rootshell_sync.py').resolve()


class RootshellSyncTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = dict(os.environ, GIT_AUTHOR_NAME='Test', GIT_AUTHOR_EMAIL='test@example.org',
                        GIT_COMMITTER_NAME='Test', GIT_COMMITTER_EMAIL='test@example.org',
                        GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull)
        self.upstream = self.root / 'upstream.git'
        self.origin = self.root / 'origin.git'
        for repo in (self.upstream, self.origin):
            self.run_git(self.root, 'init', '--bare', '--initial-branch=master', str(repo))
        self.source = self.root / 'source'
        self.run_git(self.root, 'clone', str(self.upstream), str(self.source))
        (self.source / 'Cargo.toml').write_text('[package]\nversion = "0.9.5"\n')
        self.commit(self.source, 'initial')
        self.run_git(self.source, 'push', 'origin', 'master')
        self.run_git(self.source, 'remote', 'add', 'fork', str(self.origin))
        self.run_git(self.source, 'push', 'fork', 'master')
        self.work = self.root / 'work'
        self.run_git(self.root, 'clone', str(self.origin), str(self.work))
        (self.work / 'fork-only').write_text('preserve customizations\n')
        self.commit(self.work, 'fork customization')
        self.run_git(self.work, 'push', 'origin', 'master')
        self.run_git(self.work, 'switch', '-c', 'rootshell-universal')
        self.run_git(self.work, 'tag', 'v0.9.5-rootshell.0.1.13')
        self.run_git(self.work, 'push', 'origin', 'rootshell-universal', '--tags')
        tools = self.root / 'bin'
        tools.mkdir()
        gh = tools / 'gh'
        gh.write_text('''#!/bin/sh
if [ "$1 $2" = "run list" ]; then printf '[]'; exit 0; fi
printf '%s\n' "$@" >> "$DISPATCH_LOG"
exit "${DISPATCH_EXIT:-0}"
''')
        gh.chmod(0o755)
        self.log = self.root / 'dispatch.log'
        self.env.update(PATH=f'{tools}:{os.environ["PATH"]}', DISPATCH_LOG=str(self.log))

    def run_git(self, cwd, *args):
        return subprocess.check_output(['git', *args], cwd=cwd, env=self.env,
                                       text=True, stderr=subprocess.DEVNULL).strip()

    def commit(self, repo, message):
        self.run_git(repo, 'add', '.')
        self.run_git(repo, 'commit', '-m', message)

    def update_upstream(self, tag=None):
        (self.source / 'upstream-only').write_text('new upstream content\n')
        self.commit(self.source, 'upstream update')
        if tag:
            self.run_git(self.source, 'tag', tag)
        self.run_git(self.source, 'push', 'origin', 'master', '--tags')

    def sync(self, *args, success=True):
        result = subprocess.run(['python3', str(SCRIPT), '--upstream', str(self.upstream),
                                 '--repo', 'example/fork', *args], cwd=self.work,
                                env=self.env, capture_output=True, text=True)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0)
        return result

    def test_commit_only_sync_preserves_customizations_without_release(self):
        self.update_upstream()
        self.sync()
        for branch in ('master', 'rootshell-universal'):
            self.assertEqual(self.run_git(self.root, '--git-dir', str(self.origin),
                                         'show', f'{branch}:fork-only'), 'preserve customizations')
            self.assertEqual(self.run_git(self.root, '--git-dir', str(self.origin),
                                         'show', f'{branch}:upstream-only'), 'new upstream content')
        self.assertFalse(self.log.exists())

    def test_new_upstream_tag_dispatches_once_and_retry_reuses_build_tag(self):
        self.update_upstream('v0.9.6')
        self.env['DISPATCH_EXIT'] = '42'
        self.sync(success=False)
        self.assertNotIn('refs/tags/v0.9.6', self.run_git(self.root, 'ls-remote', '--tags', str(self.origin)))
        self.env['DISPATCH_EXIT'] = '0'
        self.sync()
        tags = self.run_git(self.root, '--git-dir', str(self.origin), 'tag', '--list').splitlines()
        self.assertEqual(tags, ['v0.9.5-rootshell.0.1.13', 'v0.9.5-rootshell.0.1.14', 'v0.9.6'])
        self.assertIn('release_tag=v0.9.5-rootshell.0.1.14', self.log.read_text())
        before = self.log.read_text()
        self.sync()
        self.assertEqual(self.log.read_text(), before)

    def test_dry_run_does_not_push_or_dispatch(self):
        self.update_upstream('v0.9.6')
        before = self.run_git(self.root, 'ls-remote', str(self.origin))
        self.sync('--dry-run')
        self.assertEqual(self.run_git(self.root, 'ls-remote', str(self.origin)), before)
        self.assertFalse(self.log.exists())

    def test_merge_conflict_stops_before_importing_tags_or_dispatching(self):
        (self.work / 'Cargo.toml').write_text('[package]\nversion = "0.9.7"\n')
        self.commit(self.work, 'local version change')
        self.run_git(self.work, 'push', 'origin', 'rootshell-universal')
        (self.source / 'Cargo.toml').write_text('[package]\nversion = "0.9.6"\n')
        self.commit(self.source, 'upstream version change')
        self.run_git(self.source, 'tag', 'v0.9.6')
        self.run_git(self.source, 'push', 'origin', 'master', '--tags')
        self.sync(success=False)
        self.assertFalse(self.log.exists())
        self.assertNotIn('refs/tags/v0.9.6', self.run_git(self.root, 'ls-remote', '--tags', str(self.origin)))

    def test_build_counter_is_independent_of_upstream_version(self):
        self.assertEqual(next_build_id(['v9.0.0-rootshell.0.1.2', 'v1.0.0-rootshell.0.3.8',
                                        'rootshell-v0.2.9', 'preview-example']), '0.3.9')


if __name__ == '__main__':
    unittest.main()

"""Sync upstream into the fork; publish only when previously unseen tags arrive."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess


def git(*args):
    return subprocess.check_output(['git', *args], text=True).strip()


def remote_tags(remote):
    tags = set()
    for line in git('ls-remote', '--tags', remote).splitlines():
        _, ref = line.split()
        if not ref.endswith('^{}'):
            tags.add(ref.removeprefix('refs/tags/'))
    return tags


def next_build_id(tags):
    versions = []
    for tag in tags:
        match = re.fullmatch(r'(?:rootshell-v|v[^-]+-rootshell\.)(\d+)\.(\d+)\.(\d+)', tag)
        if match:
            versions.append(tuple(map(int, match.groups())))
    major, minor, patch = max(versions, default=(0, 1, 0))
    return f'{major}.{minor}.{patch + 1}'


def sync(upstream, repo, dry_run=False):
    if git('status', '--porcelain'):
        raise ValueError('Upstream sync requires a clean checkout')
    if 'upstream' not in git('remote').splitlines():
        git('remote', 'add', 'upstream', upstream)
    elif git('remote', 'get-url', 'upstream') != upstream:
        raise ValueError('Existing upstream remote does not match the requested source')

    new_tags = remote_tags('upstream') - remote_tags('origin')
    git('fetch', 'origin', 'master', 'arm64-rootshell', '--tags')
    git('fetch', 'upstream', 'master', '--tags')
    print(f'Upstream tags absent from the fork: {len(new_tags)}', flush=True)
    if dry_run:
        print('Dry run: no branches, tags, or releases were pushed.', flush=True)
        return

    git('switch', '--detach', 'origin/master')
    git('merge', '--no-edit', 'upstream/master')
    git('push', 'origin', 'HEAD:refs/heads/master')
    git('switch', 'arm64-rootshell')
    git('merge', '--ff-only', 'origin/arm64-rootshell')
    git('merge', '--no-edit', 'upstream/master')
    git('push', 'origin', 'HEAD:refs/heads/arm64-rootshell')
    if not new_tags:
        print('Commits synchronized; no new upstream tags, so no release.', flush=True)
        return

    base = re.search(r'^version = "(\d+\.\d+\.\d+)', Path('Cargo.toml').read_text(), re.M)
    if base is None:
        raise ValueError('Cannot determine the upstream package version')
    marker = f'Upstream-Tags: {" ".join(sorted(new_tags))}'
    tag = next((
        candidate for candidate in git('tag', '--points-at', 'HEAD').splitlines()
        if '-rootshell.' in candidate
        and marker in git('for-each-ref', '--format=%(contents)', f'refs/tags/{candidate}').splitlines()
    ), None)
    if tag is None:
        tag = f'v{base[1]}-rootshell.{next_build_id(git("tag", "--list").splitlines())}'
        git('tag', '-a', tag, '-m', f'{tag}\n\n{marker}')
        git('push', 'origin', f'refs/tags/{tag}')
    # GITHUB_TOKEN tag pushes do not trigger push workflows. Dispatch explicitly.
    runs = json.loads(subprocess.check_output([
        'gh', 'run', 'list', '--repo', repo, '--workflow', 'rootshell-release.yml',
        '--branch', tag, '--json', 'status,conclusion',
    ], text=True))
    if not runs:
        subprocess.run([
            'gh', 'workflow', 'run', 'rootshell-release.yml', '--repo', repo,
            '--ref', tag, '-f', f'release_tag={tag}',
        ], check=True)
    elif all(run['status'] == 'completed' and run['conclusion'] != 'success' for run in runs):
        raise RuntimeError(f'Release for {tag} failed; rerun its failed jobs before retrying sync')
    # Imported upstream tags are the checkpoint: advance only after dispatch succeeds.
    refs = [f'refs/tags/{name}:refs/tags/{name}' for name in sorted(new_tags)]
    git('push', '--atomic', 'origin', *refs)
    print(f'Dispatched the six-platform release for {tag}.', flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--upstream', default='https://github.com/kitknox/herdr.git')
    parser.add_argument('--repo', default=os.environ.get('GITHUB_REPOSITORY', 'm1mc/herdr'))
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    sync(args.upstream, args.repo, args.dry_run)

#!/usr/bin/env python3
"""Retry a build only when Zig fails to download a package dependency.

libghostty-vt's Zig packages (vendor/libghostty-vt/zig-pkg) are not tracked,
so a cold build downloads them from deps.files.ghostty.org and codeberg.org.
Zig reports a failed download at the dependency's url in build.zig.zon, e.g.
`pkg/wuffs\\build.zig.zon:14:20: error: HTTP response read failure: Unexpected`.
Compiler errors and package hash mismatches fail immediately.
"""

import re
import subprocess
import sys
import time

ATTEMPTS = 4
DELAYS = (15, 30, 60)
ZIG_FETCH_ERROR = re.compile(r'build\.zig\.zon:\d+:\d+: error: (?P<message>.*)')


def is_download_failure(line):
    match = ZIG_FETCH_ERROR.search(line)
    return bool(match) and 'hash mismatch' not in match['message']


def run_build(command):
    for attempt in range(1, ATTEMPTS + 1):
        download_failed = False
        with subprocess.Popen(
            command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, encoding='utf-8', errors='replace',
        ) as child:
            for line in child.stdout:
                print(line, end='', flush=True)
                download_failed |= is_download_failure(line)
            code = child.wait()
        if code == 0 or not download_failed or attempt == ATTEMPTS:
            return code
        delay = DELAYS[attempt - 1]
        print(f'Zig package download failed; retrying build in {delay}s '
              f'({attempt + 1}/{ATTEMPTS}).', flush=True)
        time.sleep(delay)


if __name__ == '__main__':
    command = sys.argv[1:]
    if command[:1] == ['--']:
        command = command[1:]
    if not command:
        sys.exit('usage: retry_build.py -- COMMAND [ARG ...]')
    sys.exit(run_build(command))

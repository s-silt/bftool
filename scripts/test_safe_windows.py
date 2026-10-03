"""Run the audited Windows suite without real-volume or privileged link tests."""

import argparse
import json
import pathlib
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--offline', action='store_true')
    parser.add_argument('--target', help='Optional Cargo target triple')
    parser.add_argument('--print-command', action='store_true')
    options = parser.parse_args()
    if sys.platform != 'win32':
        parser.error('This policy was audited for Windows only; review Linux tests separately.')
    repo = pathlib.Path(__file__).resolve().parent.parent
    exclusions = json.loads((repo / 'scripts/windows_test_exclusions.json').read_text(encoding='utf-8'))
    command = ['cargo', 'test', '--workspace', '--all-targets', '--locked']
    if options.offline:
        command.append('--offline')
    if options.target:
        command.extend(['--target', options.target])
    command.extend(['--', '--test-threads=1'])
    for case in exclusions:
        command.extend(['--skip', case['test']])
    print('Audited Windows suite; exclusion policy:', len(exclusions), 'test names.', flush=True)
    print(subprocess.list2cmdline(command), flush=True)
    if options.print_command:
        return 0
    return subprocess.run(command, cwd=repo, check=False).returncode


if __name__ == '__main__':
    raise SystemExit(main())

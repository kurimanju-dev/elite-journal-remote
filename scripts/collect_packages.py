#!/usr/bin/env python3
"""Validate the complete six-target release set and write SHA256SUMS."""
import argparse
from pathlib import Path

from build import TARGETS, sha256


def collect(directory):
    archives = sorted([*directory.glob('*.tar.gz'), *directory.glob('*.zip')])
    expected = {entry['target'] for entry in TARGETS}
    found = set()
    lines = []
    release = None
    for archive in archives:
        matches = [target for target in expected
                   if archive.name.endswith(target + ('.zip' if 'windows' in target else '.tar.gz'))]
        if len(matches) != 1 or matches[0] in found:
            raise ValueError(f'Unexpected or duplicate platform archive: {archive.name}')
        prefix = archive.name.split(matches[0], 1)[0]
        if release is not None and release != prefix:
            raise ValueError('Platform archives have different release versions')
        release = prefix
        found.add(matches[0])
        line = f'{sha256(archive)}  {archive.name}\n'
        checksum = archive.with_name(archive.name + '.sha256')
        if not checksum.is_file() or checksum.read_text(encoding='utf-8') != line:
            raise ValueError(f'Missing or incorrect checksum: {archive.name}')
        lines.append(line)
    if found != expected:
        raise ValueError('Missing platform packages: ' + ', '.join(sorted(expected - found)))
    (directory / 'SHA256SUMS').write_text(''.join(lines), encoding='utf-8')
    return len(archives)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path, help='Directory containing six archives and their .sha256 files')
    args = parser.parse_args()
    print(f'Verified {collect(args.directory)} platform packages')

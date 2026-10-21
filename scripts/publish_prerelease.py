#!/usr/bin/env python3
"""Publish verified release assets with GitHub CLI, or safely resume publication."""
import argparse
import json
from pathlib import Path
import subprocess

from build import sha256


def gh(*args):
    return subprocess.check_output(['gh', *map(str, args)], text=True)


def publish(directory, tag, commit, title, notes, repository, retire_release=None):
    # The build pins a tag before compiling. Resolve annotated/lightweight tags
    # explicitly, rather than asking the release API to create a tag for an old
    # workflow commit (which requires a separate Workflows permission).
    reference = json.loads(gh('api', f'repos/{repository}/git/ref/tags/{tag}'))['object']
    for _ in range(8):
        if reference['type'] != 'tag':
            break
        reference = json.loads(gh('api', f"repos/{repository}/git/tags/{reference['sha']}"))['object']
    if reference['type'] != 'commit' or reference['sha'] != commit:
        raise ValueError('Prerelease tag does not point to the verified build commit')
    desired = {path.name: 'sha256:' + sha256(path) for path in directory.iterdir()}
    # The tag lookup endpoint excludes drafts. Listing with write permission
    # includes them, allowing a failed upload to resume the same draft safely.
    pages = json.loads(gh('api', '--paginate', '--slurp', f'repos/{repository}/releases?per_page=100'))
    release = next((item for page in pages for item in page if item['tag_name'] == tag), None)
    if release is None:
        release = json.loads(gh('api', '--method', 'POST', f'repos/{repository}/releases',
                                '-f', f'tag_name={tag}', '-f', f'name={title}',
                                '-f', 'body=' + notes.read_text(encoding='utf-8'),
                                '-F', 'draft=true', '-F', 'prerelease=true', '-f', 'make_latest=false'))
    endpoint = f"repos/{repository}/releases/{release['id']}"
    if not release['prerelease']:
        raise ValueError('Existing release is not the requested prerelease/commit')
    existing = {asset['name']: asset for asset in release['assets']}
    for path in sorted(directory.iterdir()):
        digest = 'sha256:' + sha256(path)
        asset = existing.get(path.name)
        if asset and asset.get('digest') == digest and asset['state'] == 'uploaded':
            continue
        # Resuming a partial draft is safe. Published binaries must stay intact;
        # the checksum manifest can be repaired after GitHub filename rewriting.
        if asset and not release['draft'] and path.name != 'SHA256SUMS':
            raise ValueError(f'Published asset differs: {path.name}')
        command = ['release', 'upload', tag, path]
        if asset:
            command.append('--clobber')
        gh(*command)
    verified = json.loads(gh('api', endpoint))
    assets = {asset['name']: asset for asset in verified['assets']}
    for path in directory.iterdir():
        if assets.get(path.name, {}).get('digest') != 'sha256:' + sha256(path):
            raise ValueError(f'Uploaded digest mismatch: {path.name}')
    # Remove the previous naming/layout only after every replacement is uploaded
    # and verified. Source archives remain available through GitHub itself.
    for name, asset in assets.items():
        if name not in desired:
            gh('api', '--method', 'DELETE', f"repos/{repository}/releases/assets/{asset['id']}")
    gh('release', 'edit', tag, '--notes-file', notes, '--title', title,
       '--draft=false', '--prerelease', '--latest=false')
    published = json.loads(gh('api', endpoint))
    if published['draft'] or {a['name']: a['digest'] for a in published['assets']} != desired:
        raise ValueError('Published release assets differ from the verified download set')
    if retire_release:
        if retire_release == tag:
            raise ValueError('Cannot retire the newly published release')
        old_pages = json.loads(gh('api', '--paginate', '--slurp', f'repos/{repository}/releases?per_page=100'))
        legacy = next((r for page in old_pages for r in page if r['tag_name'] == retire_release), None)
        if legacy:
            if legacy['name'] != '0.2.0' or legacy['id'] == published['id']:
                raise ValueError('Refusing to retire a release other than the legacy 0.2.0 release')
            gh('api', '--method', 'DELETE', f"repos/{repository}/releases/{legacy['id']}")
    return published['html_url']


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('--tag', required=True)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--title', required=True)
    parser.add_argument('--notes', type=Path, required=True)
    parser.add_argument('--repository', required=True)
    parser.add_argument('--retire-release', help='Remove the legacy 0.2.0 release with this tag after successful publication')
    args = parser.parse_args()
    print(publish(args.directory, args.tag, args.commit, args.title, args.notes, args.repository, args.retire_release))

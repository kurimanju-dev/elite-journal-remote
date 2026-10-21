#!/usr/bin/env python3
"""Stage consistently named GUI installers, CLI archives and release notes."""
import argparse
import gzip
import io
from pathlib import Path
import shutil
import tarfile
import tempfile
from urllib.parse import quote
import zipfile

from build import TARGETS, sha256
from collect_packages import collect

PLATFORMS = {
    'aarch64-apple-darwin': ('macos', 'arm64', 'Mac · Apple silicon (M1 or newer)'),
    'x86_64-apple-darwin': ('macos', 'x64', 'Mac · Intel'),
    'aarch64-pc-windows-msvc': ('windows', 'arm64', 'Windows · ARM64 (including Mac ARM VMs)'),
    'x86_64-pc-windows-msvc': ('windows', 'x64', 'Windows · Intel / AMD'),
    'aarch64-unknown-linux-gnu': ('linux', 'arm64', 'Linux · ARM64'),
    'x86_64-unknown-linux-gnu': ('linux', 'x64', 'Linux · Intel / AMD'),
}


def filenames(version, target):
    system, architecture, _ = PLATFORMS[target]
    prefix = f'elite-journal-relay-{version}-{system}-{architecture}'
    result = {'cli': prefix + ('-cli.zip' if system == 'windows' else '-cli.tar.gz')}
    if system == 'macos':
        result['dmg'] = prefix + '-gui.dmg'
    elif system == 'windows':
        result['setup'] = prefix + '-gui-setup.exe'
    else:
        for kind, extension in [('deb', '.deb'), ('rpm', '.rpm'), ('arch', '.pkg.tar.zst')]:
            result[kind] = prefix + '-gui' + extension
    return result


def cli_archive(destination, binary, windows):
    """Stable timestamps keep retries byte-identical for already published assets."""
    executable = 'elite-journal-relay' + ('.exe' if windows else '')
    members = {
        executable: (binary, 0o755),
        'README.txt': (b'Elite Journal Relay CLI\nRun elite-journal-relay --help for commands and options.\nFor the desktop app, download the GUI installer for your device.\n', 0o644),
        'LICENSE': ((Path(__file__).resolve().parents[1] / 'LICENSE').read_bytes(), 0o644),
    }
    if windows:
        with zipfile.ZipFile(destination, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
            for name, (data, mode) in members.items():
                info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = (0o100000 | mode) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, data)
    else:
        with destination.open('wb') as output:
            with gzip.GzipFile(fileobj=output, filename='', mode='wb', mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode='w') as archive:
                    for name, (data, mode) in members.items():
                        info = tarfile.TarInfo(name)
                        info.size, info.mode = len(data), mode
                        archive.addfile(info, io.BytesIO(data))


def prepare(directory, destination):
    collect(directory)
    if directory.resolve().is_relative_to(destination.resolve()):
        raise ValueError('Release destination must not contain the source packages')
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.release-', dir=destination.parent) as temporary:
        stage = Path(temporary) / 'assets'
        stage.mkdir()
        for entry in TARGETS:
            target = entry['target']
            extension = '.zip' if 'windows' in target else '.tar.gz'
            archive = next(directory.glob('*' + target + extension))
            version = archive.name[len('elite-relay-'):-(len(target) + len(extension) + 1)]
            names = filenames(version, target)
            cli = 'elite-journal-relay' + ('.exe' if 'windows' in target else '')
            found = set()

            def copy_member(name, stream):
                if name == f'{target}/{cli}':
                    cli_archive(stage / names['cli'], stream.read(), 'windows' in target)
                    found.add('cli')
                    return
                if not name.startswith(f'{target}/bundle/'):
                    return
                kind = next((kind for suffix, kind in [('.dmg', 'dmg'), ('-setup.exe', 'setup'),
                            ('.deb', 'deb'), ('.rpm', 'rpm'), ('.pkg.tar.zst', 'arch')]
                             if name.endswith(suffix)), None)
                if kind not in names:
                    return
                if kind in found:
                    raise ValueError(f'Duplicate {kind} installer: {target}')
                with (stage / names[kind]).open('wb') as handle:
                    shutil.copyfileobj(stream, handle)
                found.add(kind)

            if extension == '.zip':
                with zipfile.ZipFile(archive) as contents:
                    for member in contents.infolist():
                        if not member.is_dir():
                            with contents.open(member) as stream:
                                copy_member(member.filename, stream)
            else:
                with tarfile.open(archive) as contents:
                    for member in contents:
                        if member.isfile():
                            with contents.extractfile(member) as stream:
                                copy_member(member.name, stream)
            if found != set(names):
                raise ValueError(f'Missing downloads for {target}: {set(names) - found}')
        checksums = ''.join(f'{sha256(path)}  {path.name}\n' for path in sorted(stage.iterdir()))
        (stage / 'SHA256SUMS').write_text(checksums, encoding='utf-8')
        if destination.exists():
            shutil.rmtree(destination)
        shutil.move(str(stage), destination)
    return len(list(destination.iterdir()))


def release_notes(version, tag, repository, commit, run_id):
    def link(target, kind, label):
        name = filenames(version, target)[kind]
        url = f'https://github.com/{repository}/releases/download/{quote(tag, safe="")}/{quote(name)}'
        return f'[{label}]({url})'
    mac = 'aarch64-apple-darwin'
    win = 'aarch64-pc-windows-msvc'
    lines = [
        '## Download the app', '',
        '**Mac Apple silicon → Windows ARM VM:** ' + link(mac, 'dmg', 'Download the Mac app')
        + ' + ' + link(win, 'setup', 'Download the Windows VM app') + '.', '',
        'For a physical Intel/AMD Windows PC, choose **Windows · Intel / AMD** below.', '',
        '| Your device | GUI installer |', '| --- | --- |',
    ]
    for target, (system, architecture, label) in PLATFORMS.items():
        if system == 'macos':
            downloads = link(target, 'dmg', 'Download app (.dmg)')
        elif system == 'windows':
            downloads = link(target, 'setup', 'Download setup (.exe)')
        else:
            downloads = ' · '.join(link(target, kind, text) for kind, text in
                                    [('deb', 'Debian / Ubuntu'), ('rpm', 'Fedora / RPM'), ('arch', 'Arch Linux ARM' if architecture == 'arm64' else 'Arch Linux')])
        lines.append(f'| {label} | {downloads} |')
    lines += ['', '## Command-line downloads', '',
              'Choose these only for terminal use. Each archive contains the CLI, usage note and license.', '',
              '| Your device | CLI archive |', '| --- | --- |']
    for target, (system, _, label) in PLATFORMS.items():
        lines.append(f'| {label} | {link(target, "cli", "Download .zip" if system == "windows" else "Download .tar.gz")} |')
    checksum = f'https://github.com/{repository}/releases/download/{quote(tag, safe="")}/SHA256SUMS'
    lines += ['', f'[SHA-256 checksums for every download]({checksum})', '',
              'Use the same prerelease on both devices. Public builds share a public embedded pairing key; create and select your own private key on both devices for private access. Packages are unsigned; macOS builds are not notarized.', '',
              f'Version **{version}** · [Source commit `{commit[:8]}`](https://github.com/{repository}/commit/{commit}) · [Verified build](https://github.com/{repository}/actions/runs/{run_id})', '']
    return '\n'.join(lines)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path, help='Complete six-platform archive directory')
    parser.add_argument('destination', type=Path, help='Fresh directory for release assets')
    args = parser.parse_args()
    print(f'Prepared {prepare(args.directory, args.destination)} release assets')

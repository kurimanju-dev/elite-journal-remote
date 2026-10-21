"""Build a pacman package from the same Linux binaries used by Tauri's DEB/RPM."""
import json
from pathlib import Path
import shutil
import shlex
import subprocess
import tarfile
import tempfile

from build import archive_metadata, sha256


def stage_payload(root, build_dir, stage):
    files = {
        build_dir / 'elite-relay': 'usr/bin/elite-relay',
        build_dir / 'elite-journal-relay': 'usr/bin/elite-journal-relay',
        root / 'packaging/elite-relay.desktop': 'usr/share/applications/io.elitejournalrelay.desktop.desktop',
        root / 'desktop/icon.svg': 'usr/share/icons/hicolor/scalable/apps/io.elitejournalrelay.desktop.svg',
        root / 'LICENSE': 'usr/share/licenses/elite-journal-relay/LICENSE',
        root / 'README.md': 'usr/share/doc/elite-journal-relay/README.md',
    }
    for source, relative in files.items():
        destination = stage / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
        destination.chmod(0o755 if relative.startswith('usr/bin/') else 0o644)


def build_arch_package(root, build_dir, target, env):
    for executable in ['makepkg', 'pacman', 'bsdtar', 'zstd', 'fakeroot']:
        if not shutil.which(executable, path=env.get('PATH')):
            raise RuntimeError(f'Arch packaging requires {executable}; see README build requirements')
    architecture = target.split('-')[0]
    if architecture not in ['x86_64', 'aarch64']:
        raise ValueError(f'Unsupported pacman architecture: {architecture}')
    version = json.loads((root / 'desktop/src-tauri/tauri.conf.json').read_text(encoding='utf-8'))['version']
    destination = (build_dir / 'bundle/arch').resolve()
    destination.mkdir(parents=True, exist_ok=True)
    # makepkg requires an unprivileged user and an isolated writable directory.
    with tempfile.TemporaryDirectory(prefix='elite-relay-arch-') as temporary:
        work = Path(temporary)
        payload = work / 'payload'
        stage_payload(root, build_dir, payload)
        with tarfile.open(work / 'payload.tar', 'w') as archive:
            archive.add(payload / 'usr', arcname='usr', filter=archive_metadata)
        shutil.copyfile(root / 'packaging/arch/PKGBUILD', work / 'PKGBUILD')
        # Ubuntu runners have no initialized pacman database. Keep package
        # inspection and makepkg's build-info queries independent of the host.
        database = work / 'pacman-db'
        (database / 'local').mkdir(parents=True)
        pacman = work / 'pacman-wrapper'
        pacman.write_text('#!/bin/sh\nexec pacman --dbpath ' + shlex.quote(str(database)) + ' "$@"\n', encoding='utf-8')
        pacman.chmod(0o755)
        package_env = env.copy()
        package_env.update({
            'ELITE_RELAY_VERSION': version,
            'ELITE_RELAY_PAYLOAD_SHA256': sha256(work / 'payload.tar'),
            'CARCH': architecture,
            'PKGEXT': '.pkg.tar.zst',
            'PACKAGER': 'Elite Journal Relay <build@elitejournalrelay.invalid>',
            'PKGDEST': str(destination),
            'PACMAN': str(pacman),
        })
        subprocess.run(['makepkg', '--nodeps', '--nocheck', '--nosign', '--noconfirm'],
                       cwd=work, env=package_env, check=True)
        package = destination / f'elite-journal-relay-{version}-1-{architecture}.pkg.tar.zst'
        if not package.is_file():
            raise RuntimeError(f'makepkg did not produce {package.name}')
        # Ask the actual package manager to parse metadata before shipping it.
        subprocess.run([str(pacman), '-Qip', str(package)], env=env, check=True)
    return package

#!/usr/bin/env python3
"""Package the GUI and separate CLI; CI runs this for all six desktop targets."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
TARGETS = [
    {'target': 'aarch64-apple-darwin', 'runner': 'macos-15', 'bundles': 'app,dmg'},
    {'target': 'x86_64-apple-darwin', 'runner': 'macos-15-intel', 'bundles': 'app,dmg'},
    {'target': 'aarch64-pc-windows-msvc', 'runner': 'windows-11-arm', 'bundles': 'nsis'},
    {'target': 'x86_64-pc-windows-msvc', 'runner': 'windows-2022', 'bundles': 'nsis,msi'},
    {'target': 'aarch64-unknown-linux-gnu', 'runner': 'ubuntu-24.04-arm', 'bundles': 'deb,rpm'},
    {'target': 'x86_64-unknown-linux-gnu', 'runner': 'ubuntu-24.04', 'bundles': 'deb,rpm'},
]

def sha256(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def archive_metadata(member):
    # Do not ship the build machine's account names or IDs in Unix archives.
    member.uid = member.gid = 0
    member.uname = member.gname = ''
    return member

def copy_bundles(bundle, destination, target):
    if target.endswith('apple-darwin'):
        patterns = ['macos/*.app', 'dmg/*.dmg']
    elif 'windows' in target:
        patterns = ['nsis/*-setup.exe']
        if target.startswith('x86_64-'):
            patterns.append('msi/*.msi')
    else:
        patterns = ['deb/*.deb', 'rpm/*.rpm', 'arch/*.pkg.tar.zst']
    for pattern in patterns:
        matches = sorted(bundle.glob(pattern))
        if not matches:
            raise RuntimeError(f'Missing GUI package: {bundle / pattern}')
        for source in matches:
            output = destination / source.relative_to(bundle)
            output.parent.mkdir(parents=True, exist_ok=True)
            if source.is_dir():
                shutil.copytree(source, output, symlinks=True)
            else:
                shutil.copy2(source, output)

def package_outputs(build_dir, dist, target, version, cli_only=False, debug=False):
    """Stage fresh outputs, then archive with Unix permissions and symlinks intact."""
    dist.mkdir(parents=True, exist_ok=True)
    destination = dist / target
    with tempfile.TemporaryDirectory(prefix='.stage-', dir=dist) as temporary:
        stage = Path(temporary) / target
        stage.mkdir()
        suffix = '.exe' if 'windows' in target else ''
        shutil.copy2(build_dir / ('elite-journal-relay' + suffix), stage)
        if not cli_only:
            shutil.copy2(build_dir / ('elite-relay' + suffix), stage)
            bundle = build_dir / 'bundle'
            if not bundle.is_dir() or not any(bundle.iterdir()):
                raise RuntimeError(f'No GUI packages produced in {bundle}')
            copy_bundles(bundle, stage / 'bundle', target)
        files = [{'file': p.relative_to(stage).as_posix(), 'sha256': sha256(p)}
                 for p in sorted(stage.rglob('*')) if p.is_file() and not p.is_symlink()]
        (stage / 'manifest.json').write_text(json.dumps(files, indent=2) + '\n', encoding='utf-8')
        if destination.exists():
            shutil.rmtree(destination)
        shutil.move(str(stage), destination)
    packages = dist / 'packages'
    packages.mkdir(exist_ok=True)
    variant = ('-cli' if cli_only else '') + ('-debug' if debug else '')
    stem = f'elite-relay-{version}-{target}{variant}'
    archive = packages / (stem + ('.zip' if 'windows' in target else '.tar.gz'))
    # Write beside the destination and replace only after a successful archive.
    temporary = archive.with_name(archive.name + '.tmp')
    try:
        if 'windows' in target:
            with zipfile.ZipFile(temporary, 'w', compression=zipfile.ZIP_DEFLATED) as output:
                for path in sorted(destination.rglob('*')):
                    if path.is_file():
                        output.write(path, (Path(target) / path.relative_to(destination)).as_posix())
        else:
            with tarfile.open(temporary, 'w:gz') as output:
                output.add(destination, arcname=target, filter=archive_metadata)
        temporary.replace(archive)
    finally:
        temporary.unlink(missing_ok=True)
    archive.with_name(archive.name + '.sha256').write_text(
        f'{sha256(archive)}  {archive.name}\n', encoding='utf-8')
    return archive

def run(args, **kwargs):
    subprocess.run([str(x) for x in args], cwd=ROOT, check=True, **kwargs)

def compiler_environment(env, target):
    result = env.copy()
    encoded = result.get('CARGO_ENCODED_RUSTFLAGS')
    flags = encoded.split('\x1f') if encoded else shlex.split(result.get('RUSTFLAGS', ''))
    if target.endswith('-windows-msvc'):
        flags += ['-C', 'target-feature=+crt-static']
    for path, replacement in [(Path.home(), 'build-home'), (ROOT, 'elite-journal-relay')]:
        flags.append(f'--remap-path-prefix={path}={replacement}')
    result['CARGO_ENCODED_RUSTFLAGS'] = '\x1f'.join(flags)
    return result

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', help='Rust target triple; default is the native host')
    parser.add_argument('--cli-only', action='store_true', help='Build just the CLI (cross builds need a linker)')
    parser.add_argument('--debug', action='store_true', help='Use the debug profile')
    parser.add_argument('--matrix', action='store_true', help='Print the CI target matrix as JSON')
    args = parser.parse_args()
    if args.matrix:
        print(json.dumps({'include': TARGETS}))
        return
    host = next(line.split(': ', 1)[1] for line in subprocess.check_output(['rustc', '-vV'], text=True).splitlines() if line.startswith('host:'))
    target = args.target or host
    specification = next((entry for entry in TARGETS if entry['target'] == target), None)
    if not args.cli_only and specification is None:
        parser.error('Unsupported desktop target; use --matrix to list supported targets')
    if not args.cli_only and target.split('-')[2] != host.split('-')[2]:
        parser.error('GUI packages require the target OS SDK; use the all-platform Actions workflow or --cli-only')
    env = compiler_environment(os.environ, target)
    run(['rustup', 'target', 'add', target], env=env)
    command = ['cargo', 'build', '--locked', '-p', 'elite-journal-relay', '--bin', 'elite-journal-relay', '--target', target]
    if not args.debug: command.append('--release')
    run(command, env=env)
    profile = 'debug' if args.debug else 'release'
    build_dir = ROOT / 'target' / target / profile
    if not args.cli_only:
        npm = 'npm.cmd' if sys.platform == 'win32' else 'npm'
        if not (ROOT / 'node_modules').is_dir(): run([npm, 'ci'], env=env)
        # Remove previous installer outputs so stale packages cannot enter the archive.
        if (build_dir / 'bundle').exists():
            shutil.rmtree(build_dir / 'bundle')
        command = [npm, 'run', 'build', '--', '--target', target,
                   '--bundles', specification['bundles']]
        if args.debug: command.append('--debug')
        command += ['--', '--locked']
        run(command, env=env)
        if '-linux-' in target:
            from arch_package import build_arch_package
            build_arch_package(ROOT, build_dir, target, env)
    version = json.loads((ROOT / 'desktop/src-tauri/tauri.conf.json').read_text(encoding='utf-8'))['version']
    archive = package_outputs(build_dir, ROOT / 'dist', target, version, args.cli_only, args.debug)
    print('Built', archive)

if __name__ == '__main__': main()

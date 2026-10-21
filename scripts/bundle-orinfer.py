#!/usr/bin/env python3
"""Bundle the pinned Orinfer release (local directory or verified GitHub assets)."""
import argparse
import hashlib
import json
import os
import platform
import subprocess
from pathlib import Path
import shutil
import tarfile
import urllib.request

LOCK = Path(__file__).with_name('orinfer-release.json')


def bundle(destination, release_dir=None):
    lock = json.loads(LOCK.read_text())
    destination.mkdir(parents=True, exist_ok=True)
    archives = destination / 'releases'
    archives.mkdir()
    for name, digest in lock['assets'].items():
        target = archives / name
        if release_dir:
            shutil.copyfile(release_dir / name, target)
        else:
            with urllib.request.urlopen(f"https://github.com/iMMIQ/orinfer/releases/download/v{lock['version']}/{name}", timeout=120) as response, target.open('wb') as output:
                shutil.copyfileobj(response, output)
        if hashlib.sha256(target.read_bytes()).hexdigest() != digest:
            raise ValueError(f'Orinfer release checksum mismatch: {name}')
    with tarfile.open(archives / lock['engine']) as archive:
        prefix = 'orinfer-v0.2.0-linux-aarch64/'
        for item in archive:
            if item.name.rstrip('/') == prefix.rstrip('/'):
                continue
            if not item.name.startswith(prefix):
                raise ValueError(f'unexpected engine member: {item.name}')
            relative = Path(item.name[len(prefix):])
            if relative.is_absolute() or '..' in relative.parts or not (item.isfile() or item.isdir()):
                raise ValueError(f'unsafe engine member: {item.name}')
            target = destination / relative
            if item.isdir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(item) as source, target.open('wb') as output:
                    shutil.copyfileobj(source, output)
                target.chmod(item.mode & 0o777)
    shutil.copyfile(LOCK, destination / 'release-lock.json')
    # Install at build time, including on x86 release builders via user-mode QEMU.
    installer = [str((destination / 'bin/orinfer').resolve())]
    if platform.machine() not in ('aarch64', 'arm64'):
        installer = ['qemu-aarch64', '-L', '/usr/aarch64-linux-gnu'] + installer
    environment = dict(os.environ, ORINFER_EXECUTION_CACHE=str((destination / 'packages').resolve()))
    for archive in sorted(archives.glob('orinfer-exec-*.tar.xz')):
        subprocess.run(installer + ['package', 'install', str(archive.resolve())], env=environment, check=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('destination', type=Path)
    parser.add_argument('--release-dir', type=Path, default=os.environ.get('ORINFER_RELEASE_DIR'))
    args = parser.parse_args()
    bundle(args.destination, Path(args.release_dir) if args.release_dir else None)

#!/usr/bin/env python3
"""Validate staged release inputs and final Lazycat tar/zip packages."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import struct
import subprocess
import tarfile
import tempfile
import zipfile

import yaml


FONTS = ["NotoSansCJK-Regular.ttc", "DejaVuSans.ttf"] + [
    f"{name}-Regular.ttf" for name in
    ("NotoSans", "NotoSansArabic", "NotoSansHebrew", "NotoSansDevanagari", "NotoSansThai")
]
TARGETS = {"web": (62, "x86_64-unknown-linux-gnu"),
           "aipod": (183, "aarch64-unknown-linux-gnu")}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def elf(data, machine):
    require(data[:6] == b"\x7fELF\x02\x01", "expected a 64-bit little-endian ELF")
    require(struct.unpack_from("<H", data, 18)[0] == machine, "wrong ELF target architecture")


def elf_info(data):
    with tempfile.NamedTemporaryFile() as binary:
        binary.write(data)
        binary.flush()
        return subprocess.check_output(
            ["readelf", "-d", "-V", binary.name], text=True, env={"LC_ALL": "C"}
        )


def validate_payload(kind, files):
    binary = "ferryman-web" if kind == "web" else "agxorin/ferryman-agent"
    data, mode = files[binary]
    require(mode & 0o111, f"{binary} is not executable")
    elf(data, TARGETS[kind][0])
    info = elf_info(data)
    needed = set(re.findall(r"Shared library: \[(.*?)\]", info))
    versions = set(re.findall(r"GLIBC_[0-9.]+", info))
    if kind == "web":
        require(needed <= {p.removeprefix("web-libs/") for p in files if p.startswith("web-libs/")},
                f"unbundled shared library: {needed}")
        for name in FONTS:
            require(bool(files[f"fonts/{name}"][0]), f"empty font: {name}")
        for name in ("fonts-noto-cjk", "fonts-noto-core", "fonts-dejavu-core"):
            require(bool(files[f"fonts/{name}.copyright"][0]), f"missing font license: {name}")
        require(b"BEGIN CERTIFICATE" in files["ca-certificates.crt"][0], "missing CA bundle")
        require(b"dns" in files["nsswitch.conf"][0], "missing DNS configuration")
        require(b"/assets/" in files["assets/index.html"][0], "missing built frontend")
        require(any(p.startswith("assets/assets/") and p.endswith(".js") for p in files),
                "missing frontend JavaScript")
        for path, (library, _) in files.items():
            if path.startswith("web-libs/"):
                elf(library, 62)
        available = set(re.findall(r"GLIBC_[0-9.]+", elf_info(files["web-libs/libc.so.6"][0])))
        require(versions <= available, f"sysroot cannot provide {versions - available}")
    else:
        require(needed <= {"libgcc_s.so.1", "libm.so.6", "libc.so.6", "ld-linux-aarch64.so.1"},
                f"unexpected Agent dependency: {needed}")
        require(all(tuple(map(int, v[6:].split('.'))) <= (2, 35) for v in versions),
                "Agent requires glibc newer than the pinned vLLM image's 2.35; use Ubuntu 22.04 sysroot")
        compose = yaml.safe_load(files["agxorin/docker-compose.yml"][0])
        service = compose["services"]["ferryman-agent"]
        require(re.search(r"@sha256:[0-9a-f]{64}$", service["image"]), "vLLM image must be digest pinned")
        require("/healthz" in str(service["healthcheck"]["test"]), "missing Agent healthcheck")
        require("./ferryman-agent:/usr/local/bin/ferryman-agent:ro" in service["volumes"],
                "Agent executable must be mounted read-only beside compose")
        require(compose["networks"]["default"]["name"] == "traefik-shared-network", "wrong AI Pod network")
        require(any("Host(`ferryman-agent-ai`)" in label for label in service["labels"]), "wrong AI host")
        config = yaml.safe_load(files["config/aipod.yml"][0])
        require("ferryman-agent-ai" in config["depends_on_hosts"], "missing host dependency")
    return {"target": TARGETS[kind][1], "needed": sorted(needed), "glibc": sorted(versions)}


def manifest_path(kind):
    return "artifacts.json" if kind == "web" else "agxorin/artifacts.json"


def verify_manifest(kind, files):
    metadata = json.loads(files[manifest_path(kind)][0])
    actual = {p: sha(data) for p, (data, _) in files.items() if p != manifest_path(kind)}
    require(metadata["sha256"] == actual, "artifact files differ from their checksum manifest")
    require(metadata["binary"] == validate_payload(kind, files), "binary metadata mismatch")


def directory_files(root):
    return {p.relative_to(root).as_posix(): (p.read_bytes(), p.stat().st_mode)
            for p in root.rglob("*") if p.is_file()}


def archive_files(data):
    result = {}
    stream = io.BytesIO(data)
    if zipfile.is_zipfile(stream):
        with zipfile.ZipFile(stream) as archive:
            entries = [(item.filename, archive.read(item), item.external_attr >> 16)
                       for item in archive.infolist() if not item.is_dir()]
    else:
        stream.seek(0)
        with tarfile.open(fileobj=stream, mode="r:*") as archive:
            entries = [(item.name, archive.extractfile(item).read(), item.mode)
                       for item in archive if item.isfile()]
    for name, content, mode in entries:
        path = PurePosixPath(name)
        require(not path.is_absolute() and ".." not in path.parts, f"invalid archive path {name}")
        require(path.as_posix() not in result, f"duplicate archive path {name}")
        result[path.as_posix()] = (content, mode)
    return result


def check_lpk(kind, path):
    files = archive_files(path.read_bytes())
    package = yaml.safe_load(files["package.yml"][0])
    expected = "cloud.lazycat.aipod.ferryman" if kind == "aipod" else "cloud.lazycat.app.ferryman"
    require(package["package"] == expected, "wrong release package identity")
    if kind == "aipod":
        require(tuple(map(int, str(package["min_os_version"]).split('.'))) >= (1, 5, 2), "resource exports require LZCOS >= 1.5.2")
        require("manifest.yml" not in files and "images.lock" not in files, "expected resource-only LPK")
        payload = {p.removeprefix("exports/aipod/"): v for p, v in files.items()
                   if p.startswith("exports/aipod/")}
    else:
        manifest = yaml.safe_load(files["manifest.yml"][0])
        require(manifest["services"]["web"]["image"].startswith("embed:ferryman-web"), "wrong embedded image alias")
        lock = yaml.safe_load(files["images.lock"][0])["images"]["ferryman-web"]
        require(not lock.get("upstream"), "scratch Web image must be fully embedded")
        index = json.loads(files["images/index.json"][0])
        descriptor = next(item for item in index["manifests"]
                          if item.get("annotations", {}).get("org.opencontainers.image.ref.name") == "ferryman-web")

        def oci_json(reference):
            digest = reference["digest"].removeprefix("sha256:")
            blob = files[f"images/blobs/sha256/{digest}"][0]
            require(sha(blob) == digest, "corrupted OCI metadata")
            return json.loads(blob)

        image_manifest = oci_json(descriptor)
        config = oci_json(image_manifest["config"])
        require(config["architecture"] == "amd64" and config["os"] == "linux", "wrong Web image platform")
        require(config["config"]["Entrypoint"] == ["/usr/local/bin/ferryman-web"], "wrong Web entrypoint")
        require([layer["digest"] for layer in image_manifest["layers"]] ==
                [layer["digest"] for layer in lock["layers"]], "OCI manifest and images.lock disagree")
        rootfs = {}
        require(len(config["rootfs"]["diff_ids"]) == len(lock["layers"]), "wrong OCI layer count")
        for index, layer in enumerate(lock["layers"]):
            require(layer["source"] == "embed", "missing embedded layer")
            digest = layer["digest"].removeprefix("sha256:")
            blob = files[f"images/blobs/sha256/{digest}"][0]
            require(sha(blob) == digest, "corrupted OCI layer")
            raw = gzip.decompress(blob) if blob.startswith(b"\x1f\x8b") else blob
            require("sha256:" + sha(raw) == config["rootfs"]["diff_ids"][index], "OCI layer content does not match its diff ID")
            contents = archive_files(raw)
            require(not any(PurePosixPath(p).name.startswith('.wh.') for p in contents), "unexpected whiteout in scratch image")
            rootfs.update(contents)
        metadata = json.loads(rootfs["app/artifacts.json"][0])
        payload = {"artifacts.json": rootfs["app/artifacts.json"]}
        for name in metadata["sha256"]:
            if name == "ferryman-web":
                image_path = "usr/local/bin/ferryman-web"
            elif name.startswith("web-libs/"):
                image_path = "lib/x86_64-linux-gnu/" + name.removeprefix("web-libs/")
            elif name.startswith("fonts/"):
                image_path = "app/" + name
            elif name.startswith("assets/"):
                image_path = "app/web/" + name.removeprefix("assets/")
            elif name == "nsswitch.conf":
                image_path = "etc/nsswitch.conf"
            elif name == "ca-certificates.crt":
                image_path = "etc/ssl/certs/ca-certificates.crt"
            else:
                raise ValueError(f"unexpected artifact {name}")
            payload[name] = rootfs[image_path]
        require(rootfs["lib64/ld-linux-x86-64.so.2"][0] == payload["web-libs/ld-linux-x86-64.so.2"][0], "wrong ELF loader")
    verify_manifest(kind, payload)
    print(f"Validated {path}: {len(payload)} payload files, {path.stat().st_size} bytes")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["write", "check", "lpk"])
    parser.add_argument("kind", choices=TARGETS)
    parser.add_argument("path", type=Path)
    args = parser.parse_args()
    if args.action == "lpk":
        check_lpk(args.kind, args.path)
        return
    files = directory_files(args.path)
    if args.action == "write":
        metadata = {"binary": validate_payload(args.kind, files),
                    "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
                    "cargo_lock_sha256": sha(Path("Cargo.lock").read_bytes()),
                    "sha256": {p: sha(data) for p, (data, _) in sorted(files.items())
                               if p != manifest_path(args.kind)}}
        (args.path / manifest_path(args.kind)).write_text(json.dumps(metadata, indent=2) + "\n")
        # Fresh staging must not invalidate the CLI's context digest merely due
        # to copy/build timestamps. SOURCE_DATE_EPOCH also supports source tarballs.
        epoch = os.environ.get("SOURCE_DATE_EPOCH")
        if epoch is None:
            epoch = subprocess.check_output(["git", "log", "-1", "--format=%ct"], text=True).strip()
        timestamp = int(epoch)
        for entry in [args.path, *args.path.rglob("*")]:
            os.utime(entry, (timestamp, timestamp))
    else:
        verify_manifest(args.kind, files)
    print(f"Validated {args.kind} artifacts: {args.path}")


if __name__ == "__main__":
    main()

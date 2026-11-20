#!/usr/bin/env python3
"""Release key tools and the signed update manifest.

CI builds the artifacts (.github/workflows/release.yml) and runs `sign`.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tomllib

KEY = Path(".dev/release-key.pem")
PUBLIC = Path(".dev/release-public.pem")
ED25519_SPKI = bytes.fromhex("302a300506032b6570032100")
# rusttalk-<version>-<os>-<arch>.<ext>; the updater looks up "<os>-<arch>".
ARTIFACT = re.compile(r"rusttalk-(?P<version>[0-9.]+)-(?P<platform>(linux|windows|macos)-(x86_64|aarch64))\.(AppImage|exe|dmg)")


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def version():
    with open("Cargo.toml", "rb") as cargo:
        value = tomllib.load(cargo)["workspace"]["package"]["version"]
    if (not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value)
            or any(int(component) > 65535 for component in value.split("."))):
        raise SystemExit("Release version must be MAJOR.MINOR.PATCH, with components at most 65535.")
    return value


def key_path():
    if not shutil.which("openssl"):
        raise SystemExit("openssl is missing. Install OpenSSL 3.")
    path = os.environ.get("RUSTTALK_RELEASE_KEY")
    if not path or not Path(path).is_file():
        raise SystemExit("Set RUSTTALK_RELEASE_KEY to the Ed25519 private key file.")
    return Path(path).resolve()


def public_key(key):
    der = run("openssl", "pkey", "-in", key, "-pubout", "-outform", "DER",
              stdout=subprocess.PIPE).stdout
    if not der.startswith(ED25519_SPKI) or len(der) != len(ED25519_SPKI) + 32:
        raise SystemExit("The release key must be Ed25519.")
    return der[len(ED25519_SPKI):].hex()


def generate_key():
    KEY.parent.mkdir(exist_ok=True)
    with os.fdopen(
        os.open(KEY, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), "wb"
    ) as key_file:
        run("openssl", "genpkey", "-algorithm", "Ed25519", stdout=key_file)
    run("openssl", "pkey", "-in", KEY, "-pubout", "-out", PUBLIC)
    print(f"Local test key: {KEY}\nPublic key: {PUBLIC}")


def digest(path):
    with path.open("rb") as artifact:
        return {"file": path.name, "size": path.stat().st_size,
                "sha256": hashlib.file_digest(artifact, "sha256").hexdigest()}


def sign(key, directory, ver):
    expected = os.environ.get("RUSTTALK_RELEASE_PUBLIC_KEY")
    if expected and expected != public_key(key):
        raise SystemExit("RUSTTALK_RELEASE_KEY does not match RUSTTALK_RELEASE_PUBLIC_KEY.")
    platforms = {}
    for path in sorted(directory.iterdir()):
        match = ARTIFACT.fullmatch(path.name)
        if match:
            if match["version"] != ver:
                raise SystemExit(f"{path.name} is not version {ver}.")
            platforms[match["platform"]] = digest(path)
    if not platforms:
        raise SystemExit(f"No release artifacts in {directory}.")
    document = {"version": ver, "platforms": platforms}
    images = directory / "images.json"
    if images.is_file():
        # Registry references pinned by digest, e.g. ghcr.io/owner/rusttalk-api@sha256:...
        document["images"] = json.loads(images.read_text(encoding="utf-8"))
        images.unlink()
    manifest = directory / "latest.json"
    manifest.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    run("openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", manifest,
        "-out", directory / "latest.json.sig")
    print(manifest.read_text(encoding="utf-8"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("key", "public-key", "version", "sign"))
    parser.add_argument("directory", nargs="?", default="dist", type=Path)
    args = parser.parse_args()
    if args.command == "key":
        generate_key()
    elif args.command == "public-key":
        print(public_key(key_path()))
    elif args.command == "version":
        print(version())
    else:
        sign(key_path(), args.directory, version())


if __name__ == "__main__":
    main()

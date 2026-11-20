#!/usr/bin/env python3
"""Build and sign releases from the workspace root."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import tomllib

DIST = Path("dist")
KEY = Path(".dev/release-key.pem")
PUBLIC = Path(".dev/release-public.pem")
ED25519_SPKI = bytes.fromhex("302a300506032b6570032100")


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def version():
    with open("Cargo.toml", "rb") as cargo:
        value = tomllib.load(cargo)["workspace"]["package"]["version"]
    if (not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value)
            or any(int(component) > 65535 for component in value.split("."))):
        raise SystemExit("Release version must be MAJOR.MINOR.PATCH, with components at most 65535.")
    return value


def require(tool, install):
    if not shutil.which(tool):
        raise SystemExit(f"{tool} is missing. Install {install} and put {tool} on PATH.")


def key_path():
    require("openssl", "OpenSSL 3")
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
    require("openssl", "OpenSSL 3")
    KEY.parent.mkdir(exist_ok=True)
    with os.fdopen(
        os.open(KEY, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), "wb"
    ) as key_file:
        run("openssl", "genpkey", "-algorithm", "Ed25519", stdout=key_file)
    run("openssl", "pkey", "-in", KEY, "-pubout", "-out", PUBLIC)
    print(f"Local test key: {KEY}\nPublic key: {PUBLIC}")


def names(ver):
    return {
        "linux-x86_64": f"rusttalk-{ver}-linux-x86_64.AppImage",
        "windows-x86_64": f"rusttalk-{ver}-windows-x86_64.exe",
        "api": f"rusttalk-api-{ver}.tar",
        "index": f"rusttalk-index-{ver}.tar",
    }


def appimage(ver, directory, env):
    run("cargo", "build", "--release", "--locked", "-p", "rusttalk-client",
        "-p", "rusttalk-pipewire", env=env)
    # Both binaries must use the same directory for sibling helper discovery.
    target = Path(env.get("CARGO_TARGET_DIR", "target")) / "release"
    appdir = directory / "RustTalk.AppDir"
    binaries = appdir / "usr" / "bin"
    binaries.mkdir(parents=True)
    for binary in ("rusttalk-client", "rusttalk-pipewire"):
        shutil.copy2(target / binary, binaries)
    (appdir / "AppRun").symlink_to("usr/bin/rusttalk-client")
    shutil.copy2("deploy/release/rusttalk.desktop", appdir)
    shutil.copy2("deploy/release/rusttalk.svg", appdir)
    runtime = env.get("RUSTTALK_APPIMAGE_RUNTIME")
    runtime_arg = ("--runtime-file", runtime) if runtime else ()
    run("appimagetool", *runtime_arg, appdir, directory / names(ver)["linux-x86_64"],
        env={**env, "ARCH": "x86_64"})


def digest(path):
    with path.open("rb") as artifact:
        return {"file": path.name, "size": path.stat().st_size,
                "sha256": hashlib.file_digest(artifact, "sha256").hexdigest()}


def sign(key, directory, ver, platforms, images=None):
    document = {"version": ver, "platforms": {
        name: digest(directory / file) for name, file in platforms.items()
    }}
    if images:
        document["images"] = {name: digest(directory / file) for name, file in images.items()}
    manifest = directory / "latest.json"
    manifest.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    run("openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", manifest,
        "-out", directory / "latest.json.sig")


def release(key, ver):
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise SystemExit("Run make release on Linux x86_64.")
    require("appimagetool", "the x86_64 AppImage from https://github.com/AppImage/appimagetool/releases, named appimagetool")
    require("docker", "Docker Engine with BuildKit")
    files = names(ver)
    windows = DIST / files["windows-x86_64"]
    if not windows.is_file():
        raise SystemExit(f"Build {windows.name} on Windows using deploy/release/windows.ps1 and copy it to dist/.")
    if (DIST / ver).exists():
        raise SystemExit(f"dist/{ver} already exists. Use a fresh workspace version for a new release.")
    env = dict(os.environ)
    env.pop("RUSTTALK_LOCAL_UPDATE_TEST", None)
    env.pop("RUSTTALK_RELEASE_KEY", None)
    env["RUSTTALK_RELEASE_PUBLIC_KEY"] = public_key(key)
    with tempfile.TemporaryDirectory(prefix=".release-", dir=DIST) as temporary:
        staging = Path(temporary)
        # Check that the key can actually sign before starting the release builds.
        run("openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", "Cargo.toml",
            stdout=subprocess.DEVNULL)
        appimage(ver, staging, env)
        shutil.rmtree(staging / "RustTalk.AppDir")
        shutil.copy2(windows, staging)
        for service in ("api", "index"):
            tag = f"rusttalk-{service}:{ver}"
            run("docker", "build", "--build-arg", f"SERVICE={service}",
                "--target", service, "-f", "services/api/Dockerfile", "-t", tag, ".", env=env)
            run("docker", "save", "-o", staging / files[service], tag, env=env)
        sign(key, staging, ver, {name: files[name] for name in ("linux-x86_64", "windows-x86_64")},
             {name: files[name] for name in ("api", "index")})
        staging.rename(DIST / ver)
    print(f"Signed release: {DIST / ver}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("key", "public-key", "release"))
    args = parser.parse_args()
    if args.command == "key":
        generate_key()
    elif args.command == "public-key":
        print(public_key(key_path()))
    else:
        release(key_path(), version())


if __name__ == "__main__":
    main()

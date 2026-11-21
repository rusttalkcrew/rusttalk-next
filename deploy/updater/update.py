"""Apply signed RustTalk patch releases to this Compose project."""
import base64
from contextlib import closing
from datetime import datetime, timezone
import fcntl
import hashlib
import hmac
import io
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request

RELEASE = "https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/"
REGISTRY = "ghcr.io/rusttalkcrew/rusttalk-"
DEPLOYMENT = Path("/deployment")
PINS = DEPLOYMENT / "auto-update/images.yml"
BACKUPS = Path("/backups")
JOURNAL = BACKUPS / "transaction.json"
STATUS = Path("/update-status/status.json")
DATA = {"api": Path("/api-data"), "index": Path("/index-data")}
DATABASE = {"api": "rusttalk.sqlite3", "index": "index.sqlite3"}


def version(value):
    if not isinstance(value, str) or not re.fullmatch(r"(0|[1-9][0-9]{0,4})(\.(0|[1-9][0-9]{0,4})){2}", value):
        raise ValueError("invalid release version")
    parts = tuple(map(int, value.split(".")))
    if max(parts) > 65535:
        raise ValueError("invalid release version")
    return parts


def newer_patch(candidate, current, series):
    candidate_parts = version(candidate)
    current_parts = version(current)
    return candidate_parts[:2] == current_parts[:2] == version(series + ".0")[:2] and candidate_parts > current_parts


def verify(manifest, signature, public_key):
    if not re.fullmatch(r"[0-9a-fA-F]{64}", public_key) or len(signature) != 64:
        raise ValueError("invalid Ed25519 key or signature length")
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory)
        (path / "key.der").write_bytes(bytes.fromhex("302a300506032b6570032100" + public_key))
        (path / "manifest").write_bytes(manifest)
        (path / "sig").write_bytes(signature)
        result = subprocess.run(
            ["openssl", "pkeyutl", "-verify", "-rawin", "-pubin", "-keyform", "DER",
             "-inkey", path / "key.der", "-in", path / "manifest", "-sigfile", path / "sig"],
            capture_output=True, timeout=10,
        )
        if result.returncode:
            raise ValueError("release manifest signature verification failed")
    return json.loads(manifest)


def due(quiet, now, since, hold_seconds, hour):
    return quiet or (now.timestamp() >= since + hold_seconds and now.hour == hour)


class HttpsRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if not newurl.startswith("https://"):
            raise ValueError("release assets must use HTTPS")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def fetch(url, headers=None, data=None):
    opener = urllib.request.build_opener(HttpsRedirect())
    with opener.open(urllib.request.Request(url, data, headers or {}), timeout=5) as response:
        body = response.read(1024 * 1024 + 1)
    if len(body) > 1024 * 1024:
        raise ValueError("update response too large")
    return body


def livekit_quiet(secret):
    def encode(value):
        return base64.urlsafe_b64encode(value).rstrip(b"=")

    unsigned = b".".join((
        encode(b'{"alg":"HS256","typ":"JWT"}'),
        encode(json.dumps({"iss": "rusttalk", "sub": "rusttalk-updater", "exp": int(time.time()) + 30,
                           "video": {"roomList": True}}).encode()),
    ))
    token = b".".join((unsigned, encode(hmac.digest(secret.encode(), unsigned, hashlib.sha256))))
    rooms = json.loads(fetch(
        "http://livekit:7880/twirp/livekit.RoomService/ListRooms",
        {"Authorization": f"Bearer {token.decode()}", "Content-Type": "application/json"},
        b"{}",
    )).get("rooms", [])
    if not isinstance(rooms, list):
        raise ValueError("invalid LiveKit rooms response")
    return not rooms


def read(path):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return {}


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write(path, value, mode=0o600):
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as stream:
        temp = Path(stream.name)
        try:
            os.fchmod(stream.fileno(), mode)
            json.dump(value, stream)
            stream.flush()
            os.fsync(stream.fileno())
            temp.replace(path)
            sync_dir(path.parent)
        finally:
            temp.unlink(missing_ok=True)


def docker(*args, capture=False, timeout=180, cwd=None):
    return subprocess.run(["docker", *args], check=True, text=True,
                          stdout=subprocess.PIPE if capture else None, timeout=timeout, cwd=cwd).stdout


def compose(*args, **kwargs):
    return docker("compose", "--project-directory", str(DEPLOYMENT), "--profile", "index",
                  *args, cwd=DEPLOYMENT, **kwargs)


def running(service):
    return compose("ps", "--status", "running", "-q", service, capture=True).strip()


def installed(container, service):
    image = docker("inspect", "--format", "{{.Image}}", container, capture=True).strip()
    digests = json.loads(docker("image", "inspect", "--format", "{{json .RepoDigests}}",
                                image, capture=True))
    return next((ref for ref in (digests or []) if ref.startswith(REGISTRY + service + "@")), image)


def pins():
    return {service: config["image"] for service, config in read(PINS)["services"].items()}


def set_pins(images):
    write(PINS, {"services": {name: {"image": ref} for name, ref in images.items()}}, 0o644)


def backup(archive, version, images):
    temp = archive.with_suffix(".partial")
    try:
        with tarfile.open(temp, "w:gz", compresslevel=1) as output:
            manifest = json.dumps({"version": version, "images": images}).encode()
            metadata = tarfile.TarInfo("release.json")
            metadata.size = len(manifest)
            metadata.mode = 0o600
            output.addfile(metadata, io.BytesIO(manifest))
            for service in images:
                path = DATA[service]
                # Writers have stopped, so the checkpoint and filesystem archive agree.
                with closing(sqlite3.connect((path / DATABASE[service]).as_uri() + "?mode=rw",
                                             uri=True)) as db:
                    if db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()[0]:
                        raise RuntimeError(f"{service} WAL checkpoint was busy")
                output.add(path, arcname=service)
        with temp.open("rb") as stream:
            os.fsync(stream.fileno())
        temp.replace(archive)
        sync_dir(archive.parent)
    finally:
        temp.unlink(missing_ok=True)


def remove(path):
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink(missing_ok=True)


def restore(archive, services):
    with tarfile.open(archive, "r:gz") as source:
        for service in services:
            root = DATA[service]
            members = [member for member in source.getmembers()
                       if member.name == service or member.name.startswith(service + "/")]
            if not any(member.name == f"{service}/{DATABASE[service]}" for member in members):
                raise RuntimeError(f"{service} database missing from backup")
            staging = root / ".updater-restore"
            remove(staging)
            staging.mkdir()
            try:
                # Extract before deleting live files and preserve numeric ownership of the key and DB.
                source.extractall(staging, members, numeric_owner=True, filter="fully_trusted")
                for child in root.iterdir():
                    if child != staging:
                        remove(child)
                for child in (staging / service).iterdir():
                    child.replace(root / child.name)
                os.sync()
            finally:
                remove(staging)


def verify_volumes(containers):
    def mounts(container):
        return json.loads(docker("inspect", "--format", "{{json .Mounts}}", container, capture=True))

    own = mounts(os.environ["HOSTNAME"])
    for service, container in containers.items():
        ours = next((mount for mount in own if mount["Destination"] == str(DATA[service])), {})
        theirs = next((mount for mount in mounts(container) if mount["Destination"] == "/data"), {})
        if ours.get("Type") != "volume" or ours.get("Name") != theirs.get("Name"):
            raise RuntimeError(f"{service} must use the Compose data volume mounted by the updater")


def healthy(services, expected):
    deadline = time.monotonic() + 90
    while True:
        try:
            for service in services:
                if fetch(f"http://{service}:8080/v1/health").strip() != b"ok":
                    raise ValueError(f"{service} health response")
            info = json.loads(fetch("http://api:8080/v1/server-info"))
            if info["version"] == expected:
                return
        except (OSError, ValueError, KeyError) as error:
            last = str(error)
        else:
            last = f"API version is {info['version']!r}"
        if time.monotonic() >= deadline:
            raise RuntimeError(f"expected healthy API {expected}: {last}")
        time.sleep(2)


def finish(transaction, keep):
    old = transaction["old"]
    target = transaction["target"]
    archive = BACKUPS / transaction["archive"]
    phase = transaction["phase"]
    if phase == "installing":
        print(f"Rolling back failed update to {target}; restoring {archive.name}")
        compose("stop", *old)
        restore(archive, old)
        set_pins(transaction["pins"] | old)
        # Once the restored version starts accepting writes, never restore this snapshot again.
        phase = transaction["phase"] = "restored"
        write(JOURNAL, transaction)
    if phase == "restored":
        compose("up", "-d", "--no-deps", "--no-build", "--pull", "never", *old)
        healthy(old, transaction["version"])
    elif phase == "stopping":
        print(f"Restarting previous services after interrupted backup for {target}")
        docker("start", *transaction["containers"])
        healthy(old, transaction["version"])
    outcome = "updated" if phase == "committed" else "failed"
    write(STATUS, {"last": {"version": target, "outcome": outcome, "at": int(time.time())}}, 0o644)
    JOURNAL.unlink()
    sync_dir(BACKUPS)
    print(f"Update {target}: {outcome}", flush=True)
    for stale in sorted(BACKUPS.glob("rusttalk-*.tar.gz"), reverse=True)[keep:]:
        stale.unlink()


def update(target, current, images, containers, keep):
    if any(running(service) != containers.get(service, "") for service in DATA):
        raise RuntimeError("running services changed during update check")
    verify_volumes(containers)
    old = {service: installed(container, service) for service, container in containers.items()}
    archive = BACKUPS / ("rusttalk-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ") + ".tar.gz")
    previous_pins = pins()
    transaction = {"target": target, "version": current, "old": old, "pins": previous_pins,
                   "archive": archive.name, "phase": "stopping", "containers": list(containers.values())}
    write(JOURNAL, transaction)
    try:
        compose("stop", *images)
        backup(archive, current, old)
        transaction["phase"] = "installing"
        write(JOURNAL, transaction)
        set_pins(previous_pins | images)
        compose("up", "-d", "--no-deps", "--no-build", "--pull", "never", "--force-recreate", *images)
        healthy(images, target)
        transaction["phase"] = "committed"
        write(JOURNAL, transaction)
    except Exception as error:
        print(f"Update {target} failed: {error}", flush=True)
    finish(read(JOURNAL), keep)


def check(series, key, secret, hour, hold_seconds, keep):
    transaction = read(JOURNAL)
    if transaction:
        finish(transaction, keep)
        return

    containers = {service: running(service) for service in DATA}
    if not containers["api"]:
        return
    containers = {service: container for service, container in containers.items() if container}
    current = json.loads(fetch("http://api:8080/v1/server-info"))["version"]
    current_pins = pins()
    unpinned = {service: installed(container, service) for service, container in containers.items()
                if service not in current_pins}
    if unpinned:
        set_pins(current_pins | unpinned)
    manifest = verify(fetch(RELEASE + "latest.json"), fetch(RELEASE + "latest.json.sig"), key)
    target = manifest["version"]
    state = read(STATUS)
    if not newer_patch(target, current, series) or state.get("last", {}).get("version") == target:
        if state.pop("pending", None):
            write(STATUS, state, 0o644)
        return

    images = {service: manifest["images"][service] for service in containers}
    for service, ref in images.items():
        if not re.fullmatch(re.escape(REGISTRY + service) + r"@sha256:[0-9a-f]{64}", ref):
            raise ValueError(f"invalid signed {service} image reference")
    pending = state.get("pending")
    if not pending or pending["version"] != target:
        pending = {"version": target, "since": int(time.time())}
        state["pending"] = pending
        write(STATUS, state, 0o644)
        print(f"Verified update {target} is available")
    if not ready(secret, pending["since"], hold_seconds, hour):
        return
    for ref in images.values():
        docker("pull", ref, timeout=900)
    if ready(secret, pending["since"], hold_seconds, hour):
        update(target, current, images, containers, keep)


def ready(secret, since, hold_seconds, hour):
    try:
        quiet = livekit_quiet(secret)
    except (OSError, ValueError) as error:
        print(f"LiveKit activity unknown: {error}", flush=True)
        quiet = False
    return due(quiet, datetime.now(timezone.utc), since, hold_seconds, hour)


def manual_restore(filename):
    if not re.fullmatch(r"rusttalk-[0-9]{8}T[0-9]{12}Z\.tar\.gz", filename):
        raise ValueError("expected a rusttalk-*.tar.gz archive name from /backups")
    if any(running(service) for service in DATA):
        raise RuntimeError("stop api and index before restoring")
    archive = BACKUPS / filename
    with tarfile.open(archive, "r:gz") as source:
        with source.extractfile("release.json") as metadata:
            release = json.load(metadata)
    if "api" not in release["images"]:
        raise ValueError("archive is missing API release metadata")
    restore(archive, release["images"])
    print(f"Restored {filename}. Previous RustTalk version: {release['version']}")
    for service, ref in release["images"].items():
        print(f"{service} image: {ref}")


def main():
    os.umask(0o077)
    with (BACKUPS / "updater.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if len(sys.argv) == 3 and sys.argv[1] == "restore":
            manual_restore(sys.argv[2])
        elif len(sys.argv) == 1:
            run()
        else:
            raise ValueError("usage: rusttalk-updater [restore ARCHIVE]")


def run():
    series = os.environ["RUSTTALK_VERSION"]
    version(series + ".0")
    key = os.environ["RUSTTALK_RELEASE_PUBLIC_KEY"]
    if not re.fullmatch(r"[0-9a-fA-F]{64}", key):
        raise ValueError("set RUSTTALK_RELEASE_PUBLIC_KEY to the trusted 64-digit Ed25519 key")
    hour = int(os.environ["RUSTTALK_UPDATE_HOUR_UTC"])
    hold = int(os.environ["RUSTTALK_UPDATE_HOLD_HOURS"]) * 3600
    keep = int(os.environ["RUSTTALK_UPDATE_KEEP"])
    if not 0 <= hour <= 23 or hold < 0 or keep < 1:
        raise ValueError("invalid update hour, hold or backup count")
    # Compose and the operator must both use the persistent pins as their last override.
    if not os.environ["COMPOSE_FILE"].endswith(":auto-update/images.yml") or not PINS.is_file():
        raise ValueError("add auto-update/images.yml as the final COMPOSE_FILE and initialize it")
    while True:
        try:
            check(series, key, os.environ["LIVEKIT_API_SECRET"], hour, hold, keep)
        except Exception as error:
            print(f"Update check failed: {error}", flush=True)
        time.sleep(300)


if __name__ == "__main__":
    main()

import datetime
import json
from contextlib import closing
from pathlib import Path
import sqlite3
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import update


class UpdateTests(unittest.TestCase):
    def test_selects_only_newer_patches_in_running_series(self):
        self.assertTrue(update.newer_patch("0.1.10", "0.1.9", "0.1"))
        self.assertFalse(update.newer_patch("0.1.9", "0.1.10", "0.1"))
        self.assertFalse(update.newer_patch("0.1.9", "0.1.9", "0.1"))
        self.assertFalse(update.newer_patch("0.2.0", "0.1.9", "0.1"))
        self.assertFalse(update.newer_patch("0.2.1", "0.1.9", "0.2"))
        for bad in ("0.1.01", "0.1.2-rc1", "0.1.65536", "0.1"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                update.newer_patch(bad, "0.1.1", "0.1")

    def test_verifies_exact_signed_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            key = Path(directory) / "key"
            path = key.with_name("latest.json")
            path.write_bytes(b'{"version": "0.1.10"}\n')
            subprocess.run(["openssl", "genpkey", "-algorithm", "Ed25519", "-out", key], check=True)
            public = subprocess.check_output(
                ["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER"]
            )[-32:].hex()
            signature = subprocess.check_output(
                ["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", path]
            )
            self.assertEqual(update.verify(path.read_bytes(), signature, public)["version"], "0.1.10")
            with self.assertRaisesRegex(ValueError, "verification failed"):
                update.verify(path.read_bytes().replace(b"10", b"11"), signature, public)
            with self.assertRaisesRegex(ValueError, "verification failed"):
                update.verify(path.read_bytes(), bytes(64), public)
            with self.assertRaisesRegex(ValueError, "verification failed"):
                update.verify(path.read_bytes(), signature, "00" * 32)

    def test_holds_until_quiet_or_expired_inside_window(self):
        now = datetime.datetime(2026, 1, 1, 4, 12, tzinfo=datetime.timezone.utc)
        seen = now.timestamp()
        self.assertTrue(update.due(True, now, seen, 21600, 5))
        self.assertFalse(update.due(False, now, seen - 21599, 21600, 4))
        self.assertFalse(update.due(False, now, seen - 21600, 21600, 5))
        self.assertTrue(update.due(False, now, seen - 21600, 21600, 4))

    def test_checkpoint_and_restore_entire_volume(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            data = root / "data"
            data.mkdir()
            db_path = data / update.DATABASE["api"]
            key = data / "server.key"
            key.write_bytes(b"original")
            key.chmod(0o600)
            archive = root / "old.tar.gz"
            with patch.object(update, "DATA", {"api": data}), closing(sqlite3.connect(db_path)) as db:
                db.execute("PRAGMA journal_mode=WAL")
                db.execute("CREATE TABLE example (value TEXT)")
                db.execute("INSERT INTO example VALUES ('before')")
                db.commit()
                images = {"api": "sha256:" + "a" * 64}
                update.backup(archive, "0.1.2", images)
                with tarfile.open(archive) as saved, saved.extractfile("release.json") as metadata:
                    self.assertEqual(json.load(metadata), {"version": "0.1.2", "images": images})
                self.assertEqual(db_path.with_suffix(".sqlite3-wal").stat().st_size, 0)
                db.execute("ALTER TABLE example ADD COLUMN failed_update TEXT")
                db.commit()
                db.close()
                key.write_bytes(b"new key")
                extra = data / "new-file"
                extra.touch()
                update.restore(archive, ["api"])
                self.assertEqual(key.read_bytes(), b"original")
                self.assertEqual(key.stat().st_mode & 0o777, 0o600)
                self.assertFalse(extra.exists())
                with sqlite3.connect(db_path) as previous:
                    self.assertEqual(previous.execute("SELECT * FROM example").fetchall(), [("before",)])


    def test_rollback_retries_keep_writes_made_after_restore(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal, status = root / "transaction.json", root / "status.json"
            transaction = {
                "target": "0.1.3", "version": "0.1.2", "old": {"api": "old"},
                "pins": {}, "archive": "previous.tar.gz", "phase": "installing",
            }
            update.write(journal, transaction)
            with (
                patch.object(update, "JOURNAL", journal), patch.object(update, "STATUS", status),
                patch.object(update, "BACKUPS", root), patch.object(update, "restore") as restore,
                patch.object(update, "set_pins"), patch.object(update, "compose"),
                patch.object(update, "healthy", side_effect=[RuntimeError("timeout"), None]),
            ):
                with self.assertRaisesRegex(RuntimeError, "timeout"):
                    update.finish(update.read(journal), 3)
                self.assertEqual(update.read(journal)["phase"], "restored")
                update.finish(update.read(journal), 3)
                restore.assert_called_once()
                self.assertFalse(journal.exists())
                self.assertEqual(update.read(status)["last"]["outcome"], "failed")


if __name__ == "__main__":
    unittest.main()

-- Reports (#678). The report itself (reason, note, the signed message and its key) is
-- sealed to the reviewers' devices; the server keeps the ciphertext and what it needs to
-- route, check and act: who reported which message in which channel, who sent it, and the
-- reported records as stored (device, epoch, envelope hash) so reviewers can check the
-- evidence is the message the server holds.
CREATE TABLE reports (
    id TEXT NOT NULL PRIMARY KEY,
    reporter_id BLOB NOT NULL,
    channel_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    message_device BLOB NOT NULL,
    message_epoch BIGINT NOT NULL,
    message_hash BLOB NOT NULL,
    -- The edit of the message that was shown, when it was edited.
    edit_id TEXT,
    edit_device BLOB,
    edit_epoch BIGINT,
    edit_hash BLOB,
    sender_id BLOB NOT NULL,
    -- From a DM: only the owner reviews it.
    dm BOOLEAN NOT NULL,
    body_ct BLOB NOT NULL,
    created_at BIGINT NOT NULL,
    resolved_at BIGINT,
    resolved_by BLOB,
    resolution TEXT
);
-- One open report per person and message; after it is closed they may report it again,
-- such as a later edit after a dismissal.
CREATE UNIQUE INDEX reports_open_once ON reports (reporter_id, message_id)
    WHERE resolved_at IS NULL;
CREATE INDEX reports_open ON reports (dm, resolved_at, created_at, id);
CREATE INDEX reports_reporter ON reports (reporter_id, resolved_at);

-- The report key, wrapped to each reviewer device when the report was sent.
CREATE TABLE report_keys (
    report_id TEXT NOT NULL REFERENCES reports(id) ON DELETE CASCADE,
    device_id BLOB NOT NULL,
    wrapped BLOB NOT NULL,
    PRIMARY KEY (report_id, device_id)
);

-- Stored files the reported message uses, which reviewers may download while it is open.
CREATE TABLE report_files (
    report_id TEXT NOT NULL REFERENCES reports(id) ON DELETE CASCADE,
    file_id TEXT NOT NULL,
    PRIMARY KEY (report_id, file_id)
);
CREATE INDEX report_files_file ON report_files (file_id);

-- The first 16 nonce bytes of every message and shared file per channel epoch. They pick
-- the record's XChaCha20 subkey, which a report discloses, so no two records of one epoch
-- may share them: then a disclosed key opens one record only. Kept after a record is
-- deleted (so its prefix is never reused), until the channel or its epoch goes: about
-- 60 bytes per record ever posted, like `message_ids`.
CREATE TABLE nonce_prefixes (
    channel_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    prefix BLOB NOT NULL,
    -- Two existing records already share it (never expected with random nonces): neither
    -- can be reported, as its key would open the other.
    reused BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (channel_id, epoch, prefix),
    FOREIGN KEY (channel_id, epoch) REFERENCES key_epochs(scope_id, epoch) ON DELETE CASCADE
) WITHOUT ROWID;
-- Existing records too, in key order so the build appends instead of seeking. The WHERE
-- is SQLite's required marker for an upsert from a SELECT.
INSERT INTO nonce_prefixes (channel_id, epoch, prefix)
    SELECT channel_id, epoch, substr(envelope, 2, 16) FROM messages WHERE TRUE
    ORDER BY 1, 2, 3
    ON CONFLICT (channel_id, epoch, prefix) DO UPDATE SET reused = TRUE;
INSERT INTO nonce_prefixes (channel_id, epoch, prefix)
    SELECT f.channel_id, s.epoch, substr(s.envelope, 2, 16)
    FROM shared_files s JOIN files f ON f.id = s.id
    WHERE EXISTS (SELECT 1 FROM key_epochs k WHERE k.scope_id = f.channel_id AND k.epoch = s.epoch)
    ORDER BY 1, 2, 3
    ON CONFLICT (channel_id, epoch, prefix) DO UPDATE SET reused = TRUE;

-- `review_reports` (2097152) for the built-in staff roles and custom roles that may both
-- ban and delete messages, the ones that already act on what members post.
UPDATE roles SET permissions = permissions | 2097152
WHERE id IN ('owner', 'admin', 'moderator') OR (kind = 'custom' AND (permissions & 36) = 36);

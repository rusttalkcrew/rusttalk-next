CREATE TABLE files (
    id TEXT NOT NULL PRIMARY KEY,
    channel_id TEXT NOT NULL,
    uploader BLOB NOT NULL,
    size BIGINT NOT NULL CHECK (size >= 16),
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL
);
CREATE INDEX files_uploader ON files (uploader);
CREATE INDEX files_expiry ON files (expires_at);
CREATE INDEX files_channel ON files (channel_id);

-- Keep rows and quota charges until the sweep has unlinked their blobs.
CREATE TRIGGER expire_channel_files AFTER DELETE ON channels
BEGIN
    UPDATE files SET expires_at = 0 WHERE channel_id = OLD.id;
END;

-- Older ciphertext and its paging sequence survive the larger signed envelope limit.
CREATE TABLE messages_with_attachments (
    sequence INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    channel_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    sender_device BLOB NOT NULL,
    sender_person BLOB NOT NULL,
    at BIGINT NOT NULL,
    size INTEGER NOT NULL CHECK (size = length(envelope) AND size BETWEEN 111 AND 10291),
    envelope BLOB NOT NULL,
    FOREIGN KEY (channel_id, epoch) REFERENCES key_epochs(scope_id, epoch) ON DELETE CASCADE,
    FOREIGN KEY (sender_device, sender_person) REFERENCES message_senders(device_id, person_id)
);
INSERT INTO messages_with_attachments
    (sequence, id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
    SELECT sequence, id, channel_id, epoch, sender_device, sender_person, at, size, envelope FROM messages;
DROP TABLE messages;
ALTER TABLE messages_with_attachments RENAME TO messages;
CREATE INDEX messages_channel_sequence ON messages (channel_id, sequence);
CREATE INDEX messages_channel_sender ON messages (channel_id, sender_device, sender_person);

-- Raise the envelope limit to 13,926 bytes, so a message can carry ten stored files plus a
-- live offer. Nothing references messages, so the table is rebuilt with its rows, sequences
-- and indexes.
CREATE TABLE messages_ten_files (
    sequence INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    channel_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    sender_device BLOB NOT NULL,
    sender_person BLOB NOT NULL,
    at BIGINT NOT NULL,
    size INTEGER NOT NULL CHECK (size = length(envelope) AND size BETWEEN 111 AND 13926),
    envelope BLOB NOT NULL,
    backfilled BOOLEAN NOT NULL DEFAULT FALSE,
    FOREIGN KEY (channel_id, epoch) REFERENCES key_epochs(scope_id, epoch) ON DELETE CASCADE,
    FOREIGN KEY (sender_device, sender_person) REFERENCES message_senders(device_id, person_id)
);
INSERT INTO messages_ten_files
    (sequence, id, channel_id, epoch, sender_device, sender_person, at, size, envelope, backfilled)
    SELECT sequence, id, channel_id, epoch, sender_device, sender_person, at, size, envelope, backfilled
    FROM messages;
DROP TABLE messages;
ALTER TABLE messages_ten_files RENAME TO messages;
CREATE INDEX messages_channel_sequence ON messages (channel_id, sequence);
CREATE INDEX messages_channel_sender ON messages (channel_id, sender_device, sender_person);

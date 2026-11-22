-- A stored file shared to its channel's file list. The envelope is a signed message, sealed
-- with the channel key under the file's ID, whose only content is the file's reference, so
-- the server never sees its name. Shared files are kept until removed instead of expiring.
CREATE TABLE shared_files (
    id TEXT NOT NULL PRIMARY KEY REFERENCES files (id) ON DELETE CASCADE,
    epoch BIGINT NOT NULL,
    sender_device BLOB NOT NULL,
    sender_person BLOB NOT NULL,
    at BIGINT NOT NULL,
    envelope BLOB NOT NULL,
    FOREIGN KEY (sender_device, sender_person) REFERENCES message_senders (device_id, person_id)
);

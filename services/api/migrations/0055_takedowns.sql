-- Operator takedowns (#679): what the owner removed for a legal reason, kept as a record.
-- The reason and notice reference are the owner's own words, in plain text; nothing a
-- member wrote is kept, only IDs, sizes and a SHA-256 of the removed ciphertext.
CREATE TABLE takedowns (
    id INTEGER PRIMARY KEY,
    at BIGINT NOT NULL,
    actor_person_id BLOB NOT NULL,
    reason TEXT NOT NULL,
    notice TEXT,
    -- The account banned in the same step, if any.
    banned_person_id BLOB
);

CREATE TABLE takedown_items (
    takedown_id INTEGER NOT NULL REFERENCES takedowns (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('message', 'file')),
    item_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    author_person_id BLOB NOT NULL,
    size BIGINT NOT NULL,
    -- NULL for a file whose ciphertext was already gone from disk.
    sha256 BLOB,
    PRIMARY KEY (takedown_id, item_id)
);

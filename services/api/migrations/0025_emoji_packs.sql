-- Emoji and sticker packs: named groups, shown as one picker section each. Names are space-key
-- ciphertext. The Default pack (the ID of 32 zero bytes) has an empty name: the server can't
-- encrypt one, so apps show their own "Default" until someone renames it. Packs and their
-- items are kept consistent in code, not by a foreign key, because SQLite can't add one with
-- a default.
CREATE TABLE emoji_packs (
    id TEXT NOT NULL PRIMARY KEY,
    name_ct BLOB NOT NULL,
    position BIGINT NOT NULL,
    created_at BIGINT NOT NULL
);
INSERT INTO emoji_packs (id, name_ct, position, created_at)
VALUES ('AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA', x'', 0, 0);
ALTER TABLE emoji ADD COLUMN pack_id TEXT NOT NULL DEFAULT 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA';

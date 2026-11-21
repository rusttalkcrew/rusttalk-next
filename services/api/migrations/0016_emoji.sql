-- Server emoji and stickers. Names and images are space-key ciphertext; images live on disk.
CREATE TABLE emoji (
    id TEXT NOT NULL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('emoji', 'sticker')),
    owner BLOB NOT NULL,
    size BIGINT NOT NULL,
    image_hash TEXT NOT NULL,
    name_ct BLOB NOT NULL,
    created_at BIGINT NOT NULL
);

UPDATE roles SET permissions = permissions | 512 WHERE id IN ('owner', 'admin');

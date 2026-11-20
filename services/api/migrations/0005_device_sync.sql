CREATE TABLE device_sync (
    person_id BLOB NOT NULL PRIMARY KEY REFERENCES members(person_id) ON DELETE CASCADE,
    version BIGINT NOT NULL CHECK (version > 0),
    ciphertext BLOB NOT NULL,
    size INTEGER NOT NULL CHECK (size = length(ciphertext) AND size BETWEEN 40 AND 262144),
    at BIGINT NOT NULL
);

CREATE TABLE listings (
    server_id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    address TEXT NOT NULL,
    member_count INTEGER NOT NULL,
    open_joining BOOLEAN NOT NULL,
    timestamp INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE INDEX listings_expiry ON listings (expires_at);

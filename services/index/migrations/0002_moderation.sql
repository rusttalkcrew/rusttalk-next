-- Reports from people browsing the directory. No reporter identity or address is kept.
CREATE TABLE reports (
    id INTEGER PRIMARY KEY,
    server_id TEXT NOT NULL,
    -- The listing as it was when reported, since it may change or expire.
    name TEXT NOT NULL,
    address TEXT NOT NULL,
    reason TEXT NOT NULL,
    note TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX reports_server ON reports (server_id);

-- Servers the operator removed. A delist refuses the server ID; a block also refuses
-- any server at the same host, so a new key or name does not get it back in.
CREATE TABLE sanctions (
    server_id TEXT PRIMARY KEY NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('delist', 'block')),
    host TEXT,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX sanctions_host ON sanctions (host);

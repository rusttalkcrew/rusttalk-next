-- Server events, off until someone with "Manage server" turns them on. 0032 and 0033 are
-- taken by open pull requests.
ALTER TABLE server_settings ADD COLUMN events_enabled BOOLEAN NOT NULL DEFAULT FALSE
    CHECK (events_enabled IN (0, 1));

CREATE TABLE calendar_events (
    id TEXT NOT NULL PRIMARY KEY REFERENCES channel_ids(id),
    author BLOB NOT NULL,
    author_device BLOB NOT NULL,
    certificate BLOB NOT NULL,
    approved BOOLEAN NOT NULL CHECK (approved IN (0, 1)),
    epoch BIGINT NOT NULL CHECK (epoch > 0),
    remove_at BIGINT NOT NULL,
    envelope BLOB NOT NULL
);
CREATE INDEX calendar_events_removal ON calendar_events (remove_at);

-- `suggest_events` (4096), `create_events` (8192) and `manage_events` (16384).
UPDATE roles SET permissions = permissions | 28672 WHERE id IN ('owner', 'admin', 'moderator');
UPDATE roles SET permissions = permissions | 4096 WHERE id = 'member';
-- Giving someone a role needs every permission in it. Custom roles that could give out
-- Member (manage_roles) or Moderator (also all it held) get the same, so they still can.
UPDATE roles SET permissions = permissions | 4096 WHERE kind = 'custom' AND (permissions & 8) <> 0;
UPDATE roles SET permissions = permissions | 28672 WHERE kind = 'custom' AND (permissions & 3118) = 3118;

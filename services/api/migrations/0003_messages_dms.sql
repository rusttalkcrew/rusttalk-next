-- Add private channels while retaining W06 channels, permissions, scopes and every epoch.
CREATE TEMP TABLE saved_channels AS SELECT * FROM channels;
CREATE TEMP TABLE saved_permissions AS SELECT * FROM channel_permissions;
CREATE TEMP TABLE saved_scopes AS SELECT * FROM key_scopes;
CREATE TEMP TABLE saved_epochs AS SELECT * FROM key_epochs;
CREATE TEMP TABLE saved_envelopes AS SELECT * FROM key_envelopes;
CREATE TEMP TABLE saved_rejections AS SELECT * FROM key_envelope_rejections;

DROP VIEW scope_readers;
DROP VIEW channel_readers;
DROP TABLE key_envelope_rejections;
DROP TABLE key_envelopes;
DROP TABLE key_epochs;
DROP TABLE key_scopes;
DROP TABLE channel_permissions;
DROP TABLE channels;

CREATE TABLE channels (
    id TEXT NOT NULL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('text', 'voice', 'dm')),
    category_id TEXT REFERENCES categories(id),
    position INTEGER NOT NULL,
    name_ct BLOB,
    topic_ct BLOB,
    dm_first BLOB REFERENCES members(person_id),
    dm_second BLOB REFERENCES members(person_id),
    UNIQUE (dm_first, dm_second),
    CHECK (
        (kind = 'dm' AND category_id IS NULL AND name_ct IS NULL AND topic_ct IS NULL
         AND dm_first IS NOT NULL AND dm_second IS NOT NULL AND dm_first < dm_second)
        OR
        (kind IN ('text', 'voice') AND category_id IS NOT NULL AND name_ct IS NOT NULL
         AND dm_first IS NULL AND dm_second IS NULL)
    )
);

CREATE TABLE channel_permissions (
    channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    flags INTEGER NOT NULL CHECK (flags BETWEEN 1 AND 15 AND (flags & 1) = 1),
    PRIMARY KEY (channel_id, role_id)
);

CREATE VIEW channel_readers AS
    SELECT p.channel_id, m.person_id
    FROM channel_permissions p JOIN member_roles m ON p.role_id = m.role_id
    JOIN channels c ON c.id = p.channel_id
    WHERE c.kind <> 'dm' AND (p.flags & 1) <> 0
    UNION
    SELECT c.id, s.owner_person_id
    FROM channels c CROSS JOIN server_settings s
    WHERE c.kind <> 'dm' AND s.owner_person_id IS NOT NULL
    UNION ALL
    SELECT id, dm_first FROM channels WHERE kind = 'dm'
    UNION ALL
    SELECT id, dm_second FROM channels WHERE kind = 'dm';

CREATE TABLE key_scopes (
    id TEXT NOT NULL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('space', 'channel')),
    channel_id TEXT UNIQUE REFERENCES channels(id) ON DELETE CASCADE,
    rotate BOOLEAN NOT NULL DEFAULT FALSE,
    CHECK ((kind = 'space' AND channel_id IS NULL) OR (kind = 'channel' AND channel_id = id))
);

CREATE VIEW scope_readers AS
    SELECT id AS scope_id, person_id FROM key_scopes CROSS JOIN members WHERE kind = 'space'
    UNION ALL
    SELECT k.id, r.person_id FROM key_scopes k JOIN channel_readers r ON k.channel_id = r.channel_id;

CREATE TABLE key_epochs (
    scope_id TEXT NOT NULL REFERENCES key_scopes(id) ON DELETE CASCADE,
    epoch BIGINT NOT NULL CHECK (epoch > 0),
    commitment BLOB NOT NULL CHECK (length(commitment) = 32),
    PRIMARY KEY (scope_id, epoch)
);

CREATE TABLE key_envelopes (
    scope_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    recipient_device BLOB NOT NULL REFERENCES member_devices(device_id) ON DELETE CASCADE,
    sender_device BLOB NOT NULL,
    envelope BLOB NOT NULL,
    PRIMARY KEY (scope_id, epoch, recipient_device),
    FOREIGN KEY (scope_id, epoch) REFERENCES key_epochs(scope_id, epoch) ON DELETE CASCADE
);
CREATE INDEX key_envelopes_recipient ON key_envelopes (recipient_device);

CREATE TABLE key_envelope_rejections (
    scope_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    recipient_device BLOB NOT NULL REFERENCES member_devices(device_id) ON DELETE CASCADE,
    sender_person BLOB NOT NULL REFERENCES members(person_id) ON DELETE CASCADE,
    PRIMARY KEY (scope_id, epoch, recipient_device, sender_person),
    FOREIGN KEY (scope_id, epoch) REFERENCES key_epochs(scope_id, epoch) ON DELETE CASCADE
);

INSERT INTO channels (id, kind, category_id, position, name_ct, topic_ct)
    SELECT id, kind, category_id, position, name_ct, topic_ct FROM saved_channels;
INSERT INTO channel_permissions SELECT * FROM saved_permissions;
INSERT INTO key_scopes SELECT * FROM saved_scopes;
INSERT INTO key_epochs SELECT * FROM saved_epochs;
INSERT INTO key_envelopes SELECT * FROM saved_envelopes;
INSERT INTO key_envelope_rejections SELECT * FROM saved_rejections;

DROP TABLE saved_channels;
DROP TABLE saved_permissions;
DROP TABLE saved_scopes;
DROP TABLE saved_epochs;
DROP TABLE saved_envelopes;
DROP TABLE saved_rejections;

CREATE TABLE message_senders (
    device_id BLOB NOT NULL,
    person_id BLOB NOT NULL,
    certificate BLOB NOT NULL,
    PRIMARY KEY (device_id, person_id)
);

-- Every message ID ever stored, so a deleted or pruned ID is never reused.
CREATE TABLE message_ids (id TEXT NOT NULL PRIMARY KEY);

CREATE TABLE messages (
    sequence INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    channel_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    sender_device BLOB NOT NULL,
    sender_person BLOB NOT NULL,
    at BIGINT NOT NULL,
    size INTEGER NOT NULL CHECK (size = length(envelope) AND size BETWEEN 111 AND 8175),
    envelope BLOB NOT NULL,
    FOREIGN KEY (channel_id, epoch) REFERENCES key_epochs(scope_id, epoch) ON DELETE CASCADE,
    FOREIGN KEY (sender_device, sender_person) REFERENCES message_senders(device_id, person_id)
);
CREATE INDEX messages_channel_sequence ON messages (channel_id, sequence);
CREATE INDEX messages_channel_sender ON messages (channel_id, sender_device, sender_person);

-- A closed DM's entries become visible to everyone with audit access.
ALTER TABLE audit_log ADD COLUMN dm_channel_id TEXT REFERENCES channels(id) ON DELETE SET NULL;

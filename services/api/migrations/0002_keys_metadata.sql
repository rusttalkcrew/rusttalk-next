UPDATE roles SET permissions = permissions | 64 WHERE id IN ('owner', 'admin');
-- W05 opaque role bytes did not have an epoch or a defined cipher format.
UPDATE roles SET name_ct = NULL WHERE name_ct IS NOT NULL;

ALTER TABLE server_settings ADD COLUMN name_ct BLOB;
ALTER TABLE server_settings ADD COLUMN description_ct BLOB;
ALTER TABLE server_settings ADD COLUMN icon_ct BLOB;
ALTER TABLE members ADD COLUMN display_name_ct BLOB;
ALTER TABLE members ADD COLUMN avatar_ct BLOB;
ALTER TABLE invites ADD COLUMN space_key_ct BLOB;
ALTER TABLE invites ADD COLUMN name_ct BLOB;

CREATE TABLE invite_previews (
    ip TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);

CREATE TABLE member_devices (
    device_id BLOB NOT NULL PRIMARY KEY,
    person_id BLOB NOT NULL REFERENCES members(person_id) ON DELETE CASCADE,
    certificate BLOB NOT NULL
);
CREATE INDEX member_devices_person ON member_devices (person_id);

CREATE TABLE categories (
    id TEXT NOT NULL PRIMARY KEY,
    position INTEGER NOT NULL,
    name_ct BLOB NOT NULL
);

CREATE TABLE channels (
    id TEXT NOT NULL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('text', 'voice')),
    category_id TEXT NOT NULL REFERENCES categories(id),
    position INTEGER NOT NULL,
    name_ct BLOB NOT NULL,
    topic_ct BLOB
);

CREATE TABLE channel_permissions (
    channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    flags INTEGER NOT NULL CHECK (flags BETWEEN 1 AND 15),
    PRIMARY KEY (channel_id, role_id)
);

CREATE VIEW channel_readers AS
    SELECT p.channel_id, m.person_id
    FROM channel_permissions p JOIN member_roles m ON p.role_id = m.role_id
    WHERE (p.flags & 1) <> 0
    UNION
    SELECT p.channel_id, s.owner_person_id
    FROM channel_permissions p CROSS JOIN server_settings s
    WHERE p.role_id = 'owner' AND (p.flags & 1) <> 0 AND s.owner_person_id IS NOT NULL;

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

-- Channel IDs are chosen before encryption and can only be used once.
CREATE TABLE channel_ids (id TEXT NOT NULL PRIMARY KEY);
